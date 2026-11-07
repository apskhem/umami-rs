//! Event and session data routes under `/api/websites/{websiteId}`:
//! `event-data` (with its event, events, fields, properties, stats and
//! values), `event-data-pivot` and its series, `session-data` and
//! `session-data-pivot`. Ported from upstream's
//! `src/app/api/websites/[websiteId]/{event,session}-data*`.

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

use super::analytics::authorize;
use super::params::Params;
use crate::analytics::data::{
  self,
  EventExplorer,
  Metric,
  ValuesOf,
};
use crate::analytics::filters::{
  Options,
  PropertyFilter,
  Range,
  pf_filters,
};
use crate::auth::Auth;
use crate::error::{
  AppError,
  AppResult,
};
use crate::js;
use crate::permissions::{
  self as can,
  db_id,
};
use crate::state::AppState;

const EVENTS: &[&str] = &["events"];
const SESSIONS: &[&str] = &["sessions"];

/// The schema of a data route: required `startAt`/`endAt` timestamps,
/// `filterParams`, and as given `pagingParams`, `timezone` and `unit`.
const fn options(paging: bool, timezone: bool, unit: bool) -> Options {
  Options {
    search: false,
    filters: true,
    paging,
    range: Range::Timestamps,
    timezone_required: false,
    timezone,
    unit,
  }
}

/// Neither `timezone` nor `unit`.
const PLAIN: Options = options(false, false, false);
/// `timezone` but no `unit`.
const ZONED: Options = options(false, true, false);
/// `timezone` and `unit`.
const SERIES: Options = options(false, true, true);
/// A paged pivot, with `timezone` and `unit`.
const PIVOT: Options = options(true, true, true);

/// A required string parameter (`z.string()`).
fn required<'a>(params: &'a Params, key: &str) -> AppResult<&'a str> {
  params.get(key).ok_or_else(AppError::bad_request)
}

/// The optional `dataType` (`z.coerce.number().int()`); `0` if absent.
fn data_type(params: &Params) -> AppResult<i64> {
  const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
  let Some(value) = params.get("dataType") else {
    return Ok(0);
  };
  let number = js::string_to_number(value);
  if number.fract() != 0.0 || number.abs() > MAX_SAFE {
    return Err(AppError::bad_request());
  }
  Ok(number as i64)
}

/// The `metric` (`z.enum(['sum', 'avg', 'count']).default('sum')`).
fn metric(params: &Params) -> AppResult<Metric> {
  params
    .get("metric")
    .map_or(Some(Metric::Sum), Metric::parse)
    .ok_or_else(AppError::bad_request)
}

/// The explorer's `eventName` and `pf_` filters.
fn event_explorer<'a>(params: &'a Params, filters: &'a [PropertyFilter]) -> AppResult<EventExplorer<'a>> {
  Ok(EventExplorer {
    event_name: required(params, "eventName")?,
    filters,
  })
}

/// `GET .../event-data`: events with their data, newest first and paged.
pub async fn event_data(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) =
    authorize(&state, auth, &website_id, EVENTS, &params, options(true, false, false)).await?;

  Ok(Json(data::event_data(&state.db, website_id, &filters).await?))
}

/// `GET .../event-data/{eventId}`: one event's data.
pub async fn event_data_by_id(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, event_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  can::require(can::can_view_website_section(&state.db, &auth, &website_id, EVENTS).await?)?;

  let rows = data::event_data_by_id(&state.db, db_id(&website_id)?, db_id(&event_id)?).await?;

  Ok(Json(json!(rows)))
}

/// `GET .../event-data/events`: properties per event name (with values when
/// filtered to an `event`).
pub async fn event_data_events(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = authorize(&state, auth, &website_id, EVENTS, &params, PLAIN).await?;

  Ok(Json(json!(
    data::event_data_events(&state.db, website_id, &filters).await?
  )))
}

/// `GET .../event-data/fields`: properties by frequency, optionally for one
/// `eventName`.
pub async fn event_data_fields(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = authorize(&state, auth, &website_id, EVENTS, &params, PLAIN).await?;
  let event_name = params.get("eventName");

  Ok(Json(json!(
    data::event_data_fields(&state.db, website_id, event_name, &filters).await?
  )))
}

