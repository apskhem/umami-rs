use std::sync::Arc;

use sqlx::PgPool;

use crate::config::Config;

/// Shared, cheaply cloneable state available to every handler.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
}
