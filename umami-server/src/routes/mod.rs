use axum::Router;
use axum::routing::get;

use crate::state::AppState;

mod heartbeat;

/// API routes, relative to `BASE_PATH`. Paths mirror upstream's `src/app/api/**/route.ts`.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/heartbeat", get(heartbeat::get))
}
