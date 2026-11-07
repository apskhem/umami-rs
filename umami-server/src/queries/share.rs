//! Share links of websites, links, pixels and boards (upstream's `share.ts`).

use chrono::{
  DateTime,
  Utc,
};
use serde::Serialize;
use serde_json::{
  Value,
  json,
};
use sqlx::{
  PgExecutor,
  PgPool,
  QueryBuilder,
};
use uuid::Uuid;

use super::website::Website;
use super::{
  Filters,
  model_json,
  now,
  sort_column,
};
use crate::js;
use crate::permissions::EntityType;

/// The 400 for a taken slug (upstream fails with a 500).
pub const SLUG_TAKEN: &str = "That share ID is already taken.";

/// Length of the slugs upstream generates (`getRandomChars(16)`).
pub const SLUG_LENGTH: usize = 16;

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Share {
  #[serde(rename = "id")]
  pub share_id: Uuid,
  pub entity_id: Uuid,
  pub name: String,
  pub share_type: i32,
  pub slug: String,
  pub parameters: Value,
  #[serde(serialize_with = "js::serialize_date")]
  pub created_at: Option<DateTime<Utc>>,
  #[serde(serialize_with = "js::serialize_date")]
  pub updated_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "share_id, entity_id, name, share_type, slug, parameters, created_at, updated_at";

impl Share {
  /// The Prisma model as JSON, fields in schema order.
  pub fn to_json(&self) -> Value {
    Value::Object(model_json(self))
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
  pub kind: EntityType,
  pub name: &'a str,
  pub slug: &'a str,
  pub parameters: Value,
}

impl<'a> NewShare<'a> {
  /// The share link the website routes create for a `shareId`: named after
  /// the website, showing its overview and events.
  pub fn for_website(id: Uuid, website: &'a Website, slug: &'a str) -> Self {
    Self {
      id,
      entity_id: website.website_id,
      kind: EntityType::Website,
      name: &website.name,
      slug,
      parameters: json!({ "overview": true, "events": true }),
    }
  }
}

/// Creates a share link. Fails on a taken slug.
pub async fn create(db: impl PgExecutor<'_>, share: &NewShare<'_>) -> sqlx::Result<Share> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "insert into share (share_id, entity_id, name, share_type, slug, parameters, \
         created_at, updated_at) values ($1, $2, $3, $4, $5, $6, $7, $7) returning {COLUMNS}"
  )))
  .bind(share.id)
  .bind(share.entity_id)
  .bind(share.name)
  .bind(share.kind.code())
  .bind(share.slug)
  .bind(&share.parameters)
  .bind(now())
  .fetch_one(db)
  .await
}

/// The slug of the entity's newest share (upstream's `getShareByEntityId`).
pub async fn latest_slug(db: &PgPool, entity_id: Uuid) -> sqlx::Result<Option<String>> {
  sqlx::query_scalar("select slug from share where entity_id = $1 order by created_at desc limit 1")
    .bind(entity_id)
    .fetch_optional(db)
    .await
}

/// [`latest_slug`] for each of `entity_ids` that has a share.
pub async fn latest_slugs(db: &PgPool, entity_ids: &[Uuid]) -> sqlx::Result<Vec<(Uuid, String)>> {
  sqlx::query_as(
    "select distinct on (entity_id) entity_id, slug from share \
         where entity_id = any($1) order by entity_id, created_at desc",
  )
  .bind(entity_ids)
  .fetch_all(db)
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

/// Upstream's `deleteSharesByEntityId`, for several entities.
pub async fn delete_for_entities(db: impl PgExecutor<'_>, entity_ids: &[Uuid]) -> sqlx::Result<()> {
  sqlx::query("delete from share where entity_id = any($1)")
    .bind(entity_ids)
    .execute(db)
    .await?;

  Ok(())
}

/// Upstream's `deleteShare`; `false` if the share does not exist.
pub async fn delete(db: &PgPool, share_id: Uuid) -> sqlx::Result<bool> {
  let deleted = sqlx::query("delete from share where share_id = $1")
    .bind(share_id)
    .execute(db)
    .await?;

  Ok(deleted.rows_affected() > 0)
}
