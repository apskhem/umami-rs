use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use umami_server::config::Config;
use umami_server::state::AppState;
use umami_server::{app, startup};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Like upstream's `dotenv/config`: a missing `.env` is fine.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
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

    let state = AppState {
        db,
        config: Arc::new(config),
    };

    axum::serve(listener, app(state))
        .with_graceful_shutdown(startup::shutdown_signal())
        .await?;

    Ok(())
}
