use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use umami_server::config::Config;
use umami_server::state::AppState;
use umami_server::{
  app,
  healthcheck,
  startup,
};

// musl's allocator (used by the static Linux builds) is slow under concurrency.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  // Like upstream's `dotenv/config`: a missing `.env` is fine.
  let _ = dotenvy::dotenv();

  if std::env::args().nth(1).as_deref() == Some("healthcheck") {
    let (host, port, base_path) = healthcheck::target_from_env();

    if let Err(error) = healthcheck::check(&host, port, &base_path).await {
      eprintln!("{error}");
      std::process::exit(1);
    }
    return Ok(());
  }

  tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
    .init();

  let config = Config::from_env()?;

  startup::prepare_database(&config).await?;

  let db = umami_db::connect(&config.database_url)
    .await
    .context("unable to connect to the database")?;

  let listener = TcpListener::bind((config.host.as_str(), config.port))
    .await
    .with_context(|| format!("unable to bind {}:{}", config.host, config.port))?;

  tracing::info!(
      address = %listener.local_addr()?,
      base_path = %config.base_path,
      "listening",
  );

  let geo = startup::open_geo_database(&config.collect)?;
  let dashboard = startup::open_dashboard(&config)?;
  crate_ua_rules::init();

  let state = AppState {
    db,
    config: Arc::new(config),
    geo: geo.map(Arc::new),
    dashboard: dashboard.map(Arc::new),
  };

  axum::serve(listener, app(state).into_make_service_with_connect_info::<SocketAddr>())
    .with_graceful_shutdown(startup::shutdown_signal())
    .await?;

  Ok(())
}
