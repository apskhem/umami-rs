use axum::Json;
use serde_json::{Value, json};

/// `GET /api/heartbeat`
pub async fn get() -> Json<Value> {
    Json(json!({ "ok": true }))
}
