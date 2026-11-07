//! Website analytics routes under `/api/websites/{websiteId}`, ported from
//! upstream's `src/app/api/websites/[websiteId]` (stats, pageviews, metrics,
//! active visitors, date range and filter values).

use axum::Json;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use serde_json::{
  Value,
  json,
};
use uuid::Uuid;

use super::params::Params;
use super::{
  body,
  ok,
};
use crate::analytics::date::{
  self,
  Compare,
};
use crate::analytics::filters::{
  self,
  EVENT_COLUMNS,
  FilterValue,
  Options,
  QueryFilters,
  Range,
  SESSION_COLUMNS,
};
use crate::analytics::queries::{
  self,
  DEFAULT_LIMIT,
  Page,
};
use crate::analytics::{
  events,
  realtime,
  sessions,
};
use crate::auth::Auth;
use crate::error::{
  AppError,
  AppResult,
};
use crate::js;
use crate::permissions::{
  self as can,
  WEBSITE_DELETE,
  db_id,
};
use crate::state::AppState;

/// The sections that show the website's headline numbers and charts.
const OVERVIEW: &[&str] = &["overview", "compare"];
/// The sections that show breakdowns by dimension.
const BREAKDOWNS: &[&str] = &[
  "overview",
  "events",
  "sessions",
  "compare",
  "breakdown",
  "utm",
  "attribution",
];

/// The request's filters for a website the caller may view in one of
/// `sections`, in upstream's order: parameters (400), authentication (401),
/// permission (401), then the website's own filters.
async fn authorize(
  state: &AppState,
  auth: AppResult<Auth>,
  website_id: &str,
  sections: &[&str],
  params: &Params,
  options: Options,
) -> AppResult<(Uuid, QueryFilters)> {
  let filters = filters::from_request(&state.db, params, options).await?;
  let auth = auth?;
  can::require(can::can_view_website_section(&state.db, &auth, website_id, sections).await?)?;

  let website_id = db_id(website_id)?;
  let filters = filters.for_website(&state.db, website_id).await?;

  Ok((website_id, filters))
}

const FILTERS: Options = Options {
  filters: true,
  ..Options::NONE
};

/// `GET /api/websites/{websiteId}/stats`: totals for the period, and for the
/// comparison period (`compare`, by default the previous one).
pub async fn stats(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = authorize(&state, auth, &website_id, OVERVIEW, &params, FILTERS).await?;

  let mut data = queries::website_stats(&state.db, website_id, &filters).await?;
  let (start, end) = filters
    .compare
    .unwrap_or(Compare::Prev)
    .range(filters.start_date, filters.end_date);
  let comparison = queries::website_stats(&state.db, website_id, &filters.with_range(start, end)).await?;

  data["comparison"] = comparison;
  Ok(Json(data))
}

/// `GET /api/websites/{websiteId}/pageviews`: views and visitors per time
/// bucket, with the comparison period's when `compare` is given.
pub async fn pageviews(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = authorize(&state, auth, &website_id, OVERVIEW, &params, FILTERS).await?;

  let series = |filters: QueryFilters| {
    let db = state.db.clone();
    async move {
      let pageviews = queries::series(&db, website_id, &filters, false).await?;
      let sessions = queries::series(&db, website_id, &filters, true).await?;
      AppResult::Ok((pageviews, sessions))
    }
  };

  let (pageviews, sessions) = series(filters.clone()).await?;

  let Some(compare) = filters.compare else {
    return Ok(Json(json!({ "pageviews": pageviews, "sessions": sessions })));
  };
  let (start, end) = compare.range(filters.start_date, filters.end_date);
  let (compare_pageviews, compare_sessions) = series(filters.with_range(start, end)).await?;

  Ok(Json(json!({
      "pageviews": pageviews,
      "sessions": sessions,
      "startDate": js::date(filters.start_date),
      "endDate": js::date(filters.end_date),
      "compare": {
          "pageviews": compare_pageviews,
          "sessions": compare_sessions,
          "startDate": js::date(start),
          "endDate": js::date(end),
      },
  })))
}

/// The `type`, `limit` and `offset` of the metrics routes. Negative values are
/// a 400 (upstream fails with a 500); fractions round as Postgres rounds them.
fn metric_parameters(params: &Params) -> AppResult<(String, Page)> {
  let kind = params.get("type").ok_or_else(AppError::bad_request)?;
  let number = |key: &str, default: i64| -> AppResult<i64> {
    let Some(value) = params.get(key) else {
      return Ok(default);
    };
    let number = js::string_to_number(value);
    if !number.is_finite() || number < 0.0 {
      return Err(AppError::bad_request());
    }
    Ok(number.round() as i64)
  };

  Ok((
    kind.to_owned(),
    Page {
      limit: number("limit", DEFAULT_LIMIT)?,
      offset: number("offset", 0)?,
    },
  ))
}

