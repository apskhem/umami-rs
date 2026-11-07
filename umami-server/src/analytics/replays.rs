//! Session replays and heatmaps, ported from upstream's `getSessionReplays`,
//! `getReplayChunks` and `getHeatmap`, with the replay helpers of
//! `src/lib/replay.ts`.

use std::io::Read as _;

use chrono::{
  DateTime,
  Utc,
};
use flate2::read::GzDecoder;
use serde_json::{
  Value,
  json,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::filters::{
  Operator,
  QueryFilters,
  Values,
};
use super::sql::{
  self,
  Param,
  parse_filters,
};
use crate::collect::record::is_fragment;
use crate::error::AppResult;
use crate::queries::website;

/// Upstream's `getSessionReplays`: the recorded visits (of one session, if
/// given), most recently recorded first, paged; searchable by the session's
/// identity, city, browser, OS and device, and optionally only those at least
/// `min_duration_ms` long.
pub async fn list(
  db: &PgPool,
  website_id: Uuid,
  filters: &QueryFilters,
  session_id: Option<Uuid>,
  min_duration_ms: Option<i64>,
) -> AppResult<Value> {
  let mut p = parse_filters(website_id, filters, false)?;

  let join = if p.filter_query.is_empty() && p.cohort_query.is_empty() {
    String::new()
  } else {
    format!(
      "join (select distinct website_event.website_id, website_event.session_id, website_event.visit_id
               from website_event
               {join}
               {cohort}
               where website_event.website_id = {{{{websiteId::uuid}}}}
                  and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
                  {filter}) website_event
        on website_event.website_id = sr.website_id
          and website_event.session_id = sr.session_id
          and website_event.visit_id = sr.visit_id",
      join = p.join_session_query,
      cohort = p.cohort_query,
      filter = p.filter_query,
    )
  };
  let session = match session_id {
    Some(session_id) => {
      p.bindings.set("sessionId", Param::Uuid(session_id));
      "and sr.session_id = {{sessionId::uuid}}"
    }
    None => "",
  };
  let search = match filters.search.as_deref().filter(|s| !s.is_empty()) {
    Some(search) => {
      p.bindings.set("search", Param::Text(format!("%{search}%")));
      "and (session.distinct_id ilike {{search}}
           or session.city ilike {{search}}
           or session.browser ilike {{search}}
           or session.os ilike {{search}}
           or session.device ilike {{search}})"
    }
    None => "",
  };
  let having = match min_duration_ms {
    Some(ms) => {
      p.bindings.set("minDurationMs", Param::Number(ms as f64));
      "having sum(extract(epoch from sr.ended_at - sr.started_at) * 1000) >= {{minDurationMs::numeric}}"
    }
    None => "",
  };

  let query = format!(
    r#"
    select
      sr.visit_id as "id",
      sr.session_id as "sessionId",
      sr.website_id as "websiteId",
      session.browser,
      session.os,
      session.device,
      session.country,
      session.city,
      sum(sr.event_count) as "eventCount",
      count(sr.replay_id) as "chunkCount",
      min(sr.started_at) as "startedAt",
      max(sr.ended_at) as "endedAt",
      sum(extract(epoch from sr.ended_at - sr.started_at) * 1000)::bigint as "duration",
      max(sr.created_at) as "createdAt"
    from session_replay sr
    join session on session.session_id = sr.session_id
      and session.website_id = sr.website_id
    {join}
    where sr.website_id = {{{{websiteId::uuid}}}}
      and sr.created_at between {{{{startDate}}}} and {{{{endDate}}}}
    {session}
    {search}
    group by sr.visit_id,
      sr.session_id,
      sr.website_id,
      session.browser,
      session.os,
      session.device,
      session.country,
      session.city
    {having}
    order by max(sr.created_at) desc, sr.visit_id, sr.session_id
    "#
  );

  sql::paged_as_ordered(db, &query, &p.bindings, filters).await
}

/// A stored chunk of a recording.
pub struct Chunk {
  pub session_id: Uuid,
  pub chunk_index: i32,
  pub events: Vec<Value>,
  pub started_at: DateTime<Utc>,
  pub ended_at: DateTime<Utc>,
}

