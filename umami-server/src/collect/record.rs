//! `POST /api/record`: session recordings and heatmap events from the
//! recorder script, ported from upstream's `src/app/api/record/route.ts` (with
//! `lib/replay.ts`'s event count and `saveRecording`/`saveHeatmapEvents`).

use std::io::Write as _;
use std::net::SocketAddr;

use axum::Json;
use axum::body::to_bytes;
use axum::extract::{
  Request,
  State,
};
use axum::http::{
  HeaderMap,
  header,
};
use chrono::{
  DateTime,
  Utc,
};
use crate_ua_rules as ua;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{
  Map,
  Value,
  json,
};
use sqlx::{
  PgPool,
  QueryBuilder,
};
use uuid::Uuid;

use super::{
  CacheToken,
  PeerAddr,
  client,
};
use crate::analytics::date;
use crate::error::{
  AppError,
  AppResult,
};
use crate::queries::website;
use crate::state::AppState;
use crate::{
  js,
  zod,
};

/// Upstream's `MAX_RECORD_REQUEST_BYTES`.
const MAX_BODY_BYTES: usize = 1_000_000;

/// Upstream's limit on events per request.
const MAX_EVENTS: usize = 200;

/// Upstream's `HEATMAP_EVENT_TYPE`.
const CLICK: i32 = 1;
const SCROLL: i32 = 2;

/// Upstream's `FIELD_LENGTH` for a heatmap event's URL path (the column's).
const URL_PATH_LENGTH: usize = 500;

/// `z.coerce.number()`: `Number(value)`, finite. `None` if invalid.
fn coerce_number(value: &Value) -> Option<f64> {
  let number = match value {
    Value::Number(n) => n.as_f64()?,
    Value::String(s) => js::string_to_number(s),
    Value::Bool(b) => f64::from(u8::from(*b)),
    Value::Null => 0.0,
    Value::Array(_) | Value::Object(_) => return None,
  };
  number.is_finite().then_some(number)
}

/// `z.coerce.number().int().optional()`: absent is `None`.
fn optional_int(object: &Map<String, Value>, key: &str) -> Result<Option<f64>, AppError> {
  const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
  match object.get(key) {
    None => Ok(None),
    Some(value) => {
      coerce_number(value)
        .filter(|n| n.fract() == 0.0 && n.abs() <= MAX_SAFE)
        .map(Some)
        .ok_or_else(AppError::bad_request)
    }
  }
}

/// `z.coerce.number().optional()`.
fn optional_number(object: &Map<String, Value>, key: &str) -> Result<Option<f64>, AppError> {
  object
    .get(key)
    .map(|value| coerce_number(value).ok_or_else(AppError::bad_request))
    .transpose()
}

/// A validated heatmap event.
struct HeatmapEvent {
  kind: i32,
  url: String,
  x: Option<f64>,
  y: Option<f64>,
  page_x: Option<f64>,
  page_y: Option<f64>,
  page_w: Option<f64>,
  page_h: Option<f64>,
  viewport_w: Option<f64>,
  viewport_h: Option<f64>,
  scroll_pct: Option<f64>,
  timestamp: Option<f64>,
}

impl HeatmapEvent {
  /// Upstream's `click`/`scroll` event schemas; fields the type lacks are
  /// ignored.
  fn parse(value: &Value) -> AppResult<Self> {
    let object = value.as_object().ok_or_else(AppError::bad_request)?;
    let kind = match object.get("type").and_then(Value::as_str) {
      Some("click") => CLICK,
      Some("scroll") => SCROLL,
      _ => return Err(AppError::bad_request()),
    };
    let url = object
      .get("url")
      .and_then(Value::as_str)
      .ok_or_else(AppError::bad_request)?
      .to_owned();
    let click = |key: &str| -> AppResult<Option<f64>> {
      if kind == CLICK {
        optional_number(object, key)
      } else {
        Ok(None)
      }
    };

    Ok(Self {
      kind,
      url,
      x: click("x")?,
      y: click("y")?,
      page_x: click("pageX")?,
      page_y: click("pageY")?,
      page_w: optional_number(object, "pageW")?,
      page_h: optional_number(object, "pageH")?,
      viewport_w: optional_number(object, "viewportW")?,
      viewport_h: optional_number(object, "viewportH")?,
      scroll_pct: if kind == SCROLL {
        optional_number(object, "scrollPct")?
      } else {
        None
      },
      timestamp: optional_int(object, "timestamp")?,
    })
  }
}

