//! Annotations: dated notes on a website's charts (upstream's `annotation.ts`).

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
pub struct Annotation {
  #[serde(rename = "id")]
  pub annotation_id: Uuid,
  pub website_id: Uuid,
  pub user_id: Option<Uuid>,
  #[serde(serialize_with = "js::serialize_date")]
  pub date: Option<DateTime<Utc>>,
  pub all_day: bool,
  pub note: String,
  #[serde(serialize_with = "js::serialize_date")]
  pub created_at: Option<DateTime<Utc>>,
  #[serde(serialize_with = "js::serialize_date")]
  pub updated_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "annotation_id, website_id, user_id, date, all_day, note, created_at, updated_at";

impl Annotation {
  /// The Prisma model as JSON, fields in schema order.
  pub fn to_json(&self) -> Value {
    Value::Object(model_json(self))
  }
}

/// An annotation's date, whether it covers the whole day, and its note
/// (upstream's `annotationSchema`).
pub struct Note<'a> {
  pub date: DateTime<Utc>,
  pub all_day: bool,
  pub note: &'a str,
}

/// Upstream's `getWebsiteAnnotation`.
pub async fn get(db: &PgPool, website_id: Uuid, annotation_id: Uuid) -> sqlx::Result<Option<Annotation>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {COLUMNS} from annotation where annotation_id = $1 and website_id = $2"
  )))
  .bind(annotation_id)
  .bind(website_id)
  .fetch_optional(db)
  .await
}

/// Upstream's `getWebsiteAnnotations`: the website's annotations, latest
/// first, within `range` if given, searched by note and paged.
pub async fn list(
  db: &PgPool,
  website_id: Uuid,
  range: Option<(DateTime<Utc>, DateTime<Utc>)>,
  filters: &Filters,
) -> sqlx::Result<Value> {
  let search = filters.search_pattern();

  let push_where = |builder: &mut QueryBuilder<sqlx::Postgres>| {
    builder.push(" where website_id = ").push_bind(website_id);
    if let Some((start, end)) = range {
      builder
        .push(" and date >= ")
        .push_bind(start)
        .push(" and date <= ")
        .push_bind(end);
    }
    if let Some(search) = &search {
      builder.push(" and note ilike ").push_bind(search.clone());
    }
  };

  let mut query = QueryBuilder::new(format!("select {COLUMNS} from annotation"));
  push_where(&mut query);
  query.push(" order by date desc, annotation_id");
  filters.push_limit(&mut query);
  let annotations: Vec<Annotation> = query.build_query_as().fetch_all(db).await?;

  let mut count = QueryBuilder::new("select count(*) from annotation");
  push_where(&mut count);
  let count: i64 = count.build_query_scalar().fetch_one(db).await?;

  Ok(filters.page(annotations.iter().map(Annotation::to_json).collect(), count))
}

/// Upstream's `createAnnotation`.
pub async fn create(
  db: &PgPool,
  id: Uuid,
  website_id: Uuid,
  user_id: Uuid,
  note: &Note<'_>,
) -> sqlx::Result<Annotation> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "insert into annotation (annotation_id, website_id, user_id, date, all_day, note, created_at, updated_at) \
     values ($1, $2, $3, $4, $5, $6, $7, $7) returning {COLUMNS}"
  )))
  .bind(id)
  .bind(website_id)
  .bind(user_id)
  .bind(note.date)
  .bind(note.all_day)
  .bind(note.note)
  .bind(now())
  .fetch_one(db)
  .await
}

/// Upstream's `updateAnnotation`; `None` if the annotation does not exist.
pub async fn update(db: &PgPool, annotation_id: Uuid, note: &Note<'_>) -> sqlx::Result<Option<Annotation>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "update annotation set date = $2, all_day = $3, note = $4, updated_at = $5 \
     where annotation_id = $1 returning {COLUMNS}"
  )))
  .bind(annotation_id)
  .bind(note.date)
  .bind(note.all_day)
  .bind(note.note)
  .bind(now())
  .fetch_optional(db)
  .await
}

/// Upstream's `deleteAnnotation`; `false` if the annotation does not exist.
pub async fn delete(db: &PgPool, annotation_id: Uuid) -> sqlx::Result<bool> {
  let deleted = sqlx::query("delete from annotation where annotation_id = $1")
    .bind(annotation_id)
    .execute(db)
    .await?;

  Ok(deleted.rows_affected() > 0)
}
