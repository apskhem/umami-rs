use std::sync::Arc;

use axum::body::Body;
use axum::http::{
  HeaderMap,
  Request,
  StatusCode,
};
use http_body_util::BodyExt;
use serde_json::{
  Value,
  json,
};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use umami_server::app;
use umami_server::collect::CacheToken;
use umami_server::config::Config;
use umami_server::state::AppState;

const WEBSITE: &str = "0d8e3a5c-1f2b-4c3d-9e4f-5a6b7c8d9e0f";
const BROWSER: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// State with a pool that never connects, for requests that don't reach the database.
fn state_with(vars: &[(&str, &str)]) -> AppState {
  let config = Config::from_lookup(|key| {
    match key {
      "DATABASE_URL" => Some("postgresql://umami@127.0.0.1:1/umami".to_owned()),
      _ => vars.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_owned()),
    }
  })
  .unwrap();

  let db = PgPoolOptions::new()
    .acquire_timeout(std::time::Duration::from_millis(200))
    .connect_lazy_with(umami_db::connect_options(&config.database_url).unwrap());

  AppState {
    db,
    config: Arc::new(config),
    geo: None,
  }
}

fn state(base_path: &str) -> AppState {
  state_with(&[("BASE_PATH", base_path)])
}

/// A cache token for WEBSITE, which lets `/api/send` skip the website lookup.
fn cache_token(state: &AppState) -> String {
  CacheToken::new(
    Some(WEBSITE.to_owned()),
    "ef98a066-967b-5a5f-aa43-3c1492f5d34e".to_owned(),
    "4357da39-efbd-5647-a20c-8896a5379030".to_owned(),
    chrono::Utc::now().timestamp(),
    None,
  )
  .sign(&state.config.secret)
}

async fn request(state: AppState, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
  let response = app(state).oneshot(request).await.unwrap();
  let status = response.status();
  let headers = response.headers().clone();
  let bytes = response.into_body().collect().await.unwrap().to_bytes();
  // JSON responses are parsed; anything else (the tracker script) is kept as text.
  let body = if bytes.is_empty() {
    Value::Null
  } else {
    serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
  };

  (status, headers, body)
}

async fn post(state: AppState, uri: &str, headers: &[(&str, &str)], body: Value) -> (StatusCode, Value) {
  let mut builder = Request::post(uri);
  for (name, value) in headers {
    builder = builder.header(*name, *value);
  }

  let (status, _, body) = request(state, builder.body(Body::from(body.to_string())).unwrap()).await;
  (status, body)
}

fn pageview() -> Value {
  json!({ "type": "event", "payload": { "website": WEBSITE, "url": "/", "hostname": "example.com" } })
}

fn bad_request() -> Value {
  json!({ "error": { "message": "Bad request", "code": "bad-request", "status": 400 } })
}

async fn get(state: AppState, uri: &str) -> (StatusCode, Value) {
  let (status, _, body) = request(state, Request::get(uri).body(Body::empty()).unwrap()).await;
  (status, body)
}

fn not_found() -> Value {
  json!({ "error": { "message": "Not found", "code": "not-found", "status": 404 } })
}

#[tokio::test]
async fn heartbeat() {
  assert_eq!(
    get(state(""), "/api/heartbeat").await,
    (StatusCode::OK, json!({ "ok": true }))
  );
}

#[tokio::test]
async fn unknown_route_is_json_404() {
  assert_eq!(get(state(""), "/api/nope").await, (StatusCode::NOT_FOUND, not_found()));
}

#[tokio::test]
async fn routes_are_mounted_under_base_path() {
  assert_eq!(
    get(state("/analytics/"), "/analytics/api/heartbeat").await,
    (StatusCode::OK, json!({ "ok": true }))
  );
  assert_eq!(
    get(state("/analytics"), "/api/heartbeat").await,
    (StatusCode::NOT_FOUND, not_found())
  );
  assert_eq!(
    get(state("/analytics"), "/analytics/api/nope").await,
    (StatusCode::NOT_FOUND, not_found())
  );
}

#[tokio::test]
async fn api_responses_carry_cors_headers() {
  let (status, headers, _) = request(state(""), Request::options("/api/send").body(Body::empty()).unwrap()).await;

  assert_eq!(status, StatusCode::NO_CONTENT);
  assert_eq!(headers["access-control-allow-origin"], "*");
  assert_eq!(headers["access-control-allow-headers"], "*");
  assert_eq!(headers["access-control-allow-methods"], "GET, DELETE, POST, PUT");
  assert_eq!(headers["access-control-max-age"], "86400");
  assert_eq!(headers["cache-control"], "no-cache");

  let (_, headers, _) = request(
    state_with(&[("CORS_MAX_AGE", "600")]),
    Request::get("/api/heartbeat").body(Body::empty()).unwrap(),
  )
  .await;
  assert_eq!(headers["access-control-allow-origin"], "*");
  assert_eq!(headers["access-control-max-age"], "600");
}

#[tokio::test]
async fn send_rejects_invalid_bodies() {
  for body in [
    json!(null),
    json!({ "type": "event", "payload": {} }),
    json!({ "type": "event", "payload": { "website": "nope" } }),
    json!({ "type": "nope", "payload": { "website": WEBSITE } }),
  ] {
    assert_eq!(
      post(state(""), "/api/send", &[], body).await,
      (StatusCode::BAD_REQUEST, bad_request())
    );
  }

  // Not JSON at all.
  let (status, _, body) = request(state(""), Request::post("/api/send").body(Body::from("{oops")).unwrap()).await;
  assert_eq!((status, body), (StatusCode::BAD_REQUEST, bad_request()));
}