/// What a request records.
enum Recording {
  Replay(Vec<Value>),
  Heatmap(Vec<HeatmapEvent>),
}

/// The validated body: the website, the events and the client's timestamp
/// (Unix seconds).
struct RecordBody {
  website_id: Uuid,
  recording: Recording,
  timestamp: Option<f64>,
}

impl RecordBody {
  fn parse(body: &Value) -> AppResult<Self> {
    let bad = AppError::bad_request;
    let object = body.as_object().ok_or_else(bad)?;
    let kind = object.get("type").and_then(Value::as_str).ok_or_else(bad)?;
    let payload = object.get("payload").and_then(Value::as_object).ok_or_else(bad)?;
    let website_id = payload
      .get("website")
      .and_then(Value::as_str)
      .and_then(zod::uuid)
      .ok_or_else(bad)?;
    let events = payload
      .get("events")
      .and_then(Value::as_array)
      .filter(|events| events.len() <= MAX_EVENTS)
      .ok_or_else(bad)?;
    let recording = match kind {
      "record" => Recording::Replay(events.clone()),
      "heatmap" => Recording::Heatmap(events.iter().map(HeatmapEvent::parse).collect::<AppResult<_>>()?),
      _ => return Err(bad()),
    };

    Ok(Self {
      website_id,
      recording,
      timestamp: optional_int(payload, "timestamp")?,
    })
  }

  fn is_empty(&self) -> bool {
    match &self.recording {
      Recording::Replay(events) => events.is_empty(),
      Recording::Heatmap(events) => events.is_empty(),
    }
  }
}

/// A 200 that records nothing, with upstream's reason.
fn skipped(reason: &str) -> Json<Value> {
  Json(json!({ "ok": false, "reason": reason }))
}

/// `POST /api/record`: stores a chunk of a session recording, or heatmap
/// events, for the session and visit of the request's `x-umami-cache` token.
pub async fn record(
  State(state): State<AppState>,
  PeerAddr(peer): PeerAddr,
  request: Request,
) -> AppResult<Json<Value>> {
  let headers = request.headers().clone();
  let declared = headers
    .get(header::CONTENT_LENGTH)
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.parse::<usize>().ok());
  if declared.is_some_and(|size| size > MAX_BODY_BYTES) {
    return Err(AppError::PayloadTooLarge);
  }
  let bytes = to_bytes(request.into_body(), MAX_BODY_BYTES)
    .await
    .map_err(|_| AppError::PayloadTooLarge)?;

  let body = RecordBody::parse(&serde_json::from_slice(&bytes).unwrap_or(Value::Null))?;
  if body.is_empty() {
    return Ok(Json(json!({ "ok": true })));
  }

  let token = client::header_string(&headers, "x-umami-cache")
    .filter(|token| !token.is_empty())
    .ok_or_else(|| AppError::BadRequest("Missing session token.".to_owned()))?;
  let (session_id, visit_id) = session(&state, &token, body.website_id)
    .ok_or_else(|| AppError::BadRequest("Invalid session token.".to_owned()))?;

  let Some(found) = website::find(&state.db, body.website_id)
    .await?
    .filter(|website| website.deleted_at.is_none())
  else {
    return Err(AppError::BadRequest("Website not found.".to_owned()));
  };
  if !found.recorder_enabled {
    return Ok(skipped("recorder_disabled"));
  }
  let config = website::recorder_config(found.replay_config.as_ref().unwrap_or(&Value::Null));

  if let Some(response) = rejected_client(&state, &headers, peer)? {
    return Ok(response);
  }

  let fallback_ms = body
    .timestamp
    .filter(|&t| t != 0.0)
    .unwrap_or_else(|| Utc::now().timestamp() as f64)
    * 1000.0;
  let ids = (body.website_id, session_id, visit_id);
  match body.recording {
    Recording::Replay(events) => {
      if config.get("replayEnabled") != Some(&Value::Bool(true)) {
        return Ok(skipped("replay_disabled"));
      }
      save_recording(&state, ids, &events, fallback_ms).await?;
    }
    Recording::Heatmap(events) => {
      if config.get("heatmapEnabled") != Some(&Value::Bool(true)) {
        return Ok(skipped("heatmap_disabled"));
      }
      // Like upstream, a failed save is only logged.
      if let Err(error) = save_heatmap(&state.db, &state, ids, &events, fallback_ms).await {
        tracing::error!(%error, "heatmap save failed");
      }
    }
  }

  Ok(Json(json!({ "ok": true })))
}