/// Upstream's `getReplayChunks`: a visit's recording, in chunk order, up to the
/// chunk starting at `end_at` and the chunk numbered `end_chunk`.
pub async fn chunks(
  db: &PgPool,
  website_id: Uuid,
  visit_id: Uuid,
  end_at: Option<DateTime<Utc>>,
  end_chunk: Option<i64>,
) -> AppResult<Vec<Chunk>> {
  #[derive(sqlx::FromRow)]
  struct Row {
    session_id: Uuid,
    chunk_index: i32,
    events: Vec<u8>,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
  }

  let rows: Vec<Row> = sqlx::query_as(
    "select session_id, chunk_index, events, started_at, ended_at from session_replay \
     where website_id = $1 and visit_id = $2 \
       and ($3::timestamptz is null or started_at <= $3) \
       and ($4::bigint is null or chunk_index <= $4) \
     order by chunk_index asc, created_at asc, replay_id asc",
  )
  .bind(website_id)
  .bind(visit_id)
  .bind(end_at)
  .bind(end_chunk)
  .fetch_all(db)
  .await?;

  rows
    .into_iter()
    .map(|row| {
      let mut text = Vec::new();
      GzDecoder::new(row.events.as_slice())
        .read_to_end(&mut text)
        .map_err(anyhow::Error::from)?;
      let events = match serde_json::from_slice(&text).map_err(anyhow::Error::from)? {
        Value::Array(events) => events,
        other => vec![other],
      };
      Ok(Chunk {
        session_id: row.session_id,
        chunk_index: row.chunk_index,
        events,
        started_at: row.started_at,
        ended_at: row.ended_at,
      })
    })
    .collect()
}

/// Where to stop replaying: a time (milliseconds), and a chunk and event.
#[derive(Default)]
pub struct Until {
  pub time: Option<i64>,
  pub chunk: Option<i64>,
  pub event: Option<i64>,
}

/// An event's timestamp, if it is a number (`Number(event.timestamp)`).
fn event_time(event: &Value) -> Option<f64> {
  // A missing timestamp is `undefined`, which is not a number (`null` is 0).
  let number = match event.get("timestamp")? {
    Value::Number(n) => n.as_f64()?,
    Value::String(s) => crate::js::string_to_number(s),
    Value::Bool(b) => f64::from(u8::from(*b)),
    Value::Null => 0.0,
    _ => return None,
  };
  number.is_finite().then_some(number)
}

/// Upstream's `mergeReplayEvents`: the chunks' events up to the stopping
/// point, sorted by time if they arrived out of order.
pub fn merge(chunks: &[Chunk], until: &Until) -> Vec<Value> {
  let mut events = Vec::new();
  let mut sorted = true;
  let mut last = f64::NEG_INFINITY;

  for chunk in chunks {
    let index = i64::from(chunk.chunk_index);
    if until.chunk.is_some_and(|end| index > end) {
      continue;
    }
    for (event_index, event) in chunk.events.iter().enumerate() {
      if until.chunk == Some(index) && until.event.is_some_and(|end| event_index as i64 > end) {
        continue;
      }
      let time = event_time(event);
      if let (Some(end), Some(time)) = (until.time, time)
        && time > end as f64
      {
        continue;
      }
      if let Some(time) = time {
        if time < last {
          sorted = false;
        } else {
          last = time;
        }
      }
      events.push(event.clone());
    }
  }

  if !sorted {
    // A stable sort, as JavaScript's.
    events.sort_by(|a, b| {
      let (a, b) = (event_time(a).unwrap_or(0.0), event_time(b).unwrap_or(0.0));
      a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
    });
  }

  events
}

