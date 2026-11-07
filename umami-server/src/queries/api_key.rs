//! API keys (upstream's `apiKey.ts`). Only a key's hash and display prefix are
//! stored.

use chrono::{
  DateTime,
  Utc,
};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::js;

/// What authenticating with a key needs.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKey {
  pub api_key_id: Uuid,
  pub user_id: Uuid,
  pub name: String,
  pub last_used_at: Option<DateTime<Utc>>,
}

/// A key as listed to its owner (never the key itself).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
  #[sqlx(rename = "api_key_id")]
  pub id: Uuid,
  pub name: String,
  pub key_prefix: String,
  #[serde(serialize_with = "js::serialize_date")]
  pub last_used_at: Option<DateTime<Utc>>,
  #[serde(serialize_with = "js::serialize_date")]
  pub created_at: Option<DateTime<Utc>>,
}

/// Upstream's `getApiKeyByHash`.
pub async fn by_hash(db: &PgPool, key_hash: &str) -> sqlx::Result<Option<ApiKey>> {
  sqlx::query_as("select api_key_id, user_id, name, last_used_at from api_key where key_hash = $1")
    .bind(key_hash)
    .fetch_optional(db)
    .await
}

/// Records that the key was just used.
pub async fn touch(db: &PgPool, api_key_id: Uuid) -> sqlx::Result<()> {
  sqlx::query("update api_key set last_used_at = now() where api_key_id = $1")
    .bind(api_key_id)
    .execute(db)
    .await?;

  Ok(())
}

/// The user's keys, newest first.
pub async fn list(db: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<Summary>> {
  sqlx::query_as(
    "select api_key_id, name, key_prefix, last_used_at, created_at from api_key \
         where user_id = $1 order by created_at desc",
  )
  .bind(user_id)
  .fetch_all(db)
  .await
}

/// Stores a new key by its hash; returns when it was created.
pub async fn create(
  db: &PgPool,
  api_key_id: Uuid,
  user_id: Uuid,
  name: &str,
  key_hash: &str,
  key_prefix: &str,
) -> sqlx::Result<Option<DateTime<Utc>>> {
  sqlx::query_scalar(
    "insert into api_key (api_key_id, user_id, name, key_hash, key_prefix) \
         values ($1, $2, $3, $4, $5) returning created_at",
  )
  .bind(api_key_id)
  .bind(user_id)
  .bind(name)
  .bind(key_hash)
  .bind(key_prefix)
  .fetch_one(db)
  .await
}

/// Deletes one of the user's keys; `false` if they have no such key.
pub async fn delete(db: &PgPool, api_key_id: Uuid, user_id: Uuid) -> sqlx::Result<bool> {
  let deleted = sqlx::query("delete from api_key where api_key_id = $1 and user_id = $2")
    .bind(api_key_id)
    .bind(user_id)
    .execute(db)
    .await?;

  Ok(deleted.rows_affected() > 0)
}
