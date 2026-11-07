//! The dashboard: upstream's Next.js UI as a static export, built from upstream's
//! source by `tools/dashboard` and served from `DASHBOARD_DIR`.
//!
//! The export is one app shell (`index.html`) whose client-side router renders
//! every upstream page. On navigation, Next's router fetches page data
//! (`<path>.txt`) and segment prefetches (`<path>/__next.<segment>.txt`), which the
//! shell's own files answer for every dashboard path, so navigation stays
//! client-side. `routes.json`, written by `tools/dashboard/prepare.mts`, lists
//! upstream's pages and the redirects and rewrites from its `next.config.ts`.

use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::{
  Context,
  bail,
};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{
  HeaderName,
  HeaderValue,
  Method,
  StatusCode,
  header,
};
use axum::response::{
  IntoResponse,
  Response,
};
use percent_encoding::percent_decode_str;
use regex::{
  Captures,
  Regex,
  RegexSet,
};
use serde::Deserialize;
use tower_http::services::ServeFile;

use crate::config::{
  Config,
  normalize_base_path,
};
use crate::error::AppError;

/// The route manifest written by `tools/dashboard/prepare.mts`. Not served.
const MANIFEST: &str = "routes.json";

/// Next's `Cache-Control` for hashed build assets (`/_next/static`).
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// Next's `Cache-Control` for files from `public/`.
const PUBLIC: &str = "public, max-age=0";
/// Next's `Cache-Control` for dynamically rendered pages, which upstream's all are.
const DYNAMIC: &str = "private, no-cache, no-store, max-age=0, must-revalidate";

/// What a request is answered with.
enum Served {
  /// A file from the build, as upstream serves `public/` and `/_next/static`.
  Asset,
  /// The app shell, for a page.
  Page,
  /// The shell's page data, for Next's navigation requests.
  PageData,
}

/// `:name` or `:name*` in a redirect or rewrite destination.
static DESTINATION_PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":(\w+)\*?").unwrap());

/// A loaded dashboard build.
pub struct Dashboard {
  root: PathBuf,
  pages: RegexSet,
  redirects: Vec<Rule>,
  rewrites: Vec<Rule>,
  content_security_policy: HeaderValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
  base_path: String,
  routes: Vec<String>,
  redirects: Vec<RuleSpec>,
  rewrites: Vec<RuleSpec>,
}

#[derive(Deserialize)]
struct RuleSpec {
  regex: String,
  names: Vec<String>,
  destination: String,
}

/// A redirect or rewrite from upstream's `next.config.ts`, its path-to-regexp
/// source already compiled to a regex by `prepare.mts`.
struct Rule {
  regex: Regex,
  names: Vec<String>,
  destination: String,
}

impl Rule {
  fn compile(spec: RuleSpec) -> anyhow::Result<Self> {
    Ok(Self {
      regex: Regex::new(&spec.regex).with_context(|| format!("invalid rule {:?}", spec.regex))?,
      names: spec.names,
      destination: spec.destination,
    })
  }

  /// The destination for `path` with its parameters filled in, if `path` matches.
  fn apply(&self, path: &str) -> Option<String> {
    let captures = self.regex.captures(path)?;
    let destination = DESTINATION_PARAM.replace_all(&self.destination, |param: &Captures| {
      self
        .names
        .iter()
        .position(|name| name == &param[1])
        .and_then(|index| captures.get(index + 1))
        .map_or("", |value| value.as_str())
        .to_owned()
    });

    Some(destination.into_owned())
  }
}

impl Dashboard {
  /// Loads the build in `config.dashboard.dir`, which must have been built for
  /// the configured `BASE_PATH` (Next bakes it into every asset URL).
  pub fn load(config: &Config) -> anyhow::Result<Self> {
    let root = config.dashboard.dir.clone();
    let manifest = std::fs::read(root.join(MANIFEST)).with_context(|| format!("no {MANIFEST}"))?;
    let manifest: Manifest = serde_json::from_slice(&manifest).with_context(|| format!("invalid {MANIFEST}"))?;

    let built_for = normalize_base_path(&manifest.base_path);
    if built_for != config.base_path {
      bail!(
        "built for BASE_PATH {built_for:?} but BASE_PATH is {:?}; rebuild it (see tools/dashboard)",
        config.base_path
      );
    }
    for file in ["index.html", "index.txt", "404.html"] {
      if !root.join(file).is_file() {
        bail!("no {file}");
      }
    }

    Ok(Self {
      pages: RegexSet::new(&manifest.routes).context("invalid page route")?,
      redirects: manifest
        .redirects
        .into_iter()
        .map(Rule::compile)
        .collect::<anyhow::Result<_>>()?,
      rewrites: manifest
        .rewrites
        .into_iter()
        .map(Rule::compile)
        .collect::<anyhow::Result<_>>()?,
      content_security_policy: HeaderValue::from_str(&content_security_policy(
        config.dashboard.allowed_frame_urls.as_deref(),
      ))
      .context("invalid ALLOWED_FRAME_URLS")?,
      root,
    })
  }

