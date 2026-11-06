//! Applies the vendored upstream migrations using Prisma's own bookkeeping.
//!
//! This mirrors `prisma migrate deploy`: progress is recorded in `_prisma_migrations`
//! (same table, same columns, checksum = SHA-256 of `migration.sql`), and runs are
//! serialized with the advisory lock Prisma uses. As a result, upstream Umami and this
//! server can migrate the same database interchangeably.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use sqlx::PgConnection;

/// Advisory lock key used by Prisma's migration engine.
const PRISMA_ADVISORY_LOCK_KEY: i64 = 72707369;

const CREATE_MIGRATIONS_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS "_prisma_migrations" (
    "id"                  VARCHAR(36) PRIMARY KEY NOT NULL,
    "checksum"            VARCHAR(64) NOT NULL,
    "finished_at"         TIMESTAMPTZ,
    "migration_name"      VARCHAR(255) NOT NULL,
    "logs"                TEXT,
    "rolled_back_at"      TIMESTAMPTZ,
    "started_at"          TIMESTAMPTZ NOT NULL DEFAULT now(),
    "applied_steps_count" INTEGER NOT NULL DEFAULT 0
)"#;

/// A single upstream migration: its directory name and the contents of its `migration.sql`.
#[derive(Debug)]
pub struct Migration {
    pub name: &'static str,
    pub sql: &'static str,
}

impl Migration {
    /// The checksum Prisma records for this migration.
    pub fn checksum(&self) -> String {
        hex::encode(Sha256::digest(self.sql.as_bytes()))
    }
}

macro_rules! migrations {
    ($($name:literal),* $(,)?) => {
        &[$(Migration {
            name: $name,
            sql: include_str!(concat!("../migrations/", $name, "/migration.sql")),
        }),*]
    };
}

/// Every upstream migration, in application order.
pub static MIGRATIONS: &[Migration] = migrations![
    "01_init",
    "02_report_schema_session_data",
    "03_metric_performance_index",
    "04_team_redesign",
    "05_add_visit_id",
    "06_session_data",
    "07_add_tag",
    "08_add_utm_clid",
    "09_update_hostname_region",
    "10_add_distinct_id",
    "11_add_segment",
    "12_update_report_parameter",
    "13_add_revenue",
    "14_add_link_and_pixel",
    "15_add_share",
    "16_boards",
    "17_remove_duplicate_key",
    "18_add_performance",
    "19_add_session_replay",
    "20_add_heatmap",
    "21_add_session_link",
    "22_add_2fa",
    "23_update_session_data",
    "24_lowercase_username",
    "25_add_annotation",
    "26_add_api_key",
];

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(
        "migration `{0}` previously failed; resolve it (e.g. `prisma migrate resolve`) before starting the server"
    )]
    PreviouslyFailed(String),
    #[error("migration `{name}` failed: {source}")]
    Apply {
        name: &'static str,
        #[source]
        source: sqlx::Error,
    },
}

/// Applies all pending migrations and returns the names of those applied.
///
/// Holds a session-level advisory lock for the duration, so `conn` should be a
/// dedicated connection rather than one borrowed from a pool.
pub async fn run(conn: &mut PgConnection) -> Result<Vec<&'static str>, MigrateError> {
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(PRISMA_ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await?;

    let result = run_locked(conn).await;

    let unlocked = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(PRISMA_ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await;

    let applied = result?;
    unlocked?;

    Ok(applied)
}

async fn run_locked(conn: &mut PgConnection) -> Result<Vec<&'static str>, MigrateError> {
    sqlx::raw_sql(CREATE_MIGRATIONS_TABLE)
        .execute(&mut *conn)
        .await?;

    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        r#"SELECT migration_name, checksum, finished_at IS NOT NULL
           FROM "_prisma_migrations"
           WHERE rolled_back_at IS NULL
           ORDER BY started_at"#,
    )
    .fetch_all(&mut *conn)
    .await?;

    if let Some((name, ..)) = rows.iter().find(|(_, _, finished)| !finished) {
        return Err(MigrateError::PreviouslyFailed(name.clone()));
    }

    let recorded: HashMap<&str, &str> = rows
        .iter()
        .map(|(name, checksum, _)| (name.as_str(), checksum.as_str()))
        .collect();

    let unknown: Vec<&str> = rows
        .iter()
        .map(|(name, ..)| name.as_str())
        .filter(|name| !MIGRATIONS.iter().any(|m| m.name == *name))
        .collect();

    if !unknown.is_empty() {
        tracing::warn!(
            migrations = ?unknown,
            "database has migrations unknown to this server (newer than Umami {})",
            crate::UMAMI_VERSION,
        );
    }

    let mut applied = Vec::new();

    for migration in MIGRATIONS {
        let checksum = migration.checksum();

        match recorded.get(migration.name) {
            Some(existing) if *existing != checksum => {
                tracing::warn!(
                    migration = migration.name,
                    "applied migration was modified after it was applied (checksum mismatch)",
                );
            }
            Some(_) => {}
            None => {
                apply(conn, migration, &checksum).await?;
                tracing::info!(migration = migration.name, "applied migration");
                applied.push(migration.name);
            }
        }
    }

    Ok(applied)
}

async fn apply(
    conn: &mut PgConnection,
    migration: &'static Migration,
    checksum: &str,
) -> Result<(), MigrateError> {
    let id = uuid::Uuid::new_v4().to_string();

    sqlx::query(
        r#"INSERT INTO "_prisma_migrations" (id, checksum, migration_name, started_at, applied_steps_count)
           VALUES ($1, $2, $3, now(), 0)"#,
    )
    .bind(&id)
    .bind(checksum)
    .bind(migration.name)
    .execute(&mut *conn)
    .await?;

    // A multi-statement simple query runs as one implicit transaction, so a failing
    // script leaves no partial schema behind (Prisma applies scripts the same way).
    match sqlx::raw_sql(migration.sql).execute(&mut *conn).await {
        Ok(_) => {
            sqlx::query(
                r#"UPDATE "_prisma_migrations" SET finished_at = now(), applied_steps_count = 1 WHERE id = $1"#,
            )
            .bind(&id)
            .execute(&mut *conn)
            .await?;

            Ok(())
        }
        Err(source) => {
            // Leave the failed row unfinished, as Prisma does, so later runs refuse to proceed.
            let _ = sqlx::query(r#"UPDATE "_prisma_migrations" SET logs = $2 WHERE id = $1"#)
                .bind(&id)
                .bind(source.to_string())
                .execute(&mut *conn)
                .await;

            Err(MigrateError::Apply {
                name: migration.name,
                source,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vendored_migration_is_registered_in_order() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect();
        on_disk.sort();

        let registered: Vec<&str> = MIGRATIONS.iter().map(|m| m.name).collect();

        assert_eq!(registered, on_disk);
    }

    #[test]
    fn checksum_is_lowercase_sha256_hex() {
        let migration = Migration {
            name: "test",
            sql: "abc",
        };

        assert_eq!(
            migration.checksum(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
