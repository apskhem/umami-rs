//! The realtime view, ported from upstream's `getRealtimeData` and
//! `getRealtimeActivity`.

use std::collections::HashSet;

use chrono::Utc;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::filters::QueryFilters;
use super::queries;
use super::sql::{self, parse_filters};
use crate::error::AppResult;

/// The latest 100 events of the period (ties by event ID).
async fn activity(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Vec<Value>> {
    let p = parse_filters(website_id, filters, false)?;
    let query = format!(
        r#"
    select
        website_event.session_id as "sessionId",
        website_event.event_name as "eventName",
        website_event.created_at as "createdAt",
        session.browser,
        session.os,
        session.device,
        session.country,
        website_event.url_path as "urlPath",
        website_event.referrer_domain as "referrerDomain",
        website_event.hostname
    from website_event
    {cohort}
    inner join session
      on session.session_id = website_event.session_id
        and session.website_id = website_event.website_id
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.event_type != 5
    {filter}
    {date}
    order by website_event.created_at desc, website_event.event_id desc
    limit 100
    "#,
        cohort = p.cohort_query,
        filter = p.filter_query,
        date = p.date_query,
    );

    sql::rows(db, &query, &p.bindings).await
}

/// Adds one to `counts[key]` for non-empty string keys.
fn increment(counts: &mut Map<String, Value>, key: &Value) {
    if let Some(key) = key.as_str().filter(|k| !k.is_empty()) {
        let count = counts.get(key).and_then(Value::as_i64).unwrap_or(0);
        counts.insert(key.to_owned(), json!(count + 1));
    }
}

/// Upstream's `getRealtimeData`: recent events (with a `session` entry before
/// each session's first one), counts by country, page and referrer, the
/// views and visitors series, and totals.
pub async fn data(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Value> {
    let events = activity(db, website_id, filters).await?;
    let views = queries::series(db, website_id, filters, false).await?;
    let visitors = queries::series(db, website_id, filters, true).await?;

    let mut seen = HashSet::new();
    let (mut countries, mut urls, mut referrers) = (Map::new(), Map::new(), Map::new());
    let mut feed = Vec::new();

    // Oldest first, so each session's entry precedes its events once reversed.
    for event in events.iter().rev() {
        let typed = |kind: &str| {
            let mut entry = Map::new();
            entry.insert("__type".to_owned(), json!(kind));
            entry.extend(event.as_object().cloned().unwrap_or_default());
            Value::Object(entry)
        };

        if seen.insert(event["sessionId"].to_string()) {
            increment(&mut countries, &event["country"]);
            feed.push(typed("session"));
        }
        increment(&mut urls, &event["urlPath"]);
        increment(&mut referrers, &event["referrerDomain"]);

        let has_name = event["eventName"].as_str().is_some_and(|n| !n.is_empty());
        feed.push(typed(if has_name { "event" } else { "pageview" }));
    }
    feed.reverse();

    let total = |series: &[Value]| -> i64 { series.iter().filter_map(|p| p["y"].as_i64()).sum() };
    let named_events = events
        .iter()
        .filter(|e| e["eventName"].as_str().is_some_and(|n| !n.is_empty()))
        .count();

    Ok(json!({
        "countries": countries,
        "urls": urls,
        "referrers": referrers,
        "events": feed,
        "series": { "views": views, "visitors": visitors },
        "totals": {
            "views": total(&views),
            "visitors": total(&visitors),
            "events": named_events,
            "countries": countries.len(),
        },
        "timestamp": Utc::now().timestamp_millis(),
    }))
}