const METRICS: Options = Options {
  search: true,
  filters: true,
  ..Options::NONE
};

/// Which breakdown a metric `type` asks for.
enum Metric {
  Session,
  Event,
  Pageview,
  Channel,
}

/// The breakdown for `kind`, with a search narrowing that dimension, or `None`
/// for unknown types (a 400).
fn metric(kind: &str, filters: &mut QueryFilters) -> Option<Metric> {
  if let Some(search) = filters.search.clone().filter(|s| !s.is_empty()) {
    filters.set(kind, FilterValue::Text(format!("c.{search}")));
  }

  if SESSION_COLUMNS.contains(&kind) {
    Some(Metric::Session)
  } else if kind == "event" {
    Some(Metric::Event)
  } else if EVENT_COLUMNS.contains(&kind) {
    Some(Metric::Pageview)
  } else if kind == "channel" {
    Some(Metric::Channel)
  } else {
    None
  }
}

/// `GET /api/websites/{websiteId}/metrics`: visitors (or event counts) per value
/// of a dimension (`type`).
pub async fn metrics(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (kind, page) = metric_parameters(&params)?;
  let (website_id, mut filters) = authorize(&state, auth, &website_id, BREAKDOWNS, &params, METRICS).await?;
  let db = &state.db;

  let rows = match metric(&kind, &mut filters).ok_or_else(AppError::bad_request)? {
    Metric::Session => queries::session_metrics(db, website_id, &kind, page, &filters).await?,
    Metric::Event => queries::event_metrics(db, website_id, &kind, page, &filters).await?,
    Metric::Pageview => queries::pageview_metrics(db, website_id, &kind, page, &filters).await?,
    Metric::Channel => queries::channel_metrics(db, website_id, &filters).await?,
  };

  Ok(Json(Value::Array(rows)))
}

/// `GET /api/websites/{websiteId}/metrics/expanded`: views, visitors, visits,
/// bounces and time per value of a dimension.
pub async fn expanded_metrics(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (kind, page) = metric_parameters(&params)?;
  let (website_id, mut filters) = authorize(&state, auth, &website_id, OVERVIEW, &params, METRICS).await?;
  let db = &state.db;

  let rows = match metric(&kind, &mut filters).ok_or_else(AppError::bad_request)? {
    Metric::Session => queries::session_expanded_metrics(db, website_id, &kind, page, &filters).await?,
    Metric::Event => queries::event_expanded_metrics(db, website_id, &kind, page, &filters).await?,
    Metric::Pageview => queries::pageview_expanded_metrics(db, website_id, &kind, page, &filters).await?,
    Metric::Channel => queries::channel_expanded_metrics(db, website_id, &filters).await?,
  };

  Ok(Json(Value::Array(rows)))
}

/// `GET /api/websites/{websiteId}/active`: visitors in the last five minutes.
pub async fn active(
  State(state): State<AppState>,
  auth: Auth,
  Path(website_id): Path<String>,
) -> AppResult<Json<Value>> {
  let sections = ["overview", "realtime"];
  can::require(can::can_view_website_section(&state.db, &auth, &website_id, &sections).await?)?;

  Ok(Json(queries::active_visitors(&state.db, db_id(&website_id)?).await?))
}

/// `GET /api/websites/{websiteId}/daterange`: when the website's data starts
/// and ends.
pub async fn date_range(
  State(state): State<AppState>,
  auth: Auth,
  Path(website_id): Path<String>,
) -> AppResult<Json<Value>> {
  can::require(can::can_view_shared_website(&state.db, &auth, &website_id).await?)?;

  Ok(Json(queries::date_range(&state.db, db_id(&website_id)?).await?))
}

/// Upstream's `fieldsParam`: the dimensions `values` suggests.
const VALUE_FIELDS: &[&str] = &[
  "path",
  "referrer",
  "title",
  "query",
  "os",
  "browser",
  "device",
  "country",
  "region",
  "city",
  "tag",
  "hostname",
  "distinctId",
  "language",
  "event",
  "utmSource",
  "utmMedium",
  "utmCampaign",
  "utmContent",
  "utmTerm",
];

