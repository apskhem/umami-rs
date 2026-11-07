//! Session replay routes: the recorder's settings
//! (`/api/websites/{websiteId}/recorder`), recorded visits (`replays`, a
//! session's `replays`, one replay), saved replays (`replays/saved`) and
//! `heatmaps`. Ported from upstream's `src/app/api/websites/[websiteId]`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use axum::http::header;
use axum::response::{
  IntoResponse,
  Response,
};
use serde_json::{
  Value,
  json,
};
use uuid::Uuid;

use super::body;
use super::params::{
  PAGING_SEARCH,
  Params,
};
use super::reports::check_range;
use crate::analytics::date;
use crate::analytics::filters::{
  self,
  Options,
  QueryFilters,
  Range,
};
use crate::analytics::replays::{
  self,
  Until,
};
use crate::auth::Auth;
use crate::error::{
  AppError,
  AppResult,
};
use crate::js;
use crate::permissions::{
  self as can,
  WEBSITE_UPDATE,
  db_id,
};
use crate::queries::{
  replay,
  website,
};
use crate::state::AppState;

/// The recorder's defaults (upstream's `recorder` route).
const SAMPLE_RATE: f64 = 0.15;
const MAX_DURATION: i64 = 300_000;

/// `GET /api/websites/{websiteId}/recorder`: whether the recorder script should
/// record, and how. Public, for the script.
pub async fn recorder(State(state): State<AppState>, Path(website_id): Path<String>) -> AppResult<Json<Value>> {
  // Deleted websites record nothing (upstream still configures them).
  let Some(found) = website::get(&state.db, &website_id)
    .await?
    .filter(|website| website.recorder_enabled && website.deleted_at.is_none())
  else {
    return Ok(Json(json!({ "enabled": false })));
  };
  let config = website::recorder_config(found.replay_config.as_ref().unwrap_or(&Value::Null));
  let setting = |key: &str, default: Value| config.get(key).cloned().unwrap_or(default);

  Ok(Json(json!({
      "enabled": true,
      "replayEnabled": config.get("replayEnabled") == Some(&Value::Bool(true)),
      "heatmapEnabled": config.get("heatmapEnabled") == Some(&Value::Bool(true)),
      "sampleRate": setting("sampleRate", json!(SAMPLE_RATE)),
      "heatmapSampleRate": setting("heatmapSampleRate", json!(SAMPLE_RATE)),
      "maskLevel": setting("maskLevel", json!("moderate")),
      "maxDuration": setting("maxDuration", json!(MAX_DURATION)),
      "blockSelector": setting("blockSelector", json!("")),
  })))
}

/// The filters of a route that only signed-in users who can view the website
/// may use (upstream's `canViewAuthenticatedWebsite`), in upstream's order:
/// parameters (400), authentication and permission (401), then the website's
/// own filters.
async fn authorize(
  state: &AppState,
  auth: AppResult<Auth>,
  website_id: &str,
  params: &Params,
  options: Options,
) -> AppResult<(Uuid, QueryFilters)> {
  let filters = filters::from_request(&state.db, params, options).await?;
  let auth = auth?;
  can::require(can::can_view_authenticated_website(&state.db, &auth, website_id).await?)?;

  let website_id = db_id(website_id)?;
  Ok((website_id, filters.for_website(&state.db, website_id).await?))
}

/// The optional `minDuration` (`z.coerce.number().int().nonnegative()`), in
/// seconds, as milliseconds if positive.
fn min_duration(params: &Params) -> AppResult<Option<i64>> {
  let Some(value) = params.get("minDuration") else {
    return Ok(None);
  };
  let seconds = js::string_to_number(value);
  if seconds.fract() != 0.0 || !(0.0..=9_007_199_254_740_991.0).contains(&seconds) {
    return Err(AppError::bad_request());
  }
  Ok((seconds > 0.0).then(|| (seconds as i64).saturating_mul(1000)))
}

/// `GET .../replays`: the website's recorded visits, most recent first.
pub async fn list(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let min_duration = min_duration(&params)?;
  let options = Options {
    search: true,
    filters: true,
    paging: true,
    ..Options::NONE
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, &params, options).await?;

  Ok(Json(
    replays::list(&state.db, website_id, &filters, None, min_duration).await?,
  ))
}

/// `GET .../sessions/{sessionId}/replays`: the session's recorded visits.
pub async fn session_list(
  State(state): State<AppState>,
  Path((website_id, session_id)): Path<(String, String)>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let options = Options {
    search: true,
    paging: true,
    ..Options::NONE
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, &params, options).await?;
  // Upstream fails with a 500 on a malformed session ID.
  let session_id = db_id(&session_id)?;

  Ok(Json(
    replays::list(&state.db, website_id, &filters, Some(session_id), None).await?,
  ))
}

