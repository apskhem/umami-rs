//! The sparklines of the website, link and pixel lists, ported from upstream's
//! `getWebsiteListCharts`: distinct visitors per 12 hours, and in all.

use std::collections::HashMap;

use chrono::{
  DateTime,
  Utc,
};
use serde_json::{
  Map,
  Value,
  json,
};
use sqlx::PgPool;
use uuid::Uuid;

/// The local start of the 12-hour bucket (upstream's `BUCKET_HOURS`) holding
/// `at`, a `timestamptz`, in the time zone `tz`.
fn bucket(at: &str, tz: &str) -> String {
  format!(
    "date_trunc('day', {at} at time zone {tz}) \
     + floor(extract(hour from {at} at time zone {tz}) / 12) * interval '12 hours'"
  )
}

/// How buckets are labelled (upstream's `yyyy-MM-dd HH:00:00`).
const LABEL: &str = "'YYYY-MM-DD HH24:00:00'";

/// Upstream's default range: the last seven days and today, in `timezone`.
pub async fn default_range(db: &PgPool, timezone: &str) -> sqlx::Result<(DateTime<Utc>, DateTime<Utc>)> {
  sqlx::query_as(
    "select (date_trunc('day', now() at time zone $1) - interval '7 days') at time zone $1, \
     (date_trunc('day', now() at time zone $1) + interval '1 day' - interval '1 millisecond') \
     at time zone $1",
  )
  .bind(timezone)
  .fetch_one(db)
  .await
}

/// Upstream's `getWebsiteListCharts`: for each of `ids`, the distinct sessions
/// with events of `event_type` (or, without one, page views and the like) in
/// each 12 hours of the range, as `values`, and in the whole range, as
/// `total`.
///
/// Upstream labels the buckets from `start`, 12 hours apart, so a start between
/// bucket boundaries (or a daylight saving change) leaves every later bucket
/// empty; here they are the local half-days the range covers.
pub async fn list_charts(
  db: &PgPool,
  ids: &[Uuid],
  (start, end): (DateTime<Utc>, DateTime<Utc>),
  timezone: &str,
  event_type: Option<i32>,
) -> sqlx::Result<Value> {
  if ids.is_empty() {
    return Ok(json!({}));
  }

  let buckets: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
    "select to_char(b, {LABEL}) from generate_series({}, $2 at time zone $3, interval '12 hours') b \
     order by b",
    bucket("$1", "$3")
  )))
  .bind(start)
  .bind(end)
  .bind(timezone)
  .fetch_all(db)
  .await?;

  let points: Vec<(Uuid, Option<String>, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select website_id, x, count(distinct session_id) from ( \
       select website_id, to_char({}, {LABEL}) as x, session_id from website_event \
       where website_id = any($1) \
         and (event_type = $5 or ($5 is null and event_type not in (2, 5))) \
         and created_at between $2 and $3 \
     ) e group by grouping sets ((website_id, x), (website_id))",
    bucket("created_at", "$4")
  )))
  .bind(ids)
  .bind(start)
  .bind(end)
  .bind(timezone)
  .bind(event_type)
  .fetch_all(db)
  .await?;

  let index: HashMap<&str, usize> = buckets.iter().enumerate().map(|(i, b)| (b.as_str(), i)).collect();
  let mut charts: HashMap<Uuid, (Vec<i64>, i64)> = ids.iter().map(|id| (*id, (vec![0; buckets.len()], 0))).collect();

  for (id, x, y) in points {
    let Some((values, total)) = charts.get_mut(&id) else {
      continue;
    };
    // The grouping set without a bucket carries the total.
    match x {
      None => *total = y,
      Some(x) => {
        if let Some(&i) = index.get(x.as_str()) {
          values[i] = y;
        }
      }
    }
  }

  let mut data = Map::new();
  for id in ids {
    let (values, total) = &charts[id];
    data
      .entry(id.to_string())
      .or_insert_with(|| json!({ "values": values, "total": total }));
  }

  Ok(Value::Object(data))
}
