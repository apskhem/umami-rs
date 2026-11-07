//! Saved segments and cohorts of a website (upstream's `segment.ts`).

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
  sort_column,
};
use crate::js;

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
  #[serde(rename = "id")]
  pub segment_id: Uuid,
  pub website_id: Uuid,
  #[sqlx(rename = "type")]
  #[serde(rename = "type")]
  pub kind: String,
  pub name: String,
  pub parameters: Value,
  #[serde(serialize_with = "js::serialize_date")]
  pub created_at: Option<DateTime<Utc>>,
  #[serde(serialize_with = "js::serialize_date")]
  pub updated_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "segment_id, website_id, type, name, parameters, created_at, updated_at";

impl Segment {
  /// The Prisma model as JSON, fields in schema order.
  pub fn to_json(&self) -> Value {
    Value::Object(model_json(self))
  }
}

/// Upstream's `getWebsiteSegment`.
pub async fn get(db: &PgPool, website_id: Uuid, segment_id: Uuid) -> sqlx::Result<Option<Segment>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {COLUMNS} from segment where segment_id = $1 and website_id = $2"
  )))
  .bind(segment_id)
  .bind(website_id)
  .fetch_optional(db)
  .await
}

/// Upstream's `getWebsiteSegments`: the website's segments or cohorts (`kind`),
/// paged and searched by name. Upstream leaves them unordered; here they are
/// oldest first.
pub async fn list(db: &PgPool, website_id: Uuid, kind: &str, filters: &Filters) -> sqlx::Result<Value> {
  let search = filters.search_pattern();
  let push_where = |builder: &mut QueryBuilder<sqlx::Postgres>| {
    builder
      .push(" where website_id = ")
      .push_bind(website_id)
      .push(" and type = ")
      .push_bind(kind.to_owned());
    if let Some(search) = &search {
      builder.push(" and name ilike ").push_bind(search.clone());
    }
  };

  let mut query = QueryBuilder::new(format!("select {COLUMNS} from segment"));
  push_where(&mut query);
  filters.push_order(&mut query, sort_column, "created_at asc", "segment_id");
  filters.push_limit(&mut query);
  let segments: Vec<Segment> = query.build_query_as().fetch_all(db).await?;

  let mut count = QueryBuilder::new("select count(*) from segment");
  push_where(&mut count);
  let count: i64 = count.build_query_scalar().fetch_one(db).await?;

  Ok(filters.page(segments.iter().map(Segment::to_json).collect(), count))
}

/// Upstream's `createSegment`.
pub async fn create(
  db: &PgPool,
  segment_id: Uuid,
  website_id: Uuid,
  kind: &str,
  name: &str,
  parameters: &Value,
) -> sqlx::Result<Segment> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "insert into segment (segment_id, website_id, type, name, parameters, created_at, updated_at) \
         values ($1, $2, $3, $4, $5, $6, $6) returning {COLUMNS}"
  )))
  .bind(segment_id)
  .bind(website_id)
  .bind(kind)
  .bind(name)
  .bind(parameters)
  .bind(now())
  .fetch_one(db)
  .await
}

/// Upstream's `updateSegment`; `None` if the segment does not exist.
pub async fn update(
  db: &PgPool,
  segment_id: Uuid,
  kind: &str,
  name: &str,
  parameters: &Value,
) -> sqlx::Result<Option<Segment>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "update segment set type = $2, name = $3, parameters = $4, updated_at = $5 \
         where segment_id = $1 returning {COLUMNS}"
  )))
  .bind(segment_id)
  .bind(kind)
  .bind(name)
  .bind(parameters)
  .bind(now())
  .fetch_optional(db)
  .await
}

/// Upstream's `deleteSegment`; `false` if the segment does not exist.
pub async fn delete(db: &PgPool, segment_id: Uuid) -> sqlx::Result<bool> {
  let deleted = sqlx::query("delete from segment where segment_id = $1")
    .bind(segment_id)
    .execute(db)
    .await?;

  Ok(deleted.rows_affected() > 0)
}
