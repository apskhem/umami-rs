//! Session queries, ported from upstream's `src/queries/sql/sessions`,
//! `getWeeklyTraffic` and `deleteSession` (Postgres).

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

use super::filters::QueryFilters;
use super::sql::{
  self,
  Bindings,
  Param,
  parse_filters,
  timestamp_diff_sql,
};
use crate::error::AppResult;

/// Upstream's `getWebsiteSessions`: the sessions active in the period, most
/// recently active first.
///
/// Upstream groups by hostname too, so a session seen on two hostnames (say
/// `example.com` and `www.example.com`) is listed and counted twice; here each
/// session is one row, with the hostname of its latest event.
pub async fn list(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Value> {
  let mut p = parse_filters(website_id, filters, false)?;
  let search = match filters.search.as_deref().filter(|s| !s.is_empty()) {
    Some(search) => {
      p.bindings.set("search", Param::Text(format!("%{search}%")));
      "and (distinct_id ilike {{search}}
           or city ilike {{search}}
           or browser ilike {{search}}
           or os ilike {{search}}
           or device ilike {{search}})"
    }
    None => "",
  };

  let query = format!(
    r#"
    select
      session.session_id as "id",
      session.website_id as "websiteId",
      (array_agg(website_event.hostname order by website_event.created_at desc, website_event.event_id desc))[1] as hostname,
      session.browser,
      session.os,
      session.device,
      session.screen,
      session.language,
      session.country,
      session.region,
      session.city,
      min(website_event.created_at) as "firstAt",
      max(website_event.created_at) as "lastAt",
      count(distinct website_event.visit_id) as "visits",
      sum(case when website_event.event_type = 1 then 1 else 0 end) as "views",
      sum(case when website_event.event_type = 2 then 1 else 0 end) as "events",
      max(website_event.created_at) as "createdAt"
    from website_event
    {cohort}
    join session on session.session_id = website_event.session_id
      and session.website_id = website_event.website_id
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.event_type != 5
    {date}
    {filter}
    {search}
    group by session.session_id,
      session.website_id,
      session.browser,
      session.os,
      session.device,
      session.screen,
      session.language,
      session.country,
      session.region,
      session.city
    "#,
    cohort = p.cohort_query,
    date = p.date_query,
    filter = p.filter_query,
  );

  sql::paged(
    db,
    &query,
    &p.bindings,
    filters,
    "max(website_event.created_at) desc, session.session_id",
  )
  .await
}

