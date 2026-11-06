use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use umami_server::app;
use umami_server::config::Config;
use umami_server::state::AppState;

/// State with a pool that never connects, for routes that don't touch the database.
fn state(base_path: &str) -> AppState {
    let config = Config::from_lookup(|key| match key {
        "DATABASE_URL" => Some("postgresql://umami@localhost/umami".to_owned()),
        "BASE_PATH" => Some(base_path.to_owned()),
        _ => None,
    })
    .unwrap();

    let db = PgPoolOptions::new()
        .connect_lazy_with(umami_db::connect_options(&config.database_url).unwrap());

    AppState {
        db,
        config: Arc::new(config),
    }
}

async fn get(state: AppState, uri: &str) -> (StatusCode, Value) {
    let response = app(state)
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();

    (status, serde_json::from_slice(&bytes).unwrap())
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
    assert_eq!(
        get(state(""), "/api/nope").await,
        (StatusCode::NOT_FOUND, not_found())
    );
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
