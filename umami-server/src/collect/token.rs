//! The `x-umami-cache` token: an HS256 JWT signed with the app secret, compatible
//! with upstream's `createToken` / `parseToken` (node `jsonwebtoken`).

use serde::{
  Deserialize,
  Serialize,
};

use crate::jwt;

const CACHE_TOKEN_TYPE: &str = "cache";

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheToken {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub website_id: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub session_id: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub visit_id: Option<String>,
  /// When the current visit started (Unix seconds).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub iat: Option<i64>,
  /// Set once an `identify` call has linked the session to a distinct ID.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub session_link_id: Option<String>,
  #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
  pub kind: Option<String>,
}

impl CacheToken {
  pub fn new(
    website_id: Option<String>,
    session_id: String,
    visit_id: String,
    iat: i64,
    session_link_id: Option<String>,
  ) -> Self {
    Self {
      website_id,
      session_id: Some(session_id),
      visit_id: Some(visit_id),
      iat: Some(iat),
      session_link_id,
      kind: Some(CACHE_TOKEN_TYPE.to_owned()),
    }
  }

  pub fn sign(&self, secret: &str) -> String {
    jwt::sign(self, secret)
  }

  /// Verifies a token and returns it if it is a cache token; `None` otherwise,
  /// like upstream (which ignores any invalid token).
  pub fn parse(token: &str, secret: &str) -> Option<Self> {
    jwt::verify::<Self>(token, secret).filter(|claims| claims.kind.as_deref() == Some(CACHE_TOKEN_TYPE))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const SECRET: &str = "36292cdada49fa7480ddf48a7fcd1d33b93bd7d4cd1e8f6da766e1af6dcc22b959eccc78982c0c3d5d71d6c095acaf779a77cb01753bcbf260c66ce52423320f";

  // Signed by upstream's `createToken` (node jsonwebtoken 9) with SECRET.
  const UPSTREAM_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJ3ZWJzaXRlSWQiOiIwZDhlM2E1Yy0xZjJiLTRjM2QtOWU0Zi01YTZiN2M4ZDllMGYiLCJzZXNzaW9uSWQiOiJlZjk4YTA2Ni05NjdiLTVhNWYtYWE0My0zYzE0OTJmNWQzNGUiLCJ2aXNpdElkIjoiNDM1N2RhMzktZWZiZC01NjQ3LWEyMGMtODg5NmE1Mzc5MDMwIiwiaWF0IjoxNzYyNDM2NzMwLCJ0eXBlIjoiY2FjaGUifQ.VntGWR7mENKw9xMhVy-kYCQRjaqfYepAaGgV0YzjN4U";
  const UPSTREAM_TOKEN_WRONG_SECRET: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJ3ZWJzaXRlSWQiOiIwZDhlM2E1Yy0xZjJiLTRjM2QtOWU0Zi01YTZiN2M4ZDllMGYiLCJzZXNzaW9uSWQiOiJlZjk4YTA2Ni05NjdiLTVhNWYtYWE0My0zYzE0OTJmNWQzNGUiLCJ2aXNpdElkIjoiNDM1N2RhMzktZWZiZC01NjQ3LWEyMGMtODg5NmE1Mzc5MDMwIiwiaWF0IjoxNzYyNDM2NzMwLCJ0eXBlIjoiY2FjaGUifQ.NdRBLl5ThIyu7EJLTqehcEDRHehKWUJWcr7FhYQP7tU";

  #[test]
  fn reads_upstream_tokens() {
    let token = CacheToken::parse(UPSTREAM_TOKEN, SECRET).unwrap();

    assert_eq!(
      token,
      CacheToken::new(
        Some("0d8e3a5c-1f2b-4c3d-9e4f-5a6b7c8d9e0f".into()),
        "ef98a066-967b-5a5f-aa43-3c1492f5d34e".into(),
        "4357da39-efbd-5647-a20c-8896a5379030".into(),
        1762436730,
        None,
      )
    );
    assert_eq!(CacheToken::parse(UPSTREAM_TOKEN_WRONG_SECRET, SECRET), None);
    assert_eq!(CacheToken::parse("garbage", SECRET), None);
  }

  #[test]
  fn round_trips_with_upstream_claims_encoding() {
    let token = CacheToken::parse(UPSTREAM_TOKEN, SECRET).unwrap();
    let signed = token.sign(SECRET);

    // Same claim order and omission of unset fields as upstream; only the header's
    // key order differs, which the signature covers either way.
    assert_eq!(signed.split('.').nth(1), UPSTREAM_TOKEN.split('.').nth(1));
    assert_eq!(CacheToken::parse(&signed, SECRET), Some(token));
  }

  #[test]
  fn ignores_other_token_types() {
    let other = CacheToken {
      kind: Some("partial-auth".into()),
      ..CacheToken::default()
    };

    assert_eq!(CacheToken::parse(&other.sign(SECRET), SECRET), None);
  }
}
