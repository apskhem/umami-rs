//! Headless, Umami-compatible analytics server.

pub mod config;
pub mod crypto;
pub mod error;
mod routes;
pub mod startup;
pub mod state;

use axum::Router;
use tower_http::trace::TraceLayer;

use crate::error::AppError;
use crate::state::AppState;

/// Builds the HTTP application, mounted under `BASE_PATH` when one is configured.
pub fn app(state: AppState) -> Router {
    let api = routes::router();
    let base_path = state.config.base_path.as_str();

    let router = if base_path.is_empty() {
        api
    } else {
        Router::new().nest(base_path, api)
    };

    router
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn not_found() -> AppError {
    AppError::NotFound
}