/// `GET /api/websites/{websiteId}/values`: the most common values of a
/// dimension, for filter suggestions.
pub async fn values(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let kind = params
    .get("type")
    .filter(|kind| VALUE_FIELDS.contains(kind))
    .ok_or_else(AppError::bad_request)?;
  let options = Options {
    search: true,
    ..Options::NONE
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, BREAKDOWNS, &params, options).await?;

  let column = filters::filter_column(kind).expect("every value field has a column");
  let rows = queries::values(&state.db, website_id, column, &filters).await?;

  Ok(Json(Value::Array(rows)))
}

const LIST: Options = Options {
  search: true,
  filters: true,
  paging: true,
  ..Options::NONE
};

/// `GET /api/websites/{websiteId}/events`: the period's events, newest first.
pub async fn events(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = authorize(&state, auth, &website_id, &["events"], &params, LIST).await?;

  Ok(Json(events::list(&state.db, website_id, &filters).await?))
}

/// `GET /api/websites/{websiteId}/events/series`: custom events per name and
/// time bucket, optionally for the `limit` most frequent names.
pub async fn event_series(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  // `limit` as upstream's `z.coerce.number()`; 0 means none. Negative values
  // are a 400 (upstream fails with a 500).
  let limit = match params.get("limit") {
    None => None,
    Some(value) => {
      let number = js::string_to_number(value);
      if !number.is_finite() || number < 0.0 {
        return Err(AppError::bad_request());
      }
      (number != 0.0).then(|| number.round() as i64)
    }
  };
  let options = Options {
    filters: true,
    range: Range::Timestamps,
    timezone_required: true,
    ..Options::NONE
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, &["events"], &params, options).await?;

  let rows = events::series(&state.db, website_id, limit, &filters).await?;
  Ok(Json(Value::Array(rows)))
}

/// `GET /api/websites/{websiteId}/events/stats`: custom event totals, and the
/// comparison period's.
pub async fn event_stats(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = authorize(&state, auth, &website_id, &["events"], &params, FILTERS).await?;

  let mut data = events::stats(&state.db, website_id, &filters).await?;
  let (start, end) = filters
    .compare
    .unwrap_or(Compare::Prev)
    .range(filters.start_date, filters.end_date);
  data["comparison"] = events::stats(&state.db, website_id, &filters.with_range(start, end)).await?;

  Ok(Json(json!({ "data": data })))
}

/// `GET /api/websites/{websiteId}/sessions`: the period's sessions, most
/// recently active first.
pub async fn sessions(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = authorize(&state, auth, &website_id, &["sessions"], &params, LIST).await?;

  Ok(Json(sessions::list(&state.db, website_id, &filters).await?))
}

/// `GET /api/websites/{websiteId}/sessions/stats`
pub async fn session_stats(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let options = Options {
    filters: true,
    range: Range::Timestamps,
    ..Options::NONE
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, &["sessions"], &params, options).await?;

  Ok(Json(sessions::stats(&state.db, website_id, &filters).await?))
}

/// `GET /api/websites/{websiteId}/sessions/weekly`: visitors by weekday and hour.
pub async fn weekly(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let options = Options {
    filters: true,
    range: Range::Timestamps,
    timezone_required: true,
    ..Options::NONE
  };
  let sections = ["overview", "sessions"];
  let (website_id, filters) = authorize(&state, auth, &website_id, &sections, &params, options).await?;

  Ok(Json(sessions::weekly(&state.db, website_id, &filters).await?))
}

/// The sections a single session's details belong to.
const SESSION_SECTIONS: &[&str] = &["sessions", "events", "realtime", "revenue"];

/// The website and session IDs, once the caller may view the website's sessions.
async fn session_access(state: &AppState, auth: &Auth, website_id: &str, session_id: &str) -> AppResult<(Uuid, Uuid)> {
  can::require(can::can_view_website_section(&state.db, auth, website_id, SESSION_SECTIONS).await?)?;
  Ok((db_id(website_id)?, db_id(session_id)?))
}