/// Upstream's `restoreReplayEventFragments`: fragmented events reassembled
/// where all their pieces arrived (malformed ones dropped).
pub fn restore_fragments(events: Vec<Value>) -> Vec<Value> {
  struct Pending {
    total: i64,
    values: Vec<(i64, String)>,
  }
  let mut restored = Vec::new();
  let mut pending: Vec<(String, Pending)> = Vec::new();

  for event in events {
    if !is_fragment(&event) {
      restored.push(event);
      continue;
    }
    let data = &event["data"];
    let id = data["id"].as_str().unwrap_or_default().to_owned();
    let index = data["index"].as_f64().unwrap_or_default() as i64;
    let total = data["total"].as_f64().unwrap_or_default() as i64;
    let value = data["value"].as_str().unwrap_or_default().to_owned();

    let position = match pending.iter().position(|(key, _)| *key == id) {
      Some(position) if pending[position].1.total == total => position,
      Some(position) => {
        pending[position].1 = Pending {
          total,
          values: Vec::new(),
        };
        position
      }
      None => {
        pending.push((
          id,
          Pending {
            total,
            values: Vec::new(),
          },
        ));
        pending.len() - 1
      }
    };
    let fragment = &mut pending[position].1;
    if !fragment.values.iter().any(|(i, _)| *i == index) {
      fragment.values.push((index, value));
    }
    if fragment.values.len() as i64 == fragment.total {
      let (_, fragment) = pending.remove(position);
      let serialized: String = (0..fragment.total)
        .map(|i| {
          fragment
            .values
            .iter()
            .find(|(index, _)| *index == i)
            .map_or("", |(_, v)| v.as_str())
        })
        .collect();
      if let Ok(event) = serde_json::from_str(&serialized) {
        restored.push(event);
      }
    }
  }

  restored
}

// Heatmaps

/// Upstream's `HEATMAP_EVENT_TYPE`.
const CLICK: i32 = 1;
const SCROLL: i32 = 2;
/// Upstream's limits.
const POINT_LIMIT: i64 = 5000;
const PAGE_LIMIT: i64 = 100;

/// The visits matching the filters (other than `path`), as a join, and the
/// `path` filters on the heatmap events' own paths (upstream's
/// `getRelationalHeatmapFilterContext`).
fn heatmap_filters(website_id: Uuid, filters: &QueryFilters) -> AppResult<(String, String, sql::Bindings)> {
  let paths: Vec<_> = filters
    .column_filters()?
    .into_iter()
    .filter(|filter| filter.name == "path")
    .collect();
  let mut p = parse_filters(website_id, &filters.without("path"), false)?;

  let mut clauses = Vec::new();
  for filter in &paths {
    let param = format!("heatmap_{}", filter.key);
    let (clause, value) = match (&filter.operator, &filter.values) {
      (Operator::Equals, Values::List(values)) => ("h.url_path = ANY", Param::Texts(values.clone())),
      (Operator::NotEquals, Values::List(values)) => ("h.url_path != ALL", Param::Texts(values.clone())),
      (Operator::Contains, Values::One(value)) => ("h.url_path ilike ", Param::Text(format!("%{value}%"))),
      (Operator::DoesNotContain, Values::One(value)) => ("h.url_path not ilike ", Param::Text(format!("%{value}%"))),
      (Operator::Regex, Values::One(value)) => ("h.url_path ~* ", Param::Text(value.clone())),
      (Operator::NotRegex, Values::One(value)) => ("h.url_path !~* ", Param::Text(value.clone())),
      _ => continue,
    };
    p.bindings.set(&param, value);
    clauses.push(if clause.ends_with("ANY") || clause.ends_with("ALL") {
      format!("{clause}({{{{{param}}}}})")
    } else {
      format!("{clause}{{{{{param}}}}}")
    });
  }
  let path_query = match (clauses.is_empty(), filters.match_any) {
    (true, _) => String::new(),
    (false, true) => format!("and (\n        {}\n      )", clauses.join("\n        or ")),
    (false, false) => format!("and {}", clauses.join("\n      and ")),
  };

  let join = if p.filter_query.is_empty() && p.cohort_query.is_empty() && p.exclude_bounce_query.is_empty() {
    String::new()
  } else {
    format!(
      "
    inner join (
      select distinct website_event.website_id, website_event.session_id, website_event.visit_id
      from website_event
      {join}
      {cohort}
      {bounce}
      where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        {filter}
    ) filtered_visits
      on filtered_visits.website_id = h.website_id
      and filtered_visits.session_id = h.session_id
      and filtered_visits.visit_id = h.visit_id
    ",
      join = p.join_session_query,
      cohort = p.cohort_query,
      bounce = p.exclude_bounce_query,
      filter = p.filter_query,
    )
  };

  Ok((join, path_query, p.bindings))
}