  /// Whether the dashboard answers `request`: page and asset reads, not the API.
  pub fn handles(request: &Request) -> bool {
    matches!(*request.method(), Method::GET | Method::HEAD) && !request.uri().path().starts_with("/api/")
  }

  /// Serves a request routed relative to `BASE_PATH`, with the headers upstream's
  /// `next.config.ts` and Docker proxy add to every response but redirects.
  pub async fn serve(&self, config: &Config, request: Request) -> Response {
    let mut response = self.respond(config, request).await;

    if !response.status().is_redirection() {
      let headers = response.headers_mut();
      headers.insert(header::CONTENT_SECURITY_POLICY, self.content_security_policy.clone());
      headers.insert(
        HeaderName::from_static("x-dns-prefetch-control"),
        HeaderValue::from_static("on"),
      );
    }
    response
  }

  async fn respond(&self, config: &Config, request: Request) -> Response {
    let path = request.uri().path();
    let query = request
      .uri()
      .query()
      .map(|query| format!("?{query}"))
      .unwrap_or_default();

    // Upstream's Docker proxy.
    if config.dashboard.disable_login && path == "/login" {
      return (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/plain;charset=UTF-8")],
        "Access denied",
      )
        .into_response();
    }

    // Next drops trailing slashes (`trailingSlash: false`) before anything else.
    if path.len() > 1 && path.ends_with('/') {
      let trimmed = path.trim_end_matches('/');
      let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
      return redirect(StatusCode::PERMANENT_REDIRECT, &config.base_path, trimmed, &query);
    }

    if let Some(destination) = self.redirects.iter().find_map(|rule| rule.apply(path)) {
      return redirect(StatusCode::TEMPORARY_REDIRECT, &config.base_path, &destination, &query);
    }

    let (file, served) = if let Some(file) = self.page_data(path) {
      (file, Served::PageData)
    } else if let Some(file) = self.asset(path).await {
      (file, Served::Asset)
    } else if self.is_page(path) {
      (self.root.join("index.html"), Served::Page)
    } else {
      return self.not_found().await;
    };

    // Next's headers: pages are rendered per request, `/_next/static` is
    // content-hashed, and `send` gives text types a charset.
    let (cache, content_type) = match served {
      Served::Page => (DYNAMIC, Some("text/html; charset=utf-8")),
      Served::PageData => (DYNAMIC, Some("text/plain; charset=UTF-8")),
      Served::Asset if path.starts_with("/_next/static/") => (IMMUTABLE, text_content_type(path)),
      Served::Asset => (PUBLIC, text_content_type(path)),
    };

    self.serve_file(file, cache, content_type, request).await
  }

  /// Whether `path` is a dashboard page, after upstream's rewrites
  /// (`/teams/{teamId}/websites` renders `/websites`).
  fn is_page(&self, path: &str) -> bool {
    let rewritten = self.rewrites.iter().find_map(|rule| rule.apply(path));
    self.pages.is_match(rewritten.as_deref().unwrap_or(path))
  }

  /// A file in the build, by URL path.
  async fn asset(&self, path: &str) -> Option<PathBuf> {
    // The manifest and the precompressed copies are not files Next would serve.
    if path == format!("/{MANIFEST}") || path.ends_with(".gz") || path.ends_with(".br") {
      return None;
    }
    let file = self.resolve(path)?;
    let metadata = tokio::fs::metadata(&file).await.ok()?;

    metadata.is_file().then_some(file)
  }

  /// The shell's page data or segment prefetch, for a dashboard page's
  /// `<page>.txt` or `<page>/__next.<segment>.txt`. Every page renders the
  /// same shell, so its data is the same for all of them.
  fn page_data(&self, path: &str) -> Option<PathBuf> {
    let page = path.strip_suffix(".txt")?;
    let (dir, name) = path.rsplit_once('/')?;

    if name.starts_with("__next.") {
      let dir = if dir.is_empty() { "/" } else { dir };
      return if self.is_page(dir) {
        self.resolve(&format!("/{name}"))
      } else {
        None
      };
    }

    // `/index.txt` is the root page's.
    let page = if page == "/index" { "/" } else { page };
    self.is_page(page).then(|| self.root.join("index.txt"))
  }

  /// Maps a URL path into the build directory. Segments are percent-decoded and
  /// limited to the characters Next's file names use, so nothing outside the
  /// directory (or hidden in it) can be named.
  fn resolve(&self, path: &str) -> Option<PathBuf> {
    let mut file = self.root.clone();

    for segment in path.strip_prefix('/')?.split('/') {
      let segment = percent_decode_str(segment).decode_utf8().ok()?;
      let safe = !segment.is_empty()
        && !segment.starts_with('.')
        && segment
          .chars()
          .all(|c| c.is_ascii_alphanumeric() || "-_.$~@+".contains(c));

      if !safe {
        return None;
      }
      file.push(segment.as_ref());
    }

    Some(file)
  }

