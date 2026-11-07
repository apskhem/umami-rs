use anyhow::Context;
use sqlx::{
  Connection,
  PgConnection,
};

use crate::collect::GeoDatabase;
use crate::config::{
  CollectConfig,
  Config,
};
use crate::dashboard::Dashboard;

/// Startup database check, mirroring upstream's `scripts/check-db.js`: verify
/// connectivity and the Postgres version, then apply pending migrations.
pub async fn prepare_database(config: &Config) -> anyhow::Result<()> {
  if config.skip_db_check {
    tracing::info!("skipping database check (SKIP_DB_CHECK)");
    return Ok(());
  }

  let options = umami_db::connect_options(config.migration_database_url()).context("invalid database URL")?;
  let mut conn = PgConnection::connect_with(&options)
    .await
    .context("unable to connect to the database")?;

  let version = umami_db::check_server_version(&mut conn).await?;
  tracing::info!(%version, "database version check successful");

  if config.skip_db_migration {
    tracing::info!("skipping database migrations (SKIP_DB_MIGRATION)");
  } else {
    let applied = umami_db::migrate::run(&mut conn).await?;
    tracing::info!(
      applied = applied.len(),
      umami_version = umami_db::UMAMI_VERSION,
      "database is up to date",
    );
  }

  conn.close().await?;

  Ok(())
}

/// Opens the MaxMind database for geolocation. Without one, locations come only from
/// CDN headers; that is an error only if `GEOLITE_DB_PATH` names a missing file.
pub fn open_geo_database(config: &CollectConfig) -> anyhow::Result<Option<GeoDatabase>> {
  let path = &config.geo_database;

  if !path.exists() && !config.geo_database_explicit {
    tracing::info!(
        path = %path.display(),
        "no GeoLite2 database; geolocation will use CDN headers only",
    );
    return Ok(None);
  }

  let database =
    GeoDatabase::open(path).with_context(|| format!("unable to open GeoLite2 database {}", path.display()))?;
  tracing::info!(path = %path.display(), "loaded GeoLite2 database");

  Ok(Some(database))
}

/// Loads the dashboard build. Without one the server is headless; that is an error
/// only if `DASHBOARD_DIR` names a missing directory.
pub fn open_dashboard(config: &Config) -> anyhow::Result<Option<Dashboard>> {
  let path = &config.dashboard.dir;

  if config.dashboard.disabled {
    tracing::info!("dashboard disabled (DISABLE_UI)");
    return Ok(None);
  }
  if !path.exists() && !config.dashboard.dir_explicit {
    tracing::info!(path = %path.display(), "no dashboard build; serving the API only");
    return Ok(None);
  }

  let dashboard =
    Dashboard::load(config).with_context(|| format!("unable to load the dashboard in {}", path.display()))?;
  tracing::info!(path = %path.display(), "loaded dashboard");

  Ok(Some(dashboard))
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM.
pub async fn shutdown_signal() {
  let ctrl_c = async {
    tokio::signal::ctrl_c().await.expect("failed to install Ctrl-C handler");
  };

  #[cfg(unix)]
  let terminate = async {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
      .expect("failed to install SIGTERM handler")
      .recv()
      .await;
  };

  #[cfg(not(unix))]
  let terminate = std::future::pending::<()>();

  tokio::select! {
      () = ctrl_c => {},
      () = terminate => {},
  }

  tracing::info!("shutting down");
}
