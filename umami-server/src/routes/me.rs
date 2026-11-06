use axum::Json;
use serde_json::Value;

use crate::auth::Auth;

/// `GET /api/me`: the caller's auth context.
pub async fn get(auth: Auth) -> Json<Value> {
    Json(auth.to_json())
}