  /// Serves a file with conditional, range and precompressed (`.br`, `.gz`)
  /// variants handled by `ServeFile`.
  async fn serve_file(
    &self,
    file: PathBuf,
    cache: &'static str,
    content_type: Option<&'static str>,
    request: Request,
  ) -> Response {
    let result = ServeFile::new(&file)
      .precompressed_br()
      .precompressed_gzip()
      .try_call(request)
      .await;

    match result {
      Ok(response) if response.status() == StatusCode::NOT_FOUND => self.not_found().await,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.not_found().await,
      Ok(response) => {
        let mut response = response.map(Body::new);
        let headers = response.headers_mut();

        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        if let Some(content_type) = content_type
          && headers.contains_key(header::CONTENT_TYPE)
        {
          headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        }
        response
      }
      Err(error) => {
        AppError::Internal(anyhow::Error::new(error).context(format!("unable to read {}", file.display())))
          .into_response()
      }
    }
  }

  /// Next's 404 page, with a 404 status.
  async fn not_found(&self) -> Response {
    match tokio::fs::read(self.root.join("404.html")).await {
      Ok(body) => {
        (
          StatusCode::NOT_FOUND,
          [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, DYNAMIC),
          ],
          body,
        )
          .into_response()
      }
      Err(_) => AppError::NotFound.into_response(),
    }
  }
}

/// The `Content-Type` Next's static file server (`send`) gives text files, which
/// carries a charset unlike `ServeFile`'s.
fn text_content_type(path: &str) -> Option<&'static str> {
  let (_, extension) = path.rsplit_once('.')?;

  match extension {
    "js" => Some("application/javascript; charset=UTF-8"),
    "json" => Some("application/json; charset=UTF-8"),
    "css" => Some("text/css; charset=UTF-8"),
    "html" => Some("text/html; charset=UTF-8"),
    "txt" => Some("text/plain; charset=UTF-8"),
    _ => None,
  }
}

/// A redirect to `path` under `BASE_PATH`, keeping the query string, as Next does.
fn redirect(status: StatusCode, base_path: &str, path: &str, query: &str) -> Response {
  let location = format!("{base_path}{path}{query}");

  match HeaderValue::from_str(&location) {
    Ok(location) => (status, [(header::LOCATION, location)]).into_response(),
    Err(_) => AppError::NotFound.into_response(),
  }
}

/// Upstream's `getContentSecurityPolicy` (`src/lib/csp.ts`). `API_URL` is a
/// build-time setting upstream that umami-rs doesn't support, so `connect-src`
/// never names another origin.
fn content_security_policy(allowed_frame_urls: Option<&str>) -> String {
  let frame_ancestors = ["'self'", allowed_frame_urls.unwrap_or("")]
    .into_iter()
    .filter(|source| !source.is_empty())
    .collect::<Vec<_>>()
    .join(" ");
  let directives = [
    "default-src 'self'".to_owned(),
    "img-src 'self' https: data: blob:".to_owned(),
    "script-src 'self' 'unsafe-eval' 'unsafe-inline'".to_owned(),
    "style-src 'self' 'unsafe-inline'".to_owned(),
    "connect-src 'self' https:".to_owned(),
    "frame-src 'self' http: https:".to_owned(),
    format!("frame-ancestors {frame_ancestors}"),
  ];

  format!("{};", directives.join("; "))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rule(regex: &str, names: &[&str], destination: &str) -> Rule {
    Rule::compile(RuleSpec {
      regex: regex.to_owned(),
      names: names.iter().map(|name| (*name).to_owned()).collect(),
      destination: destination.to_owned(),
    })
    .unwrap()
  }

  #[test]
  fn rules_fill_destination_params() {
    let redirect = rule("^/teams/([^/]+)$", &["id"], "/teams/:id/websites");
    let rewrite = rule("^/teams/([^/]+)(?:/(.*))?$", &["teamId", "path"], "/:path*");

    assert_eq!(redirect.apply("/teams/abc").as_deref(), Some("/teams/abc/websites"));
    assert_eq!(redirect.apply("/teams/abc/x"), None);
    assert_eq!(rewrite.apply("/teams/abc/websites/1").as_deref(), Some("/websites/1"));
    assert_eq!(rewrite.apply("/teams/abc").as_deref(), Some("/"));
  }

  /// The Dockerfile builds the dashboard from the pinned upstream release.
  #[test]
  fn dockerfile_builds_the_pinned_version() {
    let dockerfile = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../Dockerfile"));
    let version = dockerfile
      .lines()
      .find_map(|line| line.strip_prefix("ARG UMAMI_VERSION="))
      .map(|value| value.trim_matches('"'));

    assert_eq!(version, Some(umami_db::UMAMI_VERSION));
  }

  #[test]
  fn csp_matches_upstream() {
    assert_eq!(
      content_security_policy(None),
      "default-src 'self'; img-src 'self' https: data: blob:; script-src 'self' 'unsafe-eval' 'unsafe-inline'; \
       style-src 'self' 'unsafe-inline'; connect-src 'self' https:; frame-src 'self' http: https:; frame-ancestors 'self';"
    );
    assert!(content_security_policy(Some("https://a.example")).ends_with("frame-ancestors 'self' https://a.example;"));
  }
}
