use std::sync::Arc;

use sqlx::PgPool;

use crate::collect::GeoDatabase;
use crate::config::Config;

/// Shared, cheaply cloneable state available to every handler.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
    /// The MaxMind database for geolocation, if one is installed.
    pub geo: Option<Arc<GeoDatabase>>,
}
