//! The tracker script (`/script.js`).
//!
//! `assets/script.js` is upstream's built tracker, copied byte-for-byte from the
//! v3.4.1 release image by `tools/tracker/fetch.py`. It posts to `/api/send`
//! relative to wherever the script itself is served from (or `data-host-url`), so
//! `BASE_PATH` needs no special handling.

use std::sync::Arc;

use axum::body::{
  Body,
  Bytes,
};
use axum::http::{
  HeaderMap,
  HeaderValue,
  StatusCode,
  header,
};
use axum::response::{
  IntoResponse,
  Response,
};
use sha2::{
  Digest,
  Sha256,
};

const SCRIPT: &str = include_str!("../assets/script.js");

/// SHA-256 of `assets/script.js`, so an accidental edit fails the tests.
#[cfg(test)]
const SCRIPT_SHA256: &str = "654f12c7548be726eba5b71d5ef8bffc2a41a39a0053e2a49a847869ccd7ac3f";

/// The script as served: built once at startup.
pub struct TrackerScript {
  body: Bytes,
  etag: HeaderValue,
}

impl TrackerScript {
  /// Like upstream's `update-tracker` step, points the script at
  /// `COLLECT_API_ENDPOINT` instead of `/api/send` when one is configured.
  pub fn new(collect_endpoint: Option<&str>) -> Arc<Self> {
    let body = match collect_endpoint {
      Some(endpoint) => Bytes::from(SCRIPT.replace("/api/send", endpoint)),
      None => Bytes::from_static(SCRIPT.as_bytes()),
    };

    let digest = hex::encode(Sha256::digest(&body));
    let etag = HeaderValue::from_str(&format!("\"{}\"", &digest[..32])).expect("hex is a valid header value");

    Arc::new(Self { body, etag })
  }

  /// Serves the script with upstream's production headers (`next.config.ts`
  /// `trackerHeaders`), answering conditional requests with 304.
  pub fn respond(&self, request_headers: &HeaderMap) -> Response {
    let fresh = request_headers
      .get(header::IF_NONE_MATCH)
      .and_then(|value| value.to_str().ok())
      .is_some_and(|tags| {
        tags
          .split(',')
          .map(|tag| tag.trim().trim_start_matches("W/"))
          .any(|tag| tag == "*" || tag.as_bytes() == self.etag.as_bytes())
      });

    let mut response = if fresh {
      StatusCode::NOT_MODIFIED.into_response()
    } else {
      Response::new(Body::from(self.body.clone()))
    };

    let headers = response.headers_mut();
    headers.insert(
      header::CONTENT_TYPE,
      HeaderValue::from_static("application/javascript; charset=utf-8"),
    );
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(
      header::CACHE_CONTROL,
      HeaderValue::from_static("public, max-age=86400, must-revalidate"),
    );
    headers.insert(header::ETAG, self.etag.clone());

    response
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn vendored_script_is_unmodified() {
    assert_eq!(hex::encode(Sha256::digest(SCRIPT.as_bytes())), SCRIPT_SHA256);
    assert_eq!(SCRIPT.matches("/api/send").count(), 1);
  }

  #[test]
  fn collect_endpoint_is_substituted() {
    let script = TrackerScript::new(Some("/collect"));

    let body = std::str::from_utf8(&script.body).unwrap();

    assert!(body.contains(r#"replace(/\/$/,"")}/collect`"#));
    assert!(!body.contains("/api/send"));
    assert_ne!(script.etag, TrackerScript::new(None).etag);
  }
}
