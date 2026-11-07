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

fn unauthorized() -> Value {
  json!({ "error": { "message": "Unauthorized", "code": "unauthorized", "status": 401 } })
}

#[tokio::test]
async fn share_and_segment_routes_validate_before_auth() {
  let segments = format!("/api/websites/{WEBSITE}/segments");
  let shares = format!("/api/websites/{WEBSITE}/shares");
  let share = format!("/api/share/id/{WEBSITE}");
  let segment = json!({ "type": "segment", "name": "n", "parameters": {} });

  for (uri, body) in [
    (
      "/api/share",
      json!({ "entityId": "x", "shareType": 1, "name": "n", "parameters": {} }),
    ),
    (
      "/api/share",
      json!({ "entityId": WEBSITE, "shareType": "web", "name": "n", "parameters": {} }),
    ),
    (
      "/api/share",
      json!({ "entityId": WEBSITE, "shareType": 1, "name": "n" }),
    ),
    (
      "/api/share",
      json!({ "entityId": WEBSITE, "shareType": 1, "name": "n", "slug": null, "parameters": {} }),
    ),
    (&share, json!({ "name": "n", "parameters": {} })),
    (
      &share,
      json!({ "name": "n", "slug": "s".repeat(101), "parameters": {} }),
    ),
    (&shares, json!({ "name": "n".repeat(201) })),
    (&shares, json!({ "name": "n", "parameters": [] })),
    (&segments, json!({ "type": "group", "name": "n", "parameters": {} })),
    (&segments, json!({ "type": "segment", "name": "n" })),
  ] {
    assert_eq!(
      post(state(""), uri, &[], body.clone()).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri} {body}"
    );
  }
  for uri in [
    format!("{segments}?search=x"),
    format!("{segments}?type=x"),
    format!("{shares}?segment=x"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri}"
    );
  }

  let (status, body) = post(
    state(""),
    &segments,
    &[],
    json!({ "type": "cohort", "name": "n", "parameters": {
        "sessionPropertyFilters": [{ "propertyName": "p", "dataType": 1, "operator": "eq", "value": "v" }],
    } }),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert_eq!(
    body["error"]["message"],
    "Session property filters are only supported for segments."
  );

  // Valid requests without credentials never reach the database.
  for (uri, body) in [
    (
      "/api/share",
      json!({ "entityId": WEBSITE, "shareType": "1", "name": "n", "parameters": {} }),
    ),
    (share.as_str(), json!({ "name": "n", "slug": "s", "parameters": {} })),
    (shares.as_str(), json!({ "name": "n" })),
    (segments.as_str(), segment.clone()),
    (&format!("{segments}/{WEBSITE}"), segment.clone()),
  ] {
    assert_eq!(
      post(state(""), uri, &[], body).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
  for uri in [format!("{segments}?type=cohort"), shares.clone(), share.clone()] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
}

#[tokio::test]
async fn link_and_pixel_routes_validate_before_auth() {
  let slug = "parity-slug";
  for (uri, body) in [
    ("/api/links", json!({ "name": "n", "slug": slug })),
    ("/api/links", json!({ "name": "n", "url": "u", "slug": "short" })),
    (
      "/api/links",
      json!({ "name": "n".repeat(101), "url": "u", "slug": slug }),
    ),
    (
      "/api/links",
      json!({ "name": "n", "url": "u".repeat(501), "slug": slug }),
    ),
    (
      "/api/links",
      json!({ "name": "n", "url": "u", "slug": slug, "teamId": "team" }),
    ),
    ("/api/pixels", json!({ "name": "n", "slug": slug, "id": "x" })),
    (&format!("/api/links/{WEBSITE}"), json!({ "url": null })),
    (&format!("/api/pixels/{WEBSITE}"), json!({ "slug": "short" })),
    (&format!("/api/links/{WEBSITE}/shares"), json!({ "name": 1 })),
  ] {
    assert_eq!(
      post(state(""), uri, &[], body.clone()).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri} {body}"
    );
  }
  for uri in [
    "/api/links/charts".to_owned(),
    "/api/links/charts?ids=x".to_owned(),
    format!("/api/pixels/charts?ids={WEBSITE}&startAt=1.5"),
    "/api/links?pageSize=0".to_owned(),
    format!("/api/teams/{WEBSITE}/pixels?sortDescending=yes"),
    format!("/api/links/{WEBSITE}/shares?segment=x"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri}"
    );
  }

  // Valid requests without credentials never reach the database; a pixel
  // ignores the URL it has no use for.
  for (uri, body) in [
    (
      "/api/links".to_owned(),
      json!({ "name": "", "url": "u", "slug": slug, "teamId": null, "id": null }),
    ),
    (
      "/api/pixels".to_owned(),
      json!({ "name": "n", "slug": slug, "url": null }),
    ),
    (format!("/api/pixels/{WEBSITE}"), json!({ "url": null })),
    (format!("/api/links/{WEBSITE}/shares"), json!({ "name": "n" })),
  ] {
    assert_eq!(
      post(state(""), &uri, &[], body).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
  for uri in [
    format!("/api/links/charts?ids={WEBSITE},{WEBSITE}&startAt=0&endAt=1"),
    "/api/pixels".to_owned(),
    format!("/api/links/{WEBSITE}"),
    format!("/api/pixels/{WEBSITE}/shares"),
    format!("/api/teams/{WEBSITE}/links"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
  let (status, _, body) = request(
    state(""),
    Request::delete(format!("/api/links/{WEBSITE}"))
      .body(Body::empty())
      .unwrap(),
  )
  .await;
  assert_eq!((status, body), (StatusCode::UNAUTHORIZED, unauthorized()));
}

#[tokio::test]
async fn board_routes_validate_before_auth() {
  let board = format!("/api/boards/{WEBSITE}");
  let clone = format!("{board}/clone");
  for (uri, body) in [
    ("/api/boards", json!({ "name": "n" })),
    ("/api/boards", json!({ "type": "dashboard", "name": "n" })),
    ("/api/boards", json!({ "type": "mixed", "name": "n".repeat(101) })),
    (
      "/api/boards",
      json!({ "type": "mixed", "name": "n", "description": "d".repeat(501) }),
    ),
    ("/api/boards", json!({ "type": "mixed", "name": "n", "teamId": "x" })),
    (
      "/api/boards",
      json!({ "type": "mixed", "name": "n", "parameters": null }),
    ),
    (
      "/api/boards",
      json!({ "type": "website", "name": "n", "parameters": { "websiteId": "x" } }),
    ),
    (
      "/api/boards",
      json!({ "type": "pixel", "name": "n", "parameters": { "pixelId": null } }),
    ),
    (&board, json!({ "type": "Mixed" })),
    (&board, json!({ "name": "n".repeat(201) })),
    (&board, json!({ "parameters": [] })),
    (&clone, json!({ "description": 1 })),
    ("/api/dashboard", json!({ "parameters": "x" })),
    (&format!("{board}/shares"), json!({})),
  ] {
    assert_eq!(
      post(state(""), uri, &[], body.clone()).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri} {body}"
    );
  }
  for uri in [
    "/api/websites/charts".to_owned(),
    format!("/api/websites/charts?ids={WEBSITE}&endAt=x"),
    "/api/boards?page=0".to_owned(),
    format!("/api/teams/{WEBSITE}/boards?sortDescending=1"),
    format!("{board}/shares?cohort=x"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri}"
    );
  }

  // Valid requests without credentials never reach the database.
  for (uri, body) in [
    (
      "/api/boards",
      json!({ "type": "open", "name": "", "userId": null, "parameters": { "websiteId": WEBSITE, "rows": 1 } }),
    ),
    (board.as_str(), json!({ "type": "dashboard", "parameters": {} })),
    (clone.as_str(), json!({})),
    ("/api/dashboard", json!({})),
    (&format!("{board}/shares"), json!({ "name": "n" })),
  ] {
    assert_eq!(
      post(state(""), uri, &[], body).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
  for uri in [
    format!("/api/websites/charts?ids={WEBSITE}"),
    "/api/boards".to_owned(),
    board.clone(),
    format!("{board}/shares"),
    format!("/api/teams/{WEBSITE}/boards"),
    "/api/dashboard".to_owned(),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
}

#[tokio::test]
async fn data_routes_validate_before_auth() {
  let base = format!("/api/websites/{WEBSITE}");
  let range = "startAt=0&endAt=1";
  for uri in [
    format!("{base}/event-data?startAt=0"),
    format!("{base}/event-data?{range}&pageSize=0"),
    format!("{base}/event-data/values?{range}"),
    format!("{base}/event-data/values?{range}&propertyName=p&dataType=x"),
    format!("{base}/event-data-pivot?{range}"),
    format!("{base}/event-data-pivot/date-series?{range}&eventName=e"),
    format!("{base}/event-data-pivot/numeric-series?{range}&eventName=e&propertyName=p&metric=max"),
    format!("{base}/event-data-pivot/property-series?{range}&eventName=e&propertyName=p&unit=week2"),
    format!("{base}/session-data-pivot?{range}&pageSize=x"),
    format!("{base}/session-data/stats?{range}"),
    format!("{base}/session-data/properties?{range}&segment=x"),
    format!("{base}/session-data/values?{range}&dataType=1.5"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri}"
    );
  }

  // Valid requests without credentials never reach the database; routes
  // without `timezone` or `unit` ignore them.
  for uri in [
    format!("{base}/event-data?{range}&page=2&pf_x=eq.y"),
    format!("{base}/event-data/stats?{range}&timezone=Mars/Base&unit=x"),
    format!("{base}/event-data/values?{range}&propertyName=p&dataType="),
    format!("{base}/event-data-pivot/numeric-stats?{range}&eventName=e&propertyName=p&unit=x"),
    format!("{base}/session-data/properties?{range}&timezone=x"),
    format!("{base}/session-data/values?{range}"),
    format!("{base}/event-data/{WEBSITE}"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
}

#[tokio::test]
async fn report_routes_validate_before_auth() {
  let base = format!("/api/websites/{WEBSITE}");
  let range = "startAt=0&endAt=1";
  for uri in [
    format!("{base}/retention?startAt=1&endAt=0"),
    format!("{base}/retention?startAt=-1&endAt=0"),
    format!("{base}/journeys?{range}&steps=9"),
    format!("{base}/breakdown?{range}&fields=%5B%5D"),
    format!("{base}/breakdown?{range}&fields=%5B%22os%22,1%5D"),
    format!("{base}/utm/metrics?{range}"),
    format!("{base}/attribution?{range}&model=first-click&type=page&step=/"),
    format!("{base}/performance/metrics?{range}&type=path&limit=501"),
    format!("{base}/revenue/chart?{range}"),
    format!("{base}/revenue/metrics?{range}&currency=USD"),
    format!("{base}/funnels/stats?{range}&window=-1&steps=%5B%5D"),
    format!("{base}/goals/stats?{range}&value=x"),
    format!("{base}/goals?pageSize=0"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri}"
    );
  }
  for (uri, body) in [
    (
      format!("{base}/funnels"),
      json!({ "name": "n", "parameters": { "window": 1, "steps": [] } }),
    ),
    (
      format!("{base}/goals"),
      json!({ "name": "", "parameters": { "type": "path", "value": "/" } }),
    ),
    (
      format!("{base}/goals/{WEBSITE}"),
      json!({ "parameters": { "type": "path", "value": "/" } }),
    ),
  ] {
    assert_eq!(
      post(state(""), &uri, &[], body.clone()).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri} {body}"
    );
  }

  // Valid requests without credentials never reach the database.
  for uri in [
    format!("{base}/retention?{range}&unit=day"),
    format!("{base}/journeys?{range}&steps=2.5"),
    format!("{base}/breakdown?{range}&fields=%5B%22os%22,%22os%22%5D"),
    format!("{base}/revenue/stats?{range}&currency=usd"),
    format!("{base}/goals/stats?{range}&type=anything&value=x"),
    format!("{base}/funnels"),
    format!("{base}/goals/{WEBSITE}"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
}

#[tokio::test]
async fn annotation_routes_validate_before_auth() {
  let list = format!("/api/websites/{WEBSITE}/annotations");
  let item = format!("{list}/{WEBSITE}");
  for (uri, body) in [
    (&list, json!({ "note": "n" })),
    (&list, json!({ "date": "2026-01-01", "note": "" })),
    (&list, json!({ "date": "2026-01-01", "note": "n", "allDay": "yes" })),
    (&item, json!({ "date": {}, "note": "n" })),
  ] {
    assert_eq!(
      post(state(""), uri, &[], body.clone()).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri} {body}"
    );
  }
  assert_eq!(
    get(state(""), &format!("{list}?endAt=x")).await,
    (StatusCode::BAD_REQUEST, bad_request())
  );

  // Valid requests without credentials never reach the database.
  assert_eq!(
    post(state(""), &list, &[], json!({ "date": 0, "note": "n" })).await,
    (StatusCode::UNAUTHORIZED, unauthorized())
  );
  for uri in [format!("{list}?startAt=1&endAt=2&search=x"), item] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
}

#[tokio::test]
async fn replay_routes_validate_before_auth() {
  let base = format!("/api/websites/{WEBSITE}");
  let range = "startAt=0&endAt=1";
  let long_path = "/".repeat(501);
  for uri in [
    format!("{base}/replays"),
    format!("{base}/replays?{range}&minDuration=-1"),
    format!("{base}/replays?{range}&minDuration=1.5"),
    format!("{base}/replays?{range}&pageSize=0"),
    format!("{base}/sessions/{WEBSITE}/replays?{range}&page=x"),
    format!("{base}/replays/saved?pageSize=0"),
    format!("{base}/heatmaps?startAt=1&endAt=0"),
    format!("{base}/heatmaps?{range}&mode=move"),
    format!("{base}/heatmaps?{range}&urlPath={long_path}"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri}"
    );
  }
  let saved = format!("{base}/replays/saved/{WEBSITE}");
  for body in [
    json!({}),
    json!({ "isSaved": "yes" }),
    json!({ "isSaved": true, "name": "n".repeat(101) }),
  ] {
    assert_eq!(
      post(state(""), &saved, &[], body.clone()).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{body}"
    );
  }

  // Valid requests without credentials never reach the database.
  assert_eq!(
    post(state(""), &saved, &[], json!({ "isSaved": true, "name": "n" })).await,
    (StatusCode::UNAUTHORIZED, unauthorized())
  );
  for uri in [
    format!("{base}/replays?{range}&minDuration=30&search=x&page=2"),
    format!("{base}/sessions/{WEBSITE}/replays?{range}"),
    format!("{base}/replays/{WEBSITE}?until=x&chunkIndex=1.5"),
    format!("{base}/replays/saved?search=x"),
    saved,
    format!("{base}/heatmaps?{range}&mode=scroll&urlPath=/"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
}

fn recording() -> Value {
  json!({ "type": "record", "payload": { "website": WEBSITE, "events": [{ "type": 4, "timestamp": 1 }] } })
}

#[tokio::test]
async fn record_rejects_invalid_bodies() {
  for body in [
    json!(null),
    json!({ "type": "record", "payload": { "website": WEBSITE } }),
    json!({ "type": "record", "payload": { "website": "nope", "events": [] } }),
    json!({ "type": "nope", "payload": { "website": WEBSITE, "events": [] } }),
    json!({ "type": "record", "payload": { "website": WEBSITE, "events": vec![json!({}); 201] } }),
    json!({ "type": "record", "payload": { "website": WEBSITE, "events": [], "timestamp": 1.5 } }),
    json!({ "type": "heatmap", "payload": { "website": WEBSITE, "events": [{ "type": "move", "url": "/" }] } }),
  ] {
    assert_eq!(
      post(state(""), "/api/record", &[], body.clone()).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{body}"
    );
  }

  let oversized = json!({ "type": "record", "payload": { "website": WEBSITE, "events": ["x".repeat(1_000_000)] } });
  assert_eq!(
    post(state(""), "/api/record", &[], oversized).await,
    (
      StatusCode::PAYLOAD_TOO_LARGE,
      json!({ "error": { "message": "Payload too large", "code": "payload-too-large", "status": 413 } })
    )
  );
}

#[tokio::test]
async fn record_needs_a_session_token_for_the_website() {
  // Nothing to record: answered before the token is checked.
  let empty = json!({ "type": "heatmap", "payload": { "website": WEBSITE, "events": [] } });
  assert_eq!(
    post(state(""), "/api/record", &[], empty).await,
    (StatusCode::OK, json!({ "ok": true }))
  );

  let error = |message: &str| json!({ "error": { "message": message, "code": "bad-request", "status": 400 } });
  assert_eq!(
    post(state(""), "/api/record", &[], recording()).await,
    (StatusCode::BAD_REQUEST, error("Missing session token."))
  );
  assert_eq!(
    post(state(""), "/api/record", &[("x-umami-cache", "nope")], recording()).await,
    (StatusCode::BAD_REQUEST, error("Invalid session token."))
  );

  // A token issued for another website is refused (upstream records under it).
  let state = state("");
  let other = CacheToken::new(
    Some("6b1f3c2e-8a4d-4e5f-9a0b-1c2d3e4f5a6b".to_owned()),
    "ef98a066-967b-5a5f-aa43-3c1492f5d34e".to_owned(),
    "4357da39-efbd-5647-a20c-8896a5379030".to_owned(),
    chrono::Utc::now().timestamp(),
    None,
  )
  .sign(&state.config.secret);
  assert_eq!(
    post(state, "/api/record", &[("x-umami-cache", &other)], recording()).await,
    (StatusCode::BAD_REQUEST, error("Invalid session token."))
  );
}

const RECORDER: &str = include_str!("../assets/recorder.js");

#[tokio::test]
async fn serves_the_recorder_script() {
  let (status, headers, body) = get_script(state("/analytics"), "/analytics/recorder.js", &[]).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body, Value::String(RECORDER.to_owned()));
  assert_eq!(headers["content-type"], "application/javascript; charset=utf-8");

  // A tracker alias of the same name serves the tracker instead.
  let (_, _, body) = get_script(
    state_with(&[("TRACKER_SCRIPT_NAME", "recorder.js")]),
    "/recorder.js",
    &[],
  )
  .await;
  assert_eq!(body, Value::String(TRACKER.to_owned()));
}

#[tokio::test]
async fn export_validates_before_auth() {
  let export = format!("/api/websites/{WEBSITE}/export");
  let range = "startAt=0&endAt=1";
  for uri in [
    export.clone(),
    format!("{export}?startAt=0"),
    format!("{export}?{range}&pageSize=0"),
    format!("{export}?{range}&unit=week2"),
    format!("{export}?{range}&compare=next"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::BAD_REQUEST, bad_request()),
      "{uri}"
    );
  }

  // Valid requests without credentials never reach the database; filters are
  // not part of upstream's schema, so even malformed ones are ignored.
  for uri in [
    format!("{export}?{range}"),
    format!("{export}?startDate=2026-01-01&endDate=2026-02-01&format=json&browser=x&segment=y&cohort=z"),
  ] {
    assert_eq!(
      get(state(""), &uri).await,
      (StatusCode::UNAUTHORIZED, unauthorized()),
      "{uri}"
    );
  }
}