#[tokio::test]
async fn send_answers_bots_without_recording() {
  let state = state("");
  let token = cache_token(&state);
  let headers = [
    ("user-agent", "Googlebot/2.1 (+http://www.google.com/bot.html)"),
    ("x-umami-cache", token.as_str()),
  ];

  assert_eq!(
    post(state, "/api/send", &headers, pageview()).await,
    (StatusCode::OK, json!({ "beep": "boop" }))
  );
}

#[tokio::test]
async fn send_forbids_ignored_ips() {
  let state = state_with(&[("IGNORE_IP", "10.0.0.0/8")]);
  let token = cache_token(&state);
  let headers = [
    ("user-agent", BROWSER),
    ("x-forwarded-for", "10.1.2.3"),
    ("x-umami-cache", token.as_str()),
  ];

  assert_eq!(
    post(state, "/api/send", &headers, pageview()).await,
    (
      StatusCode::FORBIDDEN,
      json!({ "error": { "message": "Forbidden", "code": "forbidden", "status": 403 } })
    )
  );
}

#[tokio::test]
async fn send_reports_database_failures_as_server_errors() {
  // No cache token, so the website lookup hits the (unreachable) database.
  assert_eq!(
    post(state(""), "/api/send", &[("user-agent", BROWSER)], pageview()).await,
    (
      StatusCode::INTERNAL_SERVER_ERROR,
      json!({ "error": { "message": "Server error", "code": "server-error", "status": 500 } })
    )
  );
}

#[tokio::test]
async fn batch_reports_per_item_errors() {
  let state = state("");
  let token = cache_token(&state);
  let headers = [("user-agent", "curl/8.0"), ("x-umami-cache", token.as_str())];

  let (status, body) = post(
    state,
    "/api/batch",
    &headers,
    json!([{ "type": "event", "payload": {} }, pageview()]),
  )
  .await;

  assert_eq!(status, StatusCode::OK);
  assert_eq!(
    body,
    json!({
        "size": 2,
        "processed": 1,
        "errors": 1,
        "details": [{ "index": 0, "response": bad_request() }],
        "cache": null,
    })
  );
}

#[tokio::test]
async fn batch_rejects_non_arrays_and_oversized_batches() {
  assert_eq!(
    post(state(""), "/api/batch", &[], pageview()).await,
    (StatusCode::BAD_REQUEST, bad_request())
  );
  assert_eq!(
    post(state(""), "/api/batch", &[], json!(vec![pageview(); 501])).await,
    (StatusCode::BAD_REQUEST, bad_request())
  );
}

#[tokio::test]
async fn collect_endpoint_alias_serves_send() {
  assert_eq!(
    post(state_with(&[("COLLECT_API_ENDPOINT", "/c")]), "/c", &[], json!({})).await,
    (StatusCode::BAD_REQUEST, bad_request())
  );
  assert_eq!(post(state(""), "/c", &[], json!({})).await.0, StatusCode::NOT_FOUND);
}

const TRACKER: &str = include_str!("../assets/script.js");

async fn get_script(state: AppState, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, Value) {
  let mut builder = Request::get(uri);
  for (name, value) in headers {
    builder = builder.header(*name, *value);
  }

  request(state, builder.body(Body::empty()).unwrap()).await
}

#[tokio::test]
async fn serves_the_tracker_script() {
  let (status, headers, body) = get_script(state(""), "/script.js", &[]).await;

  assert_eq!(status, StatusCode::OK);
  assert_eq!(body, Value::String(TRACKER.to_owned()));
  assert_eq!(headers["content-type"], "application/javascript; charset=utf-8");
  assert_eq!(headers["access-control-allow-origin"], "*");
  assert_eq!(headers["cache-control"], "public, max-age=86400, must-revalidate");

  // Revalidation with the ETag gets an empty 304.
  let etag = headers["etag"].to_str().unwrap().to_owned();
  let (status, headers, body) = get_script(state(""), "/script.js", &[("if-none-match", &etag)]).await;
  assert_eq!(status, StatusCode::NOT_MODIFIED);
  assert_eq!(headers["etag"], etag.as_str());
  assert_eq!(body, Value::Null);

  let (status, _, _) = get_script(state(""), "/script.js", &[("if-none-match", "\"stale\"")]).await;
  assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn tracker_script_follows_base_path_and_aliases() {
  let state = || {
    state_with(&[
      ("BASE_PATH", "/analytics"),
      ("TRACKER_SCRIPT_NAME", "umami.js,/js/t.js"),
    ])
  };

  for path in ["/analytics/script.js", "/analytics/umami.js", "/analytics/js/t.js"] {
    let (status, _, body) = get_script(state(), path, &[]).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    assert_eq!(body, Value::String(TRACKER.to_owned()), "{path}");
  }

  assert_eq!(get_script(state(), "/script.js", &[]).await.0, StatusCode::NOT_FOUND);
  assert_eq!(
    get_script(state_with(&[]), "/umami.js", &[]).await.0,
    StatusCode::NOT_FOUND
  );
}

#[tokio::test]
async fn tracker_script_posts_to_collect_endpoint() {
  let (_, _, body) = get_script(state_with(&[("COLLECT_API_ENDPOINT", "/collect")]), "/script.js", &[]).await;

  assert_eq!(body, Value::String(TRACKER.replace("/api/send", "/collect")));
}