/// Upstream's `parseOptionalInteger`: an integer parameter, ignored otherwise.
fn optional_integer(params: &Params, key: &str) -> Option<i64> {
  let value = params.get(key).filter(|value| !value.is_empty())?;
  let number = js::string_to_number(value);
  (number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0).then_some(number as i64)
}

/// `GET .../replays/{replayId}`: a recorded visit's events, up to `until` (a
/// time), or `chunkIndex` and `eventIndex`.
pub async fn get(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, visit_id)): Path<(String, String)>,
  RawQuery(query): RawQuery,
) -> AppResult<Response> {
  let params = Params::parse(query.as_deref());
  let until = Until {
    time: optional_integer(&params, "until"),
    chunk: optional_integer(&params, "chunkIndex"),
    event: optional_integer(&params, "eventIndex"),
  };
  can::require(can::can_view_authenticated_website(&state.db, &auth, &website_id).await?)?;

  // Upstream fails with a 500 on a malformed ID or a time it cannot represent.
  let end_at = until
    .time
    .map(|ms| date::from_js_millis(ms as f64).ok_or_else(AppError::bad_request))
    .transpose()?;
  let chunks = replays::chunks(&state.db, db_id(&website_id)?, db_id(&visit_id)?, end_at, until.chunk).await?;
  let events = replays::restore_fragments(replays::merge(&chunks, &until));

  // The events' numbers as JavaScript writes them.
  let event_count = events.len();
  let body = json!({
      "sessionId": chunks.first().map(|c| c.session_id),
      "events": events,
      "startedAt": chunks.first().map(|c| js::date(c.started_at)),
      "endedAt": chunks.last().map(|c| js::date(c.ended_at)),
      "eventCount": event_count,
      "chunkCount": chunks.len(),
  });

  Ok(([(header::CONTENT_TYPE, "application/json")], js::stringify(&body)).into_response())
}

/// `GET .../replays/saved`: the website's saved replays, newest first.
pub async fn saved_list(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let filters = Params::parse(query.as_deref()).filters(PAGING_SEARCH)?;
  let auth = auth?;
  can::require(can::can_view_authenticated_website(&state.db, &auth, &website_id).await?)?;

  Ok(Json(replay::list(&state.db, db_id(&website_id)?, &filters).await?))
}

/// `GET .../replays/saved/{replayId}`: whether the visit's replay is saved.
pub async fn is_saved(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, visit_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  can::require(can::can_view_authenticated_website(&state.db, &auth, &website_id).await?)?;
  let saved = replay::is_saved(&state.db, db_id(&website_id)?, db_id(&visit_id)?).await?;

  Ok(Json(json!({ "isSaved": saved })))
}

/// `POST .../replays/saved/{replayId}`: saves the visit's replay (with an
/// optional name) or unsaves it.
pub async fn save(
  State(state): State<AppState>,
  Path((website_id, visit_id)): Path<(String, String)>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let saved = body::boolean(object, "isSaved")?;
  let name = body::optional_string(object, "name")?
    .map(|name| body::max_len(name, 100))
    .transpose()?;

  let auth = auth?;
  can::require(can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await?)?;
  // Admins pass for any ID; upstream then saves a replay of nothing.
  let Some(found) = website::get(&state.db, &website_id).await? else {
    return Err(AppError::NotFound);
  };
  let visit_id = db_id(&visit_id)?;

  if saved {
    replay::save(
      &state.db,
      state.new_id(),
      found.website_id,
      visit_id,
      name.unwrap_or_default(),
    )
    .await?;
  } else {
    replay::unsave(&state.db, found.website_id, visit_id).await?;
  }

  Ok(Json(json!({ "ok": true })))
}

/// `GET .../heatmaps`: the pages with clicks (or scrolls), and one page's
/// heatmap (`urlPath`, `mode`).
pub async fn heatmaps(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  check_range(&params)?;
  let url_path = params.get("urlPath").map(|path| body::max_len(path, 500)).transpose()?;
  let scroll = match params.get("mode") {
    None | Some("click") => false,
    Some("scroll") => true,
    Some(_) => return Err(AppError::bad_request()),
  };
  let options = Options {
    filters: true,
    range: Range::Timestamps,
    ..Options::NONE
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, &params, options).await?;

  Ok(Json(
    replays::heatmap(&state.db, website_id, &filters, url_path, scroll).await?,
  ))
}
