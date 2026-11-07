//! Serving the static dashboard build (`dashboard.rs`), against a miniature build
//! that uses the real route manifest from `tools/dashboard`.

use std::io::Write;
use std::path::{
  Path,
  PathBuf,
};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{
  HeaderMap,
  Request,
  StatusCode,
};
use http_body_util::BodyExt;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use umami_server::app;
use umami_server::config::Config;
use umami_server::dashboard::Dashboard;
use umami_server::state::AppState;

const WEBSITE: &str = "0d8e3a5c-1f2b-4c3d-9e4f-5a6b7c8d9e0f";
const SHELL: &str = "<!DOCTYPE html><title>shell</title>";
const NOT_FOUND: &str = "<!DOCTYPE html><title>404</title>";
const PAGE_DATA: &str = "0:page data";
const TREE: &str = "0:tree";
const CHUNK: &str = "console.log('chunk')";

/// A dashboard build in a fresh temporary directory, built for `base_path`.
fn build(base_path: &str) -> PathBuf {
  let dir = std::env::temp_dir().join(format!("umami-dashboard-{}", uuid::Uuid::new_v4()));
  let manifest =
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dashboard-routes.json"))
      .unwrap();
  let mut manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
  manifest["basePath"] = base_path.into();
  let manifest = manifest.to_string();
  let chunk_gz = gzip(CHUNK);

  let files: [(&str, &[u8]); 9] = [
    ("routes.json", manifest.as_bytes()),
    ("index.html", SHELL.as_bytes()),
    ("index.txt", PAGE_DATA.as_bytes()),
    ("404.html", NOT_FOUND.as_bytes()),
    ("__next._tree.txt", TREE.as_bytes()),
    ("favicon.ico", b"icon"),
    ("_next/static/chunks/0abc.js", CHUNK.as_bytes()),
    ("_next/static/chunks/0abc.js.gz", &chunk_gz),
    (".hidden", b"secret"),
  ];
  for (name, contents) in files {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
  }

  dir
}

fn gzip(text: &str) -> Vec<u8> {
  let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
  encoder.write_all(text.as_bytes()).unwrap();
  encoder.finish().unwrap()
}

fn config(vars: &[(&str, &str)]) -> Config {
  Config::from_lookup(|key| {
    match key {
      "DATABASE_URL" => Some("postgresql://umami@127.0.0.1:1/umami".to_owned()),
      _ => vars.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_owned()),
    }
  })
  .unwrap()
}

/// State serving a build made for the configured `BASE_PATH`.
fn state_with(vars: &[(&str, &str)]) -> AppState {
  let base_path = vars.iter().find(|(k, _)| *k == "BASE_PATH").map_or("", |(_, v)| v);
  let dir = build(base_path);
  let mut vars = vars.to_vec();
  let dir_var = dir.to_str().unwrap().to_owned();
  vars.push(("DASHBOARD_DIR", &dir_var));
  let config = config(&vars);
  let dashboard = Dashboard::load(&config).unwrap();

  AppState {
    db: PgPoolOptions::new().connect_lazy_with(umami_db::connect_options(&config.database_url).unwrap()),
    config: Arc::new(config),
    geo: None,
    dashboard: Some(Arc::new(dashboard)),
  }
}

fn state() -> AppState {
  state_with(&[])
}

async fn send(state: AppState, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
  let response = app(state).oneshot(request).await.unwrap();
  let status = response.status();
  let headers = response.headers().clone();
  let bytes = response.into_body().collect().await.unwrap().to_bytes();

  (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get(state: AppState, uri: &str) -> (StatusCode, HeaderMap, String) {
  send(state, Request::get(uri).body(Body::empty()).unwrap()).await
}

#[tokio::test]
async fn pages_get_the_shell() {
  for uri in [
    "/",
    "/websites",
    &format!("/websites/{WEBSITE}"),
    &format!("/websites/{WEBSITE}/realtime"),
    &format!("/teams/{WEBSITE}/websites/{WEBSITE}/events"),
    "/share/abc123",
    "/share/abc123/events/x",
    "/settings/preferences?x=1",
    "/login",
  ] {
    let (status, headers, body) = get(state(), uri).await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, SHELL), "{uri}");
    assert_eq!(headers["content-type"], "text/html; charset=utf-8", "{uri}");
    assert_eq!(
      headers["cache-control"], "private, no-cache, no-store, max-age=0, must-revalidate",
      "{uri}"
    );
  }
}

#[tokio::test]
async fn unknown_pages_get_the_404_page() {
  for uri in [
    "/nope",
    "/websites/a/b/c",
    "/routes.json",
    "/_next/static/chunks/0abc.js.gz",
    "/.hidden",
    "/%2e%2e/secret",
    "/_next/static/../../index.html",
  ] {
    let (status, headers, body) = get(state(), uri).await;

    assert_eq!((status, body.as_str()), (StatusCode::NOT_FOUND, NOT_FOUND), "{uri}");
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(
      headers["cache-control"],
      "private, no-cache, no-store, max-age=0, must-revalidate"
    );
  }
}

#[tokio::test]
async fn api_paths_and_other_methods_keep_the_json_404() {
  let (status, headers, body) = get(state(), "/api/nope").await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  assert_eq!(headers["content-type"], "application/json");
  assert!(body.contains("not-found"));

  let (status, _, body) = send(state(), Request::post("/websites").body(Body::empty()).unwrap()).await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  assert!(body.contains("not-found"));
}