/// The session and visit of a cache token. Unlike upstream, a token issued for
/// another website is refused.
fn session(state: &AppState, token: &str, website_id: Uuid) -> Option<(Uuid, Uuid)> {
  let cache = CacheToken::parse(token, &state.config.secret)?;
  if cache
    .website_id
    .as_deref()
    .is_some_and(|id| id.parse::<Uuid>().ok() != Some(website_id))
  {
    return None;
  }
  Some((cache.session_id?.parse().ok()?, cache.visit_id?.parse().ok()?))
}

/// Upstream's bot and ignored-IP checks: a bot's response, or a 403.
fn rejected_client(state: &AppState, headers: &HeaderMap, peer: Option<SocketAddr>) -> AppResult<Option<Json<Value>>> {
  let settings = &state.config.collect;
  let user_agent = client::header_string(headers, header::USER_AGENT).unwrap_or_default();
  if !settings.disable_bot_check && ua::is_bot(&user_agent) {
    return Ok(Some(Json(json!({ "beep": "boop" }))));
  }
  let ip = client::client_ip(headers, settings.client_ip_header.as_deref(), peer);
  if client::is_ignored(ip.as_deref(), &settings.ignore_ips) {
    return Err(AppError::Forbidden);
  }
  Ok(None)
}

/// Upstream's `getReplayEventCount`: events, counting a fragmented event once.
fn event_count(events: &[Value]) -> i64 {
  events
    .iter()
    .filter(|event| !is_fragment(event) || event["data"]["index"].as_i64() == Some(0))
    .count() as i64
}

/// Upstream's `REPLAY_EVENT_FRAGMENT_TYPE`.
pub const FRAGMENT_TYPE: &str = "umami:rrweb-event-fragment";

/// Upstream's `isReplayEventFragment`: one piece of an event too large to send
/// whole.
pub fn is_fragment(event: &Value) -> bool {
  let data = &event["data"];
  let integer = |v: &Value| v.as_f64().is_some_and(|n| n.fract() == 0.0);
  event["type"].as_str() == Some(FRAGMENT_TYPE)
    && data["id"].is_string()
    && integer(&data["index"])
    && integer(&data["total"])
    && data["index"].as_f64() >= Some(0.0)
    && data["index"].as_f64() < data["total"].as_f64()
    && data["value"].is_string()
}

/// Upstream's `saveRecording`: the events, gzipped as `JSON.stringify` writes
/// them, spanning their earliest to latest timestamps (or the request's).
async fn save_recording(
  state: &AppState,
  (website_id, session_id, visit_id): (Uuid, Uuid, Uuid),
  events: &[Value],
  fallback_ms: f64,
) -> AppResult<()> {
  let times: Vec<f64> = events
    .iter()
    .filter_map(|event| event.get("timestamp").and_then(coerce_number))
    .filter(|t| *t > 0.0)
    .collect();
  let (min, max) = if times.is_empty() {
    (fallback_ms, fallback_ms)
  } else {
    (
      times.iter().copied().fold(f64::INFINITY, f64::min),
      times.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    )
  };
  // Upstream fails with a 500 on times and chunk numbers its columns cannot hold.
  let time = |ms: f64| date::from_js_millis(ms).ok_or_else(AppError::bad_request);
  let (started_at, ended_at) = (time(min)?, time(max)?);
  let chunk_index = i32::try_from((fallback_ms / 1000.0) as i64).map_err(|_| AppError::bad_request())?;

  let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
  encoder
    .write_all(js::stringify(&Value::Array(events.to_vec())).as_bytes())
    .map_err(anyhow::Error::from)?;
  let compressed = encoder.finish().map_err(anyhow::Error::from)?;

  sqlx::query(
    "insert into session_replay (replay_id, website_id, session_id, visit_id, chunk_index, events, \
     event_count, started_at, ended_at) values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
  )
  .bind(state.new_id())
  .bind(website_id)
  .bind(session_id)
  .bind(visit_id)
  .bind(chunk_index)
  .bind(compressed)
  .bind(i32::try_from(event_count(events)).unwrap_or(i32::MAX))
  .bind(started_at)
  .bind(ended_at)
  .execute(&state.db)
  .await?;

  Ok(())
}

