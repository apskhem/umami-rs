//! Saved reports: the funnels and goals of a website (upstream's `report.ts`).

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
pub struct Report {
  #[serde(rename = "id")]
  pub report_id: Uuid,
  pub user_id: Uuid,
  pub website_id: Uuid,
  #[sqlx(rename = "type")]
  #[serde(rename = "type")]
  pub kind: String,
  pub name: String,
  pub description: String,
  pub parameters: Value,
  #[serde(serialize_with = "js::serialize_date")]
  pub created_at: Option<DateTime<Utc>>,
  #[serde(serialize_with = "js::serialize_date")]
  pub updated_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "report_id, user_id, website_id, type, name, description, parameters, created_at, updated_at";

impl Report {
  /// The Prisma model as JSON, fields in schema order.
  pub fn to_json(&self) -> Value {
    Value::Object(model_json(self))
  }
}

/// Upstream's `getReport`.
pub async fn get(db: &PgPool, report_id: Uuid) -> sqlx::Result<Option<Report>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {COLUMNS} from report where report_id = $1"
  )))
  .bind(report_id)
  .fetch_optional(db)
  .await
}

/// Upstream's `getReports` for a website's reports of one type (on a website
/// that is not deleted): by name, paged, searched by name, description, type,
/// creator and website.
pub async fn list(db: &PgPool, website_id: Uuid, kind: &str, filters: &Filters) -> sqlx::Result<Value> {
  let search = filters.search_pattern();

  let push_where = |builder: &mut QueryBuilder<sqlx::Postgres>| {
    builder
      .push(" where r.website_id = ")
      .push_bind(website_id)
      .push(" and r.type = ")
      .push_bind(kind.to_owned())
      .push(" and exists (select 1 from website w where w.website_id = r.website_id and w.deleted_at is null)");
    if let Some(search) = &search {
      builder
        .push(" and (r.name ilike ")
        .push_bind(search.clone())
        .push(" or r.description ilike ")
        .push_bind(search.clone())
        .push(" or r.type ilike ")
        .push_bind(search.clone())
        .push(r#" or exists (select 1 from "user" u where u.user_id = r.user_id and u.username ilike "#)
        .push_bind(search.clone())
        .push(") or exists (select 1 from website w where w.website_id = r.website_id and (w.name ilike ")
        .push_bind(search.clone())
        .push(" or w.domain ilike ")
        .push_bind(search.clone())
        .push(")))");
    }
  };

  let mut query = QueryBuilder::new(format!("select {COLUMNS} from report r"));
  push_where(&mut query);
  query.push(" order by r.name asc, r.report_id asc");
  filters.push_limit(&mut query);
  let reports: Vec<Report> = query.build_query_as().fetch_all(db).await?;

  let mut count = QueryBuilder::new("select count(*) from report r");
  push_where(&mut count);
  let count: i64 = count.build_query_scalar().fetch_one(db).await?;

  Ok(filters.page(reports.iter().map(Report::to_json).collect(), count))
}

/// A new report (upstream's `createReport`).
pub struct NewReport<'a> {
  pub id: Uuid,
  pub user_id: Uuid,
  pub website_id: Uuid,
  pub kind: &'a str,
  pub name: &'a str,
  pub description: &'a str,
  pub parameters: &'a Value,
}

pub async fn create(db: &PgPool, report: &NewReport<'_>) -> sqlx::Result<Report> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "insert into report (report_id, user_id, website_id, type, name, description, parameters, \
     created_at, updated_at) values ($1, $2, $3, $4, $5, $6, $7, $8, $8) returning {COLUMNS}"
  )))
  .bind(report.id)
  .bind(report.user_id)
  .bind(report.website_id)
  .bind(report.kind)
  .bind(report.name)
  .bind(report.description)
  .bind(report.parameters)
  .bind(now())
  .fetch_one(db)
  .await
}

/// Upstream's `updateReport` of a definition: the name and parameters, and
/// the description if given. `None` if the report does not exist.
pub async fn update(
  db: &PgPool,
  report_id: Uuid,
  name: &str,
  description: Option<&str>,
  parameters: &Value,
) -> sqlx::Result<Option<Report>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "update report set name = $2, description = coalesce($3, description), parameters = $4, \
     updated_at = $5 where report_id = $1 returning {COLUMNS}"
  )))
  .bind(report_id)
  .bind(name)
  .bind(description)
  .bind(parameters)
  .bind(now())
  .fetch_optional(db)
  .await
}

/// Upstream's `deleteReport`; `false` if the report does not exist.
pub async fn delete(db: &PgPool, report_id: Uuid) -> sqlx::Result<bool> {
  let deleted = sqlx::query("delete from report where report_id = $1")
    .bind(report_id)
    .execute(db)
    .await?;

  Ok(deleted.rows_affected() > 0)
}