#[tokio::test]
async fn next_config_redirects() {
  for (uri, location) in [
    ("/settings", "/settings/preferences"),
    ("/admin?x=1", "/admin/users?x=1"),
    ("/teams/t1", "/teams/t1/websites"),
    ("/teams/t1/settings", "/teams/t1/settings/preferences"),
    ("/teams/t1/dashboard", "/dashboard"),
  ] {
    let (status, headers, _) = get(state(), uri).await;

    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT, "{uri}");
    assert_eq!(headers["location"], location, "{uri}");
  }

  let (status, headers, _) = get(state(), "/websites/?a=b").await;
  assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
  assert_eq!(headers["location"], "/websites?a=b");
}

/// Next's router fetches page data and segment prefetches for the page it
/// navigates to; the shell's files answer for every page.
#[tokio::test]
async fn page_data_is_the_shells() {
  for (uri, expected) in [
    (format!("/websites/{WEBSITE}.txt?_rsc=x"), PAGE_DATA),
    ("/teams/t1/websites.txt".to_owned(), PAGE_DATA),
    ("/index.txt".to_owned(), PAGE_DATA),
    (format!("/websites/{WEBSITE}/__next._tree.txt"), TREE),
    ("/__next._tree.txt".to_owned(), TREE),
  ] {
    let (status, headers, body) = get(state(), &uri).await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, expected), "{uri}");
    assert_eq!(headers["content-type"], "text/plain; charset=UTF-8", "{uri}");
    assert_eq!(
      headers["cache-control"], "private, no-cache, no-store, max-age=0, must-revalidate",
      "{uri}"
    );
  }

  for uri in ["/nope.txt", "/nope/__next._tree.txt", "/websites/__next.missing.txt"] {
    assert_eq!(get(state(), uri).await.0, StatusCode::NOT_FOUND, "{uri}");
  }
}

#[tokio::test]
async fn files_are_served_with_next_caching() {
  let (status, headers, body) = get(state(), "/_next/static/chunks/0abc.js").await;
  assert_eq!((status, body.as_str()), (StatusCode::OK, CHUNK));
  assert_eq!(headers["content-type"], "application/javascript; charset=UTF-8");
  assert_eq!(headers["cache-control"], "public, max-age=31536000, immutable");

  let (status, headers, body) = get(state(), "/favicon.ico").await;
  assert_eq!((status, body.as_str()), (StatusCode::OK, "icon"));
  assert_eq!(headers["cache-control"], "public, max-age=0");
}

#[tokio::test]
async fn precompressed_files_are_served_when_accepted() {
  let request = Request::get("/_next/static/chunks/0abc.js")
    .header("accept-encoding", "gzip")
    .body(Body::empty())
    .unwrap();
  let response = app(state()).oneshot(request).await.unwrap();

  assert_eq!(response.headers()["content-encoding"], "gzip");
  let bytes = response.into_body().collect().await.unwrap().to_bytes();
  assert_eq!(bytes.as_ref(), gzip(CHUNK));
}

#[tokio::test]
async fn conditional_requests_revalidate() {
  let state = state();
  let (_, headers, _) = get(state.clone(), "/favicon.ico").await;
  let request = Request::get("/favicon.ico")
    .header("if-modified-since", headers["last-modified"].clone())
    .body(Body::empty())
    .unwrap();

  assert_eq!(send(state, request).await.0, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn responses_carry_upstream_security_headers() {
  for uri in ["/websites", "/nope", "/favicon.ico"] {
    let (_, headers, _) = get(state(), uri).await;

    assert_eq!(headers["x-dns-prefetch-control"], "on", "{uri}");
    assert!(
      headers["content-security-policy"]
        .to_str()
        .unwrap()
        .ends_with("frame-ancestors 'self';"),
      "{uri}"
    );
  }

  let (_, headers, _) = get(state_with(&[("ALLOWED_FRAME_URLS", "https://a.example")]), "/").await;
  assert!(
    headers["content-security-policy"]
      .to_str()
      .unwrap()
      .ends_with("frame-ancestors 'self' https://a.example;")
  );

  // Upstream's redirects carry neither.
  let (_, headers, _) = get(state(), "/settings").await;
  assert!(!headers.contains_key("content-security-policy"));
  assert!(!headers.contains_key("x-dns-prefetch-control"));
}

#[tokio::test]
async fn disable_login_forbids_the_login_page() {
  let (status, headers, body) = get(state_with(&[("DISABLE_LOGIN", "1")]), "/login").await;

  assert_eq!((status, body.as_str()), (StatusCode::FORBIDDEN, "Access denied"));
  assert_eq!(headers["content-type"], "text/plain;charset=UTF-8");
}

#[tokio::test]
async fn served_under_base_path() {
  let state = || state_with(&[("BASE_PATH", "/analytics")]);

  assert_eq!(get(state(), "/analytics/websites").await.2, SHELL);
  assert_eq!(get(state(), "/analytics").await.2, SHELL);
  assert_eq!(get(state(), "/analytics/_next/static/chunks/0abc.js").await.2, CHUNK);
  assert_eq!(get(state(), "/websites").await.0, StatusCode::NOT_FOUND);

  let (status, headers, _) = get(state(), "/analytics/settings").await;
  assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
  assert_eq!(headers["location"], "/analytics/settings/preferences");
}

#[test]
fn load_checks_the_build() {
  let dir = build("/other");
  let dir = dir.to_str().unwrap();
  let error = Dashboard::load(&config(&[("DASHBOARD_DIR", dir)])).err().unwrap();
  assert!(error.to_string().contains("built for BASE_PATH \"/other\""), "{error}");
  assert!(Dashboard::load(&config(&[("DASHBOARD_DIR", dir), ("BASE_PATH", "/other/")])).is_ok());

  let empty = std::env::temp_dir().join(format!("umami-dashboard-{}", uuid::Uuid::new_v4()));
  std::fs::create_dir_all(&empty).unwrap();
  assert!(Dashboard::load(&config(&[("DASHBOARD_DIR", empty.to_str().unwrap())])).is_err());
}