/// The most used viewport among weighted samples (the first of equals wins),
/// with the largest page seen at it (upstream's `pickSnapshotViewport`).
fn pick_viewport(samples: &[(i64, i64, i64, i64, i64)]) -> Option<(i64, i64, i64, i64)> {
  // (viewportW, viewportH, weight, maxPageW, maxPageH), in first-seen order.
  let mut viewports: Vec<(i64, i64, i64, i64, i64)> = Vec::new();
  for &(width, height, page_w, page_h, weight) in samples {
    match viewports.iter_mut().find(|v| v.0 == width && v.1 == height) {
      Some(v) => {
        v.2 += weight;
        v.3 = v.3.max(page_w);
        v.4 = v.4.max(page_h);
      }
      None => viewports.push((width, height, weight, page_w, page_h)),
    }
  }
  let mut best: Option<(i64, i64, i64, i64, i64)> = None;
  for viewport in viewports {
    if best.is_none_or(|b| viewport.2 > b.2) {
      best = Some(viewport);
    }
  }
  best.map(|(width, height, _, page_w, page_h)| (width, height, page_w, page_h))
}

/// Upstream's `buildHeatmapPageUrl`: the page on the website's first domain.
fn page_url(domain: Option<&str>, url_path: &str) -> Option<String> {
  let host = domain?.split(',').next()?.trim();
  if host.is_empty() {
    return None;
  }
  let origin = if host.starts_with("http://") || host.starts_with("https://") {
    url::Url::parse(host).ok()?
  } else {
    let local = ["localhost", "127.0.0.1", "[::1]"].iter().any(|l| host.starts_with(l));
    url::Url::parse(&format!("{}://{host}", if local { "http" } else { "https" })).ok()?
  };
  let path = if url_path.is_empty() { "/" } else { url_path };
  Some(origin.join(path).ok()?.to_string())
}

/// Upstream's `getIframeSnapshot`: the page to show under the heatmap, at the
/// chosen viewport.
async fn snapshot(
  db: &PgPool,
  website_id: Uuid,
  url_path: &str,
  viewport: Option<(i64, i64, i64, i64)>,
) -> AppResult<Value> {
  let Some((width, height, page_w, page_h)) = viewport.filter(|v| v.0 != 0 && v.2 != 0 && v.3 != 0) else {
    return Ok(Value::Null);
  };
  let found = website::find(db, website_id).await?;
  let Some(url) = page_url(found.as_ref().and_then(|w| w.domain.as_deref()), url_path) else {
    return Ok(Value::Null);
  };
  let height = if height != 0 { height } else { page_h.clamp(640, 1080) };

  Ok(json!({
      "kind": "iframe",
      "id": format!("iframe:{website_id}:{url_path}:{width}x{height}"),
      "url": url,
      "pageW": page_w,
      "pageH": page_h,
      "viewportW": width,
      "viewportH": height,
  }))
}

fn empty_scroll() -> Value {
  json!({
      "buckets": [],
      "totalSessions": 0,
      "pageW": null,
      "pageH": null,
      "viewportW": null,
      "viewportH": null,
  })
}

