//! Headless, Umami-compatible analytics server.

pub mod analytics;
pub mod auth;
pub mod collect;
pub mod config;
pub mod crypto;
pub mod error;
pub mod healthcheck;
pub mod json;
pub mod jwt;
pub mod permissions;
pub mod queries;
mod routes;
pub mod startup;
pub mod state;
pub mod tracker;
pub mod zod;

use axum::{
  Router,
  middleware,
};
use tower_http::trace::TraceLayer;

use crate::error::AppError;
use crate::state::AppState;

/// Builds the HTTP application, mounted under `BASE_PATH` when one is configured.
pub fn app(state: AppState) -> Router {
  let api = routes::router(&state.config)
    .fallback(not_found)
    .layer(middleware::from_fn_with_state(state.clone(), routes::api_headers));
  let base_path = state.config.base_path.as_str();

  let router = if base_path.is_empty() {
    api
  } else {
    Router::new().nest(base_path, api).fallback(not_found)
  };

  router.layer(TraceLayer::new_for_http()).with_state(state)
}

async fn not_found() -> AppError {
  AppError::NotFound
}
