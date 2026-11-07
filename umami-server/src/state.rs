use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use crate::collect::GeoDatabase;
use crate::config::Config;
use crate::crypto;
use crate::dashboard::Dashboard;

/// Shared, cheaply cloneable state available to every handler.
#[derive(Clone)]
pub struct AppState {
  pub db: PgPool,
  pub config: Arc<Config>,
  /// The MaxMind database for geolocation, if one is installed.
  pub geo: Option<Arc<GeoDatabase>>,
  /// The static dashboard build, if one is installed.
  pub dashboard: Option<Arc<Dashboard>>,
}

impl AppState {
  /// A new record ID: UUIDv7 if `USE_UUIDV7` is set, else v4 (upstream's `uuid()`).
  pub fn new_id(&self) -> Uuid {
    crypto::random_uuid(self.config.collect.use_uuidv7)
  }
}