/// Upstream's `getUrlPath`: an absolute URL's path, a relative one's up to its
/// query or fragment, else `/`.
fn url_path(url: &str) -> String {
  if let Ok(parsed) = url::Url::parse(url) {
    return match parsed.path() {
      "" => "/".to_owned(),
      path => path.to_owned(),
    };
  }
  if url.starts_with('/') {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    return if path.is_empty() {
      "/".to_owned()
    } else {
      path.to_owned()
    };
  }
  "/".to_owned()
}

/// `Math.round` of a coordinate, or `None` if it does not fit the column
/// (upstream fails the whole batch).
fn to_int(value: Option<f64>) -> Option<i32> {
  value.and_then(|v| i32::try_from((v + 0.5).floor() as i64).ok())
}

/// Upstream's `saveHeatmapEvents`: one row per event, coordinates rounded,
/// scroll depth clamped to 0-100.
async fn save_heatmap(
  db: &PgPool,
  state: &AppState,
  (website_id, session_id, visit_id): (Uuid, Uuid, Uuid),
  events: &[HeatmapEvent],
  fallback_ms: f64,
) -> AppResult<()> {
  let mut rows = Vec::new();
  for event in events {
    let created_at: DateTime<Utc> =
      date::from_js_millis(event.timestamp.unwrap_or(fallback_ms)).ok_or_else(AppError::bad_request)?;
    rows.push((event, created_at));
  }

  let mut query = QueryBuilder::new(
    "insert into heatmap_event (heatmap_event_id, website_id, session_id, visit_id, url_path, \
     event_type, x, y, page_x, page_y, page_w, viewport_w, viewport_h, page_h, scroll_pct, created_at) ",
  );
  query.push_values(rows, |mut row, (event, created_at)| {
    let scroll = to_int(event.scroll_pct).map(|pct| pct.clamp(0, 100));
    row
      .push_bind(state.new_id())
      .push_bind(website_id)
      .push_bind(session_id)
      .push_bind(visit_id)
      .push_bind(js::truncate(&url_path(&event.url), URL_PATH_LENGTH).to_owned())
      .push_bind(event.kind)
      .push_bind(to_int(event.x))
      .push_bind(to_int(event.y))
      .push_bind(to_int(event.page_x))
      .push_bind(to_int(event.page_y))
      .push_bind(to_int(event.page_w))
      .push_bind(to_int(event.viewport_w))
      .push_bind(to_int(event.viewport_h))
      .push_bind(to_int(event.page_h))
      .push_bind(scroll)
      .push_bind(created_at);
  });
  query.build().execute(db).await?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn counts_fragmented_events_once() {
    let fragment =
      |index: i64| json!({ "type": FRAGMENT_TYPE, "data": { "id": "a", "index": index, "total": 2, "value": "{}" } });
    let events = [
      json!({ "type": 2 }),
      fragment(0),
      fragment(1),
      json!({ "type": FRAGMENT_TYPE }),
    ];
    assert_eq!(event_count(&events), 3);
  }

  #[test]
  fn takes_url_paths_like_upstream() {
    assert_eq!(url_path("https://example.com/a/b?c#d"), "/a/b");
    assert_eq!(url_path("https://example.com"), "/");
    assert_eq!(url_path("/docs?x=1"), "/docs");
    assert_eq!(url_path("?x"), "/");
    assert_eq!(url_path("relative/path"), "/");
  }

  #[test]
  fn rounds_like_javascript() {
    assert_eq!(to_int(Some(2.5)), Some(3));
    assert_eq!(to_int(Some(-2.5)), Some(-2));
    assert_eq!(to_int(Some(1e12)), None);
    assert_eq!(to_int(None), None);
  }

  #[test]
  fn validates_heatmap_events() {
    let click = HeatmapEvent::parse(&json!({ "type": "click", "url": "/", "x": "12.4", "scrollPct": 5 })).unwrap();
    assert_eq!((click.x, click.scroll_pct), (Some(12.4), None));
    for invalid in [
      json!({ "type": "move", "url": "/" }),
      json!({ "type": "click" }),
      json!({ "type": "scroll", "url": "/", "scrollPct": "x" }),
      json!({ "type": "click", "url": "/", "timestamp": 1.5 }),
    ] {
      assert!(HeatmapEvent::parse(&invalid).is_err(), "{invalid}");
    }
  }
}