/// `GET .../event-data/properties`: properties per event name.
pub async fn event_data_properties(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = authorize(&state, auth, &website_id, EVENTS, &params, PLAIN).await?;

  Ok(Json(json!(
    data::event_data_properties(&state.db, website_id, &filters).await?
  )))
}

/// `GET .../event-data/stats`: counts of events, properties and records.
pub async fn event_data_stats(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = authorize(&state, auth, &website_id, EVENTS, &params, PLAIN).await?;

  Ok(Json(data::event_data_stats(&state.db, website_id, &filters).await?))
}

/// `GET .../event-data/values`: a property's most frequent values.
pub async fn event_data_values(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let of = ValuesOf {
    property_name: required(&params, "propertyName")?,
    data_type: data_type(&params)?,
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, EVENTS, &params, PLAIN).await?;
  let event_name = params.get("eventName");

  Ok(Json(json!(
    data::event_data_values(&state.db, website_id, event_name, &of, &filters).await?
  )))
}

/// `GET .../event-data-pivot`: an event name's events with their data, as
/// parallel key and value lists, newest first and paged.
pub async fn event_data_pivot(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let pf = pf_filters(&params);
  let explorer = event_explorer(&params, &pf)?;
  let (website_id, filters) = authorize(&state, auth, &website_id, EVENTS, &params, PIVOT).await?;

  Ok(Json(
    data::event_data_pivot(&state.db, website_id, &explorer, &filters).await?,
  ))
}

/// Which event explorer series a route returns.
enum EventSeries {
  Array,
  Date,
  Numeric,
  Property,
}

/// The event explorer's series routes.
async fn event_series(
  state: AppState,
  website_id: String,
  query: Option<String>,
  auth: AppResult<Auth>,
  series: EventSeries,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let pf = pf_filters(&params);
  let explorer = event_explorer(&params, &pf)?;
  let property_name = required(&params, "propertyName")?;
  let (metric, options) = match series {
    EventSeries::Numeric => (metric(&params)?, SERIES),
    EventSeries::Date => (Metric::Sum, ZONED),
    EventSeries::Array | EventSeries::Property => (Metric::Sum, SERIES),
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, EVENTS, &params, options).await?;

  let db = &state.db;
  let rows = match series {
    EventSeries::Array => data::event_data_array_series(db, website_id, &explorer, property_name, &filters).await?,
    EventSeries::Date => data::event_data_date_series(db, website_id, &explorer, property_name, &filters).await?,
    EventSeries::Numeric => {
      data::event_data_numeric_series(db, website_id, &explorer, property_name, metric, &filters).await?
    }
    EventSeries::Property => {
      data::event_data_property_series(db, website_id, &explorer, property_name, &filters).await?
    }
  };

  Ok(Json(json!(rows)))
}

/// `GET .../event-data-pivot/array-series`
pub async fn event_array_series(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  event_series(state, website_id, query, auth, EventSeries::Array).await
}

/// `GET .../event-data-pivot/date-series`
pub async fn event_date_series(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  event_series(state, website_id, query, auth, EventSeries::Date).await
}

/// `GET .../event-data-pivot/numeric-series`
pub async fn event_numeric_series(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  event_series(state, website_id, query, auth, EventSeries::Numeric).await
}

/// `GET .../event-data-pivot/property-series`
pub async fn event_property_series(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  event_series(state, website_id, query, auth, EventSeries::Property).await
}

/// `GET .../event-data-pivot/numeric-stats`
pub async fn event_numeric_stats(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let pf = pf_filters(&params);
  let explorer = event_explorer(&params, &pf)?;
  let property_name = required(&params, "propertyName")?;
  let (website_id, filters) = authorize(&state, auth, &website_id, EVENTS, &params, PLAIN).await?;

  Ok(Json(
    data::event_data_numeric_stats(&state.db, website_id, &explorer, property_name, &filters).await?,
  ))
}