/// Upstream's `getHeatmap`: the pages with clicks (or scrolls), and for one
/// page its click points or scroll depths, with a snapshot of the page.
pub async fn heatmap(
  db: &PgPool,
  website_id: Uuid,
  filters: &QueryFilters,
  url_path: Option<&str>,
  scroll: bool,
) -> AppResult<Value> {
  let mode = if scroll { "scroll" } else { "click" };
  let (join, path_query, mut bindings) = heatmap_filters(website_id, filters)?;
  bindings.set(
    "eventType",
    Param::Number(f64::from(if scroll { SCROLL } else { CLICK })),
  );
  let complete = if scroll {
    "and h.scroll_pct is not null"
  } else {
    "and h.x is not null
      and h.y is not null
      and h.page_x is not null
      and h.page_y is not null"
  };
  let measured = "and h.page_w is not null
      and h.page_h is not null
      and h.viewport_w is not null
      and h.viewport_h is not null";

  let pages = sql::rows(
    db,
    &format!(
      r#"
    select
      h.url_path as "urlPath",
      count(*)::int as count,
      count(distinct h.visit_id)::int as sessions
    from heatmap_event h
    {join}
    where h.website_id = {{{{websiteId::uuid}}}}
      and h.event_type = {{{{eventType::int}}}}
      and h.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      {path_query}
      {complete}
      {measured}
    group by h.url_path
    order by sessions desc, count desc, h.url_path
    limit {PAGE_LIMIT}
    "#
    ),
    &bindings,
  )
  .await?;

  let Some(url_path) = url_path.filter(|path| !path.is_empty()) else {
    return Ok(json!({ "mode": mode, "pages": pages, "points": [], "snapshot": null, "scroll": empty_scroll() }));
  };
  bindings.set("urlPath", Param::Text(url_path.to_owned()));
  let int = |row: &Value, key: &str| row[key].as_i64().unwrap_or_default();

  if scroll {
    let rows = sql::rows(
      db,
      &format!(
        r#"
      select
        (floor(max_pct / 10) * 10)::int as depth,
        count(*)::int as sessions,
        page_w::int as "pageW",
        page_h::int as "pageH",
        viewport_w::int as "viewportW",
        viewport_h::int as "viewportH"
      from (
        select
          h.visit_id,
          max(h.scroll_pct) as max_pct,
          (mode() within group (order by h.page_w))::int as page_w,
          (mode() within group (order by h.page_h))::int as page_h,
          (mode() within group (order by h.viewport_w))::int as viewport_w,
          (mode() within group (order by h.viewport_h))::int as viewport_h
        from heatmap_event h
        {join}
        where h.website_id = {{{{websiteId::uuid}}}}
          and h.event_type = {{{{eventType::int}}}}
          and h.url_path = {{{{urlPath}}}}
          and h.created_at between {{{{startDate}}}} and {{{{endDate}}}}
          {path_query}
          {complete}
          {measured}
        group by h.visit_id
      ) per_session
      group by depth, page_w, page_h, viewport_w, viewport_h
      order by depth, sessions desc, page_w, page_h, viewport_w, viewport_h
      "#
      ),
      &bindings,
    )
    .await?;

    let samples: Vec<_> = rows
      .iter()
      .map(|r| {
        (
          int(r, "viewportW"),
          int(r, "viewportH"),
          int(r, "pageW"),
          int(r, "pageH"),
          int(r, "sessions"),
        )
      })
      .collect();
    let viewport = pick_viewport(&samples);
    let total: i64 = rows.iter().map(|r| int(r, "sessions")).sum();
    let snapshot = snapshot(db, website_id, url_path, viewport).await?;

    return Ok(json!({
        "mode": mode,
        "pages": pages,
        "points": [],
        "snapshot": snapshot,
        "scroll": {
            "buckets": rows,
            "totalSessions": total,
            "pageW": viewport.map(|v| v.2),
            "pageH": viewport.map(|v| v.3),
            "viewportW": viewport.map(|v| v.0),
            "viewportH": viewport.map(|v| v.1),
        },
    }));
  }

  let points = sql::rows(
    db,
    &format!(
      r#"
    select
      h.x,
      h.y,
      h.page_x as "pageX",
      h.page_y as "pageY",
      h.page_w as "pageW",
      h.page_h as "pageH",
      h.viewport_w as "viewportW",
      h.viewport_h as "viewportH",
      count(*)::int as count
    from heatmap_event h
    {join}
    where h.website_id = {{{{websiteId::uuid}}}}
      and h.event_type = {{{{eventType::int}}}}
      and h.url_path = {{{{urlPath}}}}
      and h.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      {path_query}
      {complete}
      {measured}
    group by h.x, h.y, h.page_x, h.page_y, h.page_w, h.page_h, h.viewport_w, h.viewport_h
    order by count desc, h.x, h.y, h.page_x, h.page_y, h.page_w, h.page_h, h.viewport_w, h.viewport_h
    limit {POINT_LIMIT}
    "#
    ),
    &bindings,
  )
  .await?;

  let samples: Vec<_> = points
    .iter()
    .map(|p| {
      (
        int(p, "viewportW"),
        int(p, "viewportH"),
        int(p, "pageW"),
        int(p, "pageH"),
        int(p, "count"),
      )
    })
    .collect();
  let snapshot = snapshot(db, website_id, url_path, pick_viewport(&samples)).await?;

  Ok(json!({ "mode": mode, "pages": pages, "points": points, "snapshot": snapshot, "scroll": empty_scroll() }))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::collect::record::FRAGMENT_TYPE;

  fn chunk(index: i32, events: Value) -> Chunk {
    Chunk {
      session_id: Uuid::nil(),
      chunk_index: index,
      events: events.as_array().cloned().unwrap_or_default(),
      started_at: DateTime::UNIX_EPOCH,
      ended_at: DateTime::UNIX_EPOCH,
    }
  }

  #[test]
  fn merges_chunks_like_upstream() {
    let chunks = [
      chunk(1, json!([{ "timestamp": 10 }, { "timestamp": 30 }])),
      chunk(2, json!([{ "timestamp": 20 }, { "type": 3 }, { "timestamp": 40 }])),
    ];
    let times = |events: Vec<Value>| events.iter().map(|e| e["timestamp"].clone()).collect::<Vec<_>>();

    // Out of order: sorted, untimed events counting as 0.
    assert_eq!(
      times(merge(&chunks, &Until::default())),
      [json!(null), json!(10), json!(20), json!(30), json!(40)]
    );
    let until = Until {
      time: Some(25),
      ..Until::default()
    };
    // In order once cut off: untimed events don't trigger a sort.
    assert_eq!(times(merge(&chunks, &until)), [json!(10), json!(20), json!(null)]);
    let until = Until {
      chunk: Some(2),
      event: Some(0),
      ..Until::default()
    };
    assert_eq!(times(merge(&chunks, &until)), [json!(10), json!(20), json!(30)]);
  }

  #[test]
  fn restores_fragments() {
    let piece = |id: &str, index: i64, total: i64, value: &str| json!({ "type": FRAGMENT_TYPE, "data": { "id": id, "index": index, "total": total, "value": value } });
    let events = vec![
      json!({ "type": 4 }),
      piece("a", 1, 2, r#""x":1}"#),
      piece("b", 0, 2, "{"),
      piece("a", 0, 2, r#"{"type":2,"#),
      piece("c", 0, 1, "not json"),
    ];
    assert_eq!(
      restore_fragments(events),
      [json!({ "type": 4 }), json!({ "type": 2, "x": 1 })]
    );
  }

  #[test]
  fn builds_page_urls_like_upstream() {
    assert_eq!(
      page_url(Some("example.com, other.com"), "/a"),
      Some("https://example.com/a".to_owned())
    );
    assert_eq!(
      page_url(Some("localhost:3000"), ""),
      Some("http://localhost:3000/".to_owned())
    );
    assert_eq!(
      page_url(Some("http://x.test/base/"), "/b"),
      Some("http://x.test/b".to_owned())
    );
    assert_eq!(page_url(None, "/"), None);
  }

  #[test]
  fn picks_the_most_used_viewport() {
    let samples = [
      (1280, 800, 1280, 3000, 2),
      (390, 844, 390, 5000, 3),
      (1280, 800, 1300, 2000, 2),
    ];
    assert_eq!(pick_viewport(&samples), Some((1280, 800, 1300, 3000)));
    assert_eq!(pick_viewport(&[]), None);
  }
}
