//! Saved session replays (upstream's `sessionReplay.ts`).

use chrono::{
  DateTime,
  Utc,
};
use serde::Serialize;
use serde_json::Value;
use sqlx::{
  PgPool,
  QueryBuilder,
};
use uuid::Uuid;

use super::{
  Filters,
  model_json,
  now,
};
use crate::js;

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedReplay {
  #[serde(rename = "id")]
  pub saved_replay_id: Uuid,
  pub name: String,
  pub website_id: Uuid,
  pub visit_id: Uuid,
  #[serde(serialize_with = "js::serialize_date")]
  pub created_at: Option<DateTime<Utc>>,
  #[serde(serialize_with = "js::serialize_date")]
  pub updated_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "saved_replay_id, name, website_id, visit_id, created_at, updated_at";

/// Upstream's `getSavedReplays`: newest first, searched by name, paged.
pub async fn list(db: &PgPool, website_id: Uuid, filters: &Filters) -> sqlx::Result<Value> {
  let search = filters.search_pattern();
  let push_where = |builder: &mut QueryBuilder<sqlx::Postgres>| {
    builder.push(" where website_id = ").push_bind(website_id);
    if let Some(search) = &search {
      builder.push(" and name ilike ").push_bind(search.clone());
    }
  };

  let mut query = QueryBuilder::new(format!("select {COLUMNS} from session_replay_saved"));
  push_where(&mut query);
  query.push(" order by created_at desc, saved_replay_id");
  filters.push_limit(&mut query);
  let saved: Vec<SavedReplay> = query.build_query_as().fetch_all(db).await?;

  let mut count = QueryBuilder::new("select count(*) from session_replay_saved");
  push_where(&mut count);
  let count: i64 = count.build_query_scalar().fetch_one(db).await?;

  let data = saved.iter().map(|s| Value::Object(model_json(s))).collect();
  Ok(filters.page(data, count))
}

/// Upstream's `getReplaySaved`: whether the visit's replay is saved.
pub async fn is_saved(db: &PgPool, website_id: Uuid, visit_id: Uuid) -> sqlx::Result<bool> {
  sqlx::query_scalar("select exists(select 1 from session_replay_saved where website_id = $1 and visit_id = $2)")
    .bind(website_id)
    .bind(visit_id)
    .fetch_one(db)
    .await
}

/// Upstream's `createReplaySaved`. Saving it again renames it (upstream fails
/// with a 500).
pub async fn save(db: &PgPool, id: Uuid, website_id: Uuid, visit_id: Uuid, name: &str) -> sqlx::Result<()> {
  sqlx::query(
    "insert into session_replay_saved (saved_replay_id, name, website_id, visit_id, created_at, updated_at) \
     values ($1, $2, $3, $4, $5, $5) \
     on conflict (website_id, visit_id) do update set name = excluded.name, updated_at = excluded.updated_at",
  )
  .bind(id)
  .bind(name)
  .bind(website_id)
  .bind(visit_id)
  .bind(now())
  .execute(db)
  .await?;

  Ok(())
}

/// Upstream's `deleteReplaySaved`.
pub async fn unsave(db: &PgPool, website_id: Uuid, visit_id: Uuid) -> sqlx::Result<()> {
  sqlx::query("delete from session_replay_saved where website_id = $1 and visit_id = $2")
    .bind(website_id)
    .bind(visit_id)
    .execute(db)
    .await?;

  Ok(())
}