/// `GET .../session-data-pivot`: the sessions with a property, each with its
/// latest values, newest first and paged.
pub async fn session_data_pivot(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let pf = pf_filters(&params);
  let property_name = required(&params, "propertyName")?;
  let (website_id, filters) = authorize(&state, auth, &website_id, SESSIONS, &params, PIVOT).await?;

  Ok(Json(
    data::session_data_pivot(&state.db, website_id, property_name, &pf, &filters).await?,
  ))
}

/// Which session explorer result a route returns.
enum SessionSeries {
  Array,
  Date,
  Numeric,
  Property,
  NumericStats,
  ActivityStats,
}

/// The session explorer's routes that take a required `propertyName`.
async fn session_series(
  state: AppState,
  website_id: String,
  query: Option<String>,
  auth: AppResult<Auth>,
  series: SessionSeries,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let pf = pf_filters(&params);
  let property_name = required(&params, "propertyName")?;
  let (metric, options) = match series {
    SessionSeries::Numeric => (metric(&params)?, SERIES),
    SessionSeries::Date | SessionSeries::NumericStats => (Metric::Sum, ZONED),
    SessionSeries::Array | SessionSeries::Property | SessionSeries::ActivityStats => (Metric::Sum, SERIES),
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, SESSIONS, &params, options).await?;

  let db = &state.db;
  let result = match series {
    SessionSeries::Array => json!(data::session_data_array_series(db, website_id, property_name, &pf, &filters).await?),
    SessionSeries::Date => json!(data::session_data_date_series(db, website_id, property_name, &pf, &filters).await?),
    SessionSeries::Numeric => {
      json!(data::session_data_numeric_series(db, website_id, property_name, metric, &pf, &filters).await?)
    }
    SessionSeries::Property => {
      json!(data::session_data_property_series(db, website_id, property_name, &pf, &filters).await?)
    }
    SessionSeries::NumericStats => {
      data::session_data_numeric_stats(db, website_id, property_name, &pf, &filters).await?
    }
    SessionSeries::ActivityStats => {
      json!(data::session_data_activity_stats(db, website_id, property_name, &pf, &filters).await?)
    }
  };

  Ok(Json(result))
}

/// `GET .../session-data/array-series`
pub async fn session_array_series(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  session_series(state, website_id, query, auth, SessionSeries::Array).await
}

/// `GET .../session-data/date-series`
pub async fn session_date_series(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  session_series(state, website_id, query, auth, SessionSeries::Date).await
}

/// `GET .../session-data/numeric-series`
pub async fn session_numeric_series(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  session_series(state, website_id, query, auth, SessionSeries::Numeric).await
}

/// `GET .../session-data/numeric-stats`
pub async fn session_numeric_stats(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  session_series(state, website_id, query, auth, SessionSeries::NumericStats).await
}

/// `GET .../session-data/property-series`
pub async fn session_property_series(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  session_series(state, website_id, query, auth, SessionSeries::Property).await
}

/// `GET .../session-data/stats`: activity per value of a string property.
pub async fn session_activity_stats(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  session_series(state, website_id, query, auth, SessionSeries::ActivityStats).await
}

/// `GET .../session-data/properties`: the properties of the period's
/// sessions (those with `propertyName`, if given).
pub async fn session_data_properties(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let pf = pf_filters(&params);
  let (website_id, filters) = authorize(&state, auth, &website_id, SESSIONS, &params, PLAIN).await?;
  let property_name = params.get("propertyName");

  Ok(Json(json!(
    data::session_data_properties(&state.db, website_id, property_name, &pf, &filters).await?
  )))
}

/// `GET .../session-data/values`: a session property's most frequent values.
pub async fn session_data_values(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let data_type = data_type(&params)?;
  let (website_id, filters) = authorize(&state, auth, &website_id, SESSIONS, &params, PLAIN).await?;
  let property_name = params.get("propertyName");

  Ok(Json(json!(
    data::session_data_values(&state.db, website_id, property_name, data_type, &filters).await?
  )))
}