/// `GET /api/websites/{websiteId}/sessions/{sessionId}`: the session, with the
/// identities it is linked to and how many sessions share its identity.
pub async fn session(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, session_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  let can_delete = can::can_change_website(&state.db, &auth, &website_id, WEBSITE_DELETE).await?;
  let (website_id, session_id) = session_access(&state, &auth, &website_id, &session_id).await?;

  let Some(mut data) = sessions::get(&state.db, website_id, session_id).await? else {
    return Err(AppError::NotFound);
  };

  let linked = sessions::linked_distinct_ids(&state.db, website_id, session_id).await?;
  let distinct_ids = if linked.is_empty() {
    data["distinctId"]
      .as_str()
      .filter(|id| !id.is_empty())
      .map(|id| vec![id.to_owned()])
      .unwrap_or_default()
  } else {
    linked
  };

  // A session linked to several identities belongs to none of them.
  let object = data.as_object_mut().expect("a row is an object");
  let mut stitched = 1;
  match distinct_ids.as_slice() {
    [distinct_id] => {
      object.insert("distinctId".to_owned(), json!(distinct_id));
      let links = sessions::linked_sessions(&state.db, website_id, distinct_id).await?;
      stitched += links.iter().filter(|(id, _)| *id != session_id).count();
    }
    _ => {
      object.remove("distinctId");
    }
  }
  object.insert("canDelete".to_owned(), json!(can_delete));
  object.insert("distinctIds".to_owned(), json!(distinct_ids));
  object.insert("stitchedSessionCount".to_owned(), json!(stitched));

  Ok(Json(data))
}

/// `DELETE /api/websites/{websiteId}/sessions/{sessionId}`
pub async fn delete_session(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, session_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  can::require(can::can_change_website(&state.db, &auth, &website_id, WEBSITE_DELETE).await?)?;
  let (website_id, session_id) = (db_id(&website_id)?, db_id(&session_id)?);

  if !sessions::delete(&state.db, website_id, session_id).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
}

/// `GET /api/websites/{websiteId}/sessions/{sessionId}/activity`: the session's
/// events, with those of sessions sharing its identity (and the range widened
/// to whole months around when they were linked).
pub async fn session_activity(
  State(state): State<AppState>,
  Path((website_id, session_id)): Path<(String, String)>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let timestamp = |key: &str| -> AppResult<chrono::DateTime<chrono::Utc>> {
    let number = js::string_to_number(params.get(key).ok_or_else(AppError::bad_request)?);
    if number.fract() != 0.0 {
      return Err(AppError::bad_request());
    }
    date::from_js_millis(number).ok_or_else(AppError::bad_request)
  };
  let (mut start, mut end) = (timestamp("startAt")?, timestamp("endAt")?);
  let requested_identity = params.get("distinctId").map(str::to_owned);
  if requested_identity.as_deref().is_some_and(|id| body::js_len(id) > 50) {
    return Err(AppError::bad_request());
  }

  let auth = auth?;
  let (website_id, session_id) = session_access(&state, &auth, &website_id, &session_id).await?;

  let distinct_ids = match requested_identity {
    Some(id) => vec![id],
    None => sessions::linked_distinct_ids(&state.db, website_id, session_id).await?,
  };
  let mut session_ids = vec![session_id];
  if let [distinct_id] = distinct_ids.as_slice() {
    let links = sessions::linked_sessions(&state.db, website_id, distinct_id).await?;
    for (id, _) in &links {
      if !session_ids.contains(id) {
        session_ids.push(*id);
      }
    }
    let dates: Vec<_> = links.iter().filter_map(|(_, at)| *at).collect();
    if session_ids.len() > 1
      && let (Some(first), Some(last)) = (dates.iter().min(), dates.iter().max())
    {
      start = start.min(date::month_start(*first));
      end = end.max(date::month_end(*last));
    }
  }

  let options = Options {
    range: Range::Fixed(start, end),
    ..Options::NONE
  };
  let filters = filters::from_request(&state.db, &Params::parse(None), options)
    .await?
    .for_website(&state.db, website_id)
    .await?;

  let rows = sessions::activity(&state.db, website_id, &session_ids, &filters).await?;
  Ok(Json(Value::Array(rows)))
}

/// `GET /api/websites/{websiteId}/sessions/{sessionId}/properties`: the
/// session's stored data.
pub async fn session_properties(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, session_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  let (website_id, session_id) = session_access(&state, &auth, &website_id, &session_id).await?;

  Ok(Json(Value::Array(
    sessions::data(&state.db, website_id, session_id).await?,
  )))
}

/// `GET /api/realtime/{websiteId}`: the last 30 minutes.
pub async fn realtime(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  use chrono::{
    Duration,
    DurationRound,
    Utc,
  };

  let params = Params::parse(query.as_deref());
  let now = Utc::now();
  let start = now.duration_trunc(Duration::minutes(1)).unwrap_or(now) - Duration::minutes(30);
  let options = Options {
    filters: true,
    range: Range::Fixed(start, now),
    ..Options::NONE
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, &["realtime"], &params, options).await?;

  Ok(Json(realtime::data(&state.db, website_id, &filters).await?))
}
