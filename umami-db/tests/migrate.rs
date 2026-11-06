//! Runs the migrator against a real Postgres.
//!
//! `TEST_DATABASE_URL=postgresql://umami:umami@localhost:5432/umami cargo test -- --ignored`
//! (see `compose.yaml`). Each test works in a throwaway schema.

use sqlx::{
  Connection,
  PgConnection,
};
use umami_db::{
  check_server_version,
  connect_options,
  migrate,
};

struct Scratch {
  base_url: String,
  schema: String,
}

impl Scratch {
  async fn new() -> Self {
    let base_url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL must be set");
    let schema = format!("umami_test_{}", uuid_suffix());

    let mut conn = PgConnection::connect(&base_url).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA \"{schema}\"")))
      .execute(&mut conn)
      .await
      .unwrap();

    Self { base_url, schema }
  }

  async fn connect(&self) -> PgConnection {
    let separator = if self.base_url.contains('?') { '&' } else { '?' };
    let url = format!("{}{separator}schema={}", self.base_url, self.schema);

    PgConnection::connect_with(&connect_options(&url).unwrap())
      .await
      .unwrap()
  }

  async fn drop(self) {
    let mut conn = PgConnection::connect(&self.base_url).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA \"{}\" CASCADE", self.schema)))
      .execute(&mut conn)
      .await
      .unwrap();
  }
}

fn uuid_suffix() -> String {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_nanos()
    .to_string()
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn migrates_fresh_database_then_is_idempotent() {
  let scratch = Scratch::new().await;
  let mut conn = scratch.connect().await;

  check_server_version(&mut conn).await.unwrap();

  let applied = migrate::run(&mut conn).await.unwrap();
  assert_eq!(applied.len(), migrate::MIGRATIONS.len());

  let (admins,): (i64,) = sqlx::query_as(r#"SELECT count(*) FROM "user" WHERE username = 'admin'"#)
    .fetch_one(&mut conn)
    .await
    .unwrap();
  assert_eq!(admins, 1);

  let (recorded,): (i64,) = sqlx::query_as(
    r#"SELECT count(*) FROM "_prisma_migrations" WHERE finished_at IS NOT NULL AND applied_steps_count = 1"#,
  )
  .fetch_one(&mut conn)
  .await
  .unwrap();
  assert_eq!(recorded as usize, migrate::MIGRATIONS.len());

  let reapplied = migrate::run(&mut conn).await.unwrap();
  assert!(reapplied.is_empty());

  conn.close().await.unwrap();
  scratch.drop().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn refuses_to_continue_after_a_failed_migration() {
  let scratch = Scratch::new().await;
  let mut conn = scratch.connect().await;

  migrate::run(&mut conn).await.unwrap();
  sqlx::query(r#"UPDATE "_prisma_migrations" SET finished_at = NULL WHERE migration_name = '26_add_api_key'"#)
    .execute(&mut conn)
    .await
    .unwrap();

  let err = migrate::run(&mut conn).await.unwrap_err();
  assert!(
    matches!(&err, migrate::MigrateError::PreviouslyFailed(name) if name == "26_add_api_key"),
    "{err}"
  );

  conn.close().await.unwrap();
  scratch.drop().await;
}
