use anyhow::Context;
use sqlx::{Connection, PgConnection};

use crate::config::Config;

/// Startup database check, mirroring upstream's `scripts/check-db.js`: verify
/// connectivity and the Postgres version, then apply pending migrations.
pub async fn prepare_database(config: &Config) -> anyhow::Result<()> {
    if config.skip_db_check {
        tracing::info!("skipping database check (SKIP_DB_CHECK)");
        return Ok(());
    }

    let options = umami_db::connect_options(config.migration_database_url())
        .context("invalid database URL")?;
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

/// Resolves on Ctrl-C or, on Unix, SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
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
