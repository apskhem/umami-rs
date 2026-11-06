//! Schema parity: umami-rs's migrator must produce exactly the database Prisma does,
//! and each server must run on a database the other migrated.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use serde_json::json;
use sqlx::PgPool;
use umami_parity::{
  Stack,
  wait_for,
};

/// Every column, index and constraint in the public schema, as sorted text.
async fn schema(db: &PgPool) -> Vec<String> {
  let mut items: Vec<String> = sqlx::query_scalar(
    "select format('column %s.%s %s(%s,%s,%s) nullable=%s default=%s', table_name, column_name, \
           data_type, character_maximum_length, numeric_precision, numeric_scale, is_nullable, column_default) \
         from information_schema.columns where table_schema = 'public' \
         union all \
         select format('index %s', indexdef) from pg_indexes where schemaname = 'public' \
         union all \
         select format('constraint %s.%s %s', c.conrelid::regclass, c.conname, pg_get_constraintdef(c.oid)) \
         from pg_constraint c join pg_namespace n on n.oid = c.connamespace where n.nspname = 'public' \
         union all \
         select format('extension %s', extname) from pg_extension",
  )
  .fetch_all(db)
  .await
  .unwrap();

  items.sort();
  items
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn schema_matches_prisma() {
  let stack = Stack::connect().await;

  let upstream = schema(&stack.upstream.db).await;
  let rs = schema(&stack.rs.db).await;
  assert!(upstream.len() > 300, "suspiciously small schema: {upstream:#?}");

  let only_upstream: Vec<_> = upstream.iter().filter(|i| !rs.contains(i)).collect();
  let only_rs: Vec<_> = rs.iter().filter(|i| !upstream.contains(i)).collect();

  assert!(
    only_upstream.is_empty() && only_rs.is_empty(),
    "schemas differ\nonly in upstream:\n  {only_upstream:#?}\nonly in rs:\n  {only_rs:#?}"
  );
}

/// Same migration names, checksums and outcome as Prisma recorded, which is what
/// lets Prisma treat a umami-rs database as up to date (and vice versa).
#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn migration_records_match_prisma() {
  let stack = Stack::connect().await;
  let records = |db| {
    async move {
      sqlx::query_as::<_, (String, String, i32, bool, bool)>(
        "select migration_name, checksum, applied_steps_count, finished_at is not null, \
             rolled_back_at is not null from _prisma_migrations order by migration_name",
      )
      .fetch_all(db)
      .await
      .unwrap()
    }
  };

  let upstream = records(&stack.upstream.db).await;
  let rs = records(&stack.rs.db).await;

  assert_eq!(upstream.len(), umami_db::migrate::MIGRATIONS.len());
  assert_eq!(upstream, rs);
}

/// Upstream started on the database umami-rs migrated (and vice versa) without
/// applying anything, and both can record events there.
#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn servers_share_each_others_databases() {
  let stack = Stack::connect().await;
  let upstream_on_rs = std::env::var("PARITY_UPSTREAM_ON_RS_URL").unwrap_or("http://localhost:3003".into());
  let rs_on_upstream = std::env::var("PARITY_RS_ON_UPSTREAM_URL").unwrap_or("http://localhost:3004".into());

  wait_for(stack.http(), "upstream-on-rs-db", &upstream_on_rs).await;
  wait_for(stack.http(), "rs-on-upstream-db", &rs_on_upstream).await;

  for target in [&stack.upstream, &stack.rs] {
    let (count, unfinished): (i64, i64) =
      sqlx::query_as("select count(*), count(*) filter (where finished_at is null) from _prisma_migrations")
        .fetch_one(&target.db)
        .await
        .unwrap();

    assert_eq!(
      (count as usize, unfinished),
      (umami_db::migrate::MIGRATIONS.len(), 0),
      "{}: migrations were re-applied or left unfinished",
      target.name
    );
  }

  // Each server records an event in the other's database.
  let website = stack.website().await;
  for (url, ua) in [
    (
      &upstream_on_rs,
      "Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0",
    ),
    (
      &rs_on_upstream,
      "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0",
    ),
  ] {
    let response = stack
      .http()
      .post(format!("{url}/api/send"))
      .header("user-agent", ua)
      .json(&json!({ "type": "event", "payload": { "website": website, "url": "/shared" } }))
      .send()
      .await
      .unwrap();

    assert_eq!(response.status(), 200, "{url}: {}", response.text().await.unwrap());
  }

  for target in [&stack.upstream, &stack.rs] {
    let events: i64 = sqlx::query_scalar("select count(*) from website_event where website_id = $1")
      .bind(website)
      .fetch_one(&target.db)
      .await
      .unwrap();
    assert_eq!(events, 1, "{}", target.name);
  }
}
