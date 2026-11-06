//! Event queries, ported from upstream's `src/queries/sql/events` (Postgres).

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::filters::QueryFilters;
use super::sql::{self, Param, date_sql, parse_filters};
use crate::error::AppResult;

/// Upstream's `getWebsiteEvents`: the events (pageviews and custom events) of
/// the period, newest first, each with its session's location and device and
/// whether it carries data.
pub async fn list(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Value> {
    let mut p = parse_filters(website_id, filters, false)?;
    let search = match filters.search.as_deref().filter(|s| !s.is_empty()) {
        Some(search) => {
            p.bindings
                .set("eventSearch", Param::Text(format!("%{search}%")));
            "and ((event_name ilike {{eventSearch}} and event_type = 2)
           or (url_path ilike {{eventSearch}} and event_type = 1))"
        }
        None => "",
    };

    let events = format!(
        "
    select
      website_event.event_id,
      website_event.created_at
    from website_event
    {cohort}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.event_type != 5
    {date}
    {filter}
    {search}
  ",
        cohort = p.cohort_query,
        join = p.join_session_query,
        date = p.date_query,
        filter = p.filter_query,
    );
    let count = sql::count(db, &events, &p.bindings, filters).await?;

    // Events at the same instant are ordered by ID, so pages never overlap
    // (upstream leaves their order to the database).
    let (size, offset) = sql::page_window(filters);
    let data_date = p.date_query.replace("website_event.", "event_data.");
    let query = format!(
        r#"
    with paged_events as (
      {events}
      order by created_at desc, event_id desc
      limit {size} offset {offset}
    ),
    paged_event_data as (
      select distinct event_data.website_event_id as event_id
      from event_data
      join paged_events on paged_events.event_id = event_data.website_event_id
      where event_data.website_id = {{{{websiteId::uuid}}}}
      {data_date}
    )
    select
      website_event.event_id as "id",
      website_event.website_id as "websiteId",
      website_event.session_id as "sessionId",
      website_event.created_at as "createdAt",
      website_event.hostname,
      website_event.url_path as "urlPath",
      website_event.url_query as "urlQuery",
      website_event.referrer_path as "referrerPath",
      website_event.referrer_query as "referrerQuery",
      website_event.referrer_domain as "referrerDomain",
      session.country as country,
      session.city as city,
      session.device as device,
      session.os as os,
      session.browser as browser,
      website_event.page_title as "pageTitle",
      website_event.event_type as "eventType",
      website_event.event_name as "eventName",
      (paged_event_data.event_id is not null) as "hasData"
    from paged_events
    join website_event on website_event.event_id = paged_events.event_id
    left join session on session.session_id = website_event.session_id
      and session.website_id = website_event.website_id
    left join paged_event_data on paged_event_data.event_id = website_event.event_id
    order by paged_events.created_at desc, paged_events.event_id desc
    "#
    );
    let data = sql::rows(db, &query, &p.bindings).await?;

    Ok(sql::page(data, count, filters))
}

/// Upstream's `getEventStats`: custom events per name and time bucket, for the
/// `limit` most frequent names if given (the most frequent first, ties by name).
pub async fn series(
    db: &PgPool,
    website_id: Uuid,
    limit: Option<i64>,
    filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
    let p = parse_filters(website_id, filters, false)?;
    let limit_query = match limit {
        Some(limit) => format!(
            "and event_name in (
    select event_name
    from website_event
    where website_id = {{{{websiteId::uuid}}}}
      and created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and event_type = 2
    group by event_name
    order by count(*) desc, event_name
    limit {limit}
  )"
        ),
        None => String::new(),
    };

    let query = format!(
        "
    select
      event_name x,
      {date} t,
      count(*) y
    from website_event
    {cohort}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and website_event.event_type = 2
      {filter}
      {limit_query}
    group by 1, 2
    order by 2, 1
    ",
        date = date_sql("website_event.created_at", filters),
        cohort = p.cohort_query,
        join = p.join_session_query,
        filter = p.filter_query,
    );

    sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getWebsiteEventStats`: custom event totals for the period.
pub async fn stats(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Value> {
    let p = parse_filters(website_id, filters, false)?;
    let query = format!(
        r#"
    select
      cast(count(*) as bigint) as "events",
      count(distinct website_event.session_id) as "visitors",
      count(distinct website_event.visit_id) as "visits",
      count(distinct website_event.event_name) as "uniqueEvents"
    from website_event
    {cohort}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and website_event.event_type = 2
      {filter}
    "#,
        cohort = p.cohort_query,
        join = p.join_session_query,
        filter = p.filter_query,
    );

    sql::row(db, &query, &p.bindings).await
}
