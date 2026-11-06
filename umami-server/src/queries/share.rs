//! Share links of websites, links, pixels and boards (upstream's `share.ts`).

use chrono::{
  DateTime,
  Utc,
};
use serde_json::{
  Value,
  json,
};
use sqlx::{
  PgPool,
  QueryBuilder,
};
use uuid::Uuid;

use super::{
  Filters,
  now,
  sort_column,
};
use crate::json as js;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Share {
  pub share_id: Uuid,
  pub entity_id: Uuid,
  pub name: String,
  pub share_type: i32,
  pub slug: String,
  pub parameters: Value,
  pub created_at: Option<DateTime<Utc>>,
  pub updated_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "share_id, entity_id, name, share_type, slug, parameters, created_at, updated_at";

impl Share {
  /// The Prisma model as JSON, fields in schema order.
  pub fn to_json(&self) -> Value {
    json!({
        "id": self.share_id,
        "entityId": self.entity_id,
        "name": self.name,
        "shareType": self.share_type,
        "slug": self.slug,
        "parameters": self.parameters,
        "createdAt": self.created_at.map(js::date),
        "updatedAt": self.updated_at.map(js::date),
    })
  }
}

/// Upstream's `getShare`.
pub async fn get(db: &PgPool, share_id: Uuid) -> sqlx::Result<Option<Share>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {COLUMNS} from share where share_id = $1"
  )))
  .bind(share_id)
  .fetch_optional(db)
  .await
}

/// Upstream's `getShareByCode`.
pub async fn by_slug(db: &PgPool, slug: &str) -> sqlx::Result<Option<Share>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {COLUMNS} from share where slug = $1"
  )))
  .bind(slug)
  .fetch_optional(db)
  .await
}

/// Upstream's `getSharesByEntityId`: newest first, paged.
pub async fn list(db: &PgPool, entity_id: Uuid, filters: &Filters) -> sqlx::Result<Value> {
  let mut query = QueryBuilder::new(format!("select {COLUMNS} from share where entity_id = "));
  query.push_bind(entity_id);
  filters.push_order(&mut query, sort_column, "created_at desc", "share_id");
  filters.push_limit(&mut query);
  let shares: Vec<Share> = query.build_query_as().fetch_all(db).await?;

  let count: i64 = sqlx::query_scalar("select count(*) from share where entity_id = $1")
    .bind(entity_id)
    .fetch_one(db)
    .await?;

  Ok(filters.page(shares.iter().map(Share::to_json).collect(), count))
}

/// A new share link (upstream's `createShare`).
pub struct NewShare<'a> {
  pub id: Uuid,
  pub entity_id: Uuid,
  pub share_type: i32,
  pub name: &'a str,
  pub slug: &'a str,
  pub parameters: &'a Value,
}

/// Creates a share link. Fails on a taken slug.
pub async fn create(db: &PgPool, share: &NewShare<'_>) -> sqlx::Result<Share> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "insert into share (share_id, entity_id, name, share_type, slug, parameters, \
         created_at, updated_at) values ($1, $2, $3, $4, $5, $6, $7, $7) returning {COLUMNS}"
  )))
  .bind(share.id)
  .bind(share.entity_id)
  .bind(share.name)
  .bind(share.share_type)
  .bind(share.slug)
  .bind(share.parameters)
  .bind(now())
  .fetch_one(db)
  .await
}

/// Upstream's `updateShare`; `None` if the share does not exist. Fails on a
/// taken slug.
pub async fn update(
  db: &PgPool,
  share_id: Uuid,
  name: &str,
  slug: &str,
  parameters: &Value,
) -> sqlx::Result<Option<Share>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "update share set name = $2, slug = $3, parameters = $4, updated_at = $5 \
         where share_id = $1 returning {COLUMNS}"
  )))
  .bind(share_id)
  .bind(name)
  .bind(slug)
  .bind(parameters)
  .bind(now())
  .fetch_optional(db)
  .await
}

/// Upstream's `deleteShare`; `false` if the share does not exist.
pub async fn delete(db: &PgPool, share_id: Uuid) -> sqlx::Result<bool> {
  let deleted = sqlx::query("delete from share where share_id = $1")
    .bind(share_id)
    .execute(db)
    .await?;

  Ok(deleted.rows_affected() > 0)
}