/// Upstream's `getWebsiteSessionStats`: totals for the period, each as
/// `{ value }`.
pub async fn stats(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Value> {
  let p = parse_filters(website_id, filters, false)?;
  let query = format!(
    r#"
    select
      sum(case when website_event.event_type = 1 then 1 else 0 end) as "pageviews",
      count(distinct website_event.session_id) as "visitors",
      count(distinct website_event.visit_id) as "visits",
      count(distinct session.country) as "countries",
      sum(case when website_event.event_type = 2 then 1 else 0 end) as "events"
    from website_event
    {cohort}
    join session on website_event.session_id = session.session_id
      and website_event.website_id = session.website_id
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and website_event.event_type != 5
      {filter}
    "#,
    cohort = p.cohort_query,
    filter = p.filter_query,
  );

  let row = sql::row(db, &query, &p.bindings).await?;
  let stats: Map<String, Value> = row
    .as_object()
    .into_iter()
    .flatten()
    .map(|(key, value)| (key.clone(), json!({ "value": value.as_i64().unwrap_or(0) })))
    .collect();

  Ok(Value::Object(stats))
}

/// Upstream's `getWeeklyTraffic`: visitors per weekday (Sunday first) and hour,
/// in the request's time zone, as 7 rows of 24.
pub async fn weekly(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Value> {
  let p = parse_filters(website_id, filters, false)?;
  let tz = "{{tz}}";
  let query = format!(
        "
    select
      concat(extract(dow from (website_event.created_at at time zone {tz})), ':', to_char((website_event.created_at at time zone {tz}), 'HH24')) as time,
      count(distinct website_event.session_id) as value
    from website_event
    {cohort}
    {bounce}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and website_event.event_type NOT IN (2, 5)
      {filter}
    group by time
    order by 1
    ",
        cohort = p.cohort_query,
        bounce = p.exclude_bounce_query,
        join = p.join_session_query,
        filter = p.filter_query,
    );
  let rows = sql::rows(db, &query, &p.bindings).await?;

  let days: Vec<Vec<i64>> = (0..7)
    .map(|day| {
      (0..24)
        .map(|hour| {
          let time = format!("{day}:{hour:02}");
          rows
            .iter()
            .find(|row| row["time"] == time.as_str())
            .and_then(|row| row["value"].as_i64())
            .unwrap_or(0)
        })
        .collect()
    })
    .collect();

  Ok(json!(days))
}

fn ids(website_id: Uuid, session_id: Uuid) -> Bindings {
  let mut bindings = Bindings::default();
  bindings.set("websiteId", Param::Uuid(website_id));
  bindings.set("sessionId", Param::Uuid(session_id));
  bindings
}

/// Upstream's `getWebsiteSession`: the session's attributes and activity
/// totals, or `None` if it has no events.
pub async fn get(db: &PgPool, website_id: Uuid, session_id: Uuid) -> AppResult<Option<Value>> {
  let totaltime = timestamp_diff_sql("min_time", "max_time");
  let query = format!(
    r#"
    select id,
      distinct_id as "distinctId",
      website_id as "websiteId",
      browser,
      os,
      device,
      screen,
      language,
      country,
      region,
      city,
      min(min_time) as "firstAt",
      max(max_time) as "lastAt",
      count(distinct visit_id) as visits,
      sum(views) as views,
      sum(events) as events,
      sum({totaltime}) as "totaltime"
    from (select
          session.session_id as id,
          session.distinct_id,
          website_event.visit_id,
          session.website_id,
          session.browser,
          session.os,
          session.device,
          session.screen,
          session.language,
          session.country,
          session.region,
          session.city,
          min(website_event.created_at) as min_time,
          max(website_event.created_at) as max_time,
          sum(case when website_event.event_type = 1 then 1 else 0 end) as views,
          sum(case when website_event.event_type = 2 then 1 else 0 end) as events
    from session
    join website_event on website_event.session_id = session.session_id
    where session.website_id = {{{{websiteId::uuid}}}}
      and session.session_id = {{{{sessionId::uuid}}}}
      and website_event.event_type != 5
    group by session.session_id, session.distinct_id, visit_id, session.website_id, session.browser, session.os, session.device, session.screen, session.language, session.country, session.region, session.city) t
    group by id, distinct_id, website_id, browser, os, device, screen, language, country, region, city
    "#
  );

  let row = sql::row(db, &query, &ids(website_id, session_id)).await?;
  Ok((!row.is_null()).then_some(row))
}

/// Upstream's `getLinkedDistinctIds`: the identities the session was linked to.
pub async fn linked_distinct_ids(db: &PgPool, website_id: Uuid, session_id: Uuid) -> AppResult<Vec<String>> {
  Ok(
    sqlx::query_scalar(
      "select distinct distinct_id from session_link \
         where website_id = $1 and session_id = $2 order by distinct_id",
    )
    .bind(website_id)
    .bind(session_id)
    .fetch_all(db)
    .await?,
  )
}

/// Upstream's `getLinkedSessionIds`: the sessions linked to an identity, each
/// with when it was first linked.
pub async fn linked_sessions(
  db: &PgPool,
  website_id: Uuid,
  distinct_id: &str,
) -> AppResult<Vec<(Uuid, Option<DateTime<Utc>>)>> {
  Ok(
    sqlx::query_as(
      "select session_id, min(created_at) from session_link \
         where website_id = $1 and distinct_id = $2 group by session_id order by session_id",
    )
    .bind(website_id)
    .bind(distinct_id)
    .fetch_all(db)
    .await?,
  )
}

/// Upstream's `getSessionActivity`: the sessions' latest 500 events in the
/// period (ties by event ID, which upstream leaves to the database).
pub async fn activity(
  db: &PgPool,
  website_id: Uuid,
  session_ids: &[Uuid],
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let mut bindings = Bindings::default();
  bindings.set("websiteId", Param::Uuid(website_id));
  bindings.set(
    "sessionIds",
    Param::Texts(session_ids.iter().map(Uuid::to_string).collect()),
  );
  bindings.set("startDate", Param::Time(filters.start_date));
  bindings.set("endDate", Param::Time(filters.end_date));

  sql::rows(
    db,
    r#"
    select
      created_at as "createdAt",
      url_path as "urlPath",
      url_query as "urlQuery",
      referrer_domain as "referrerDomain",
      event_id as "eventId",
      event_type as "eventType",
      event_name as "eventName",
      visit_id as "visitId",
      hostname,
      EXISTS (select 1
              from event_data d
              where d.website_event_id = e.event_id
                and d.website_id = {{websiteId::uuid}}
                and d.created_at between {{startDate}} and {{endDate}}) AS "hasData"
    from website_event e
    where website_id = {{websiteId::uuid}}
      and session_id = any({{sessionIds}}::uuid[])
      and event_type != 5
      and created_at between {{startDate}} and {{endDate}}
    order by created_at desc, event_id desc
    limit 500
    "#,
    &bindings,
  )
  .await
}

/// Upstream's `getSessionData`: the session's stored properties.
pub async fn data(db: &PgPool, website_id: Uuid, session_id: Uuid) -> AppResult<Vec<Value>> {
  sql::rows(
    db,
    r#"
    select
        website_id as "websiteId",
        session_id as "sessionId",
        data_key as "dataKey",
        data_type as "dataType",
        replace(string_value, '.0000', '') as "stringValue",
        number_value as "numberValue",
        date_value as "dateValue",
        created_at as "createdAt"
    from session_data
    where website_id = {{websiteId::uuid}}
      and session_id = {{sessionId::uuid}}
    order by data_key asc, session_data_id
    "#,
    &ids(website_id, session_id),
  )
  .await
}

/// Upstream's `deleteSession`: the session and everything recorded in it.
/// `false` if the website has no such session.
pub async fn delete(db: &PgPool, website_id: Uuid, session_id: Uuid) -> AppResult<bool> {
  let mut tx = db.begin().await?;

  let exists: bool =
    sqlx::query_scalar("select exists(select 1 from session where session_id = $1 and website_id = $2)")
      .bind(session_id)
      .bind(website_id)
      .fetch_one(&mut *tx)
      .await?;
  if !exists {
    return Ok(false);
  }

  let statements = [
    "delete from event_data where website_event_id in \
           (select event_id from website_event where website_id = $1 and session_id = $2)",
    "delete from session_replay_saved where website_id = $1 and visit_id in \
           (select visit_id from website_event where website_id = $1 and session_id = $2 \
            union select visit_id from session_replay where website_id = $1 and session_id = $2)",
    "delete from session_replay where website_id = $1 and session_id = $2",
    "delete from heatmap_event where website_id = $1 and session_id = $2",
    "delete from revenue where website_id = $1 and session_id = $2",
    "delete from session_data where website_id = $1 and session_id = $2",
    "delete from session_link where website_id = $1 and session_id = $2",
    "delete from website_event where website_id = $1 and session_id = $2",
    "delete from session where website_id = $1 and session_id = $2",
  ];
  for statement in statements {
    sqlx::query(statement)
      .bind(website_id)
      .bind(session_id)
      .execute(&mut *tx)
      .await?;
  }
  tx.commit().await?;

  Ok(true)
}
