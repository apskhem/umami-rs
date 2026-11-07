//! Report routes under `/api/websites/{websiteId}`: saved funnels and goals
//! (`funnels`, `goals`, each with `/{id}`, `stats` and `/{id}/stats`),
//! `journeys`, `retention`, `breakdown`, `utm/metrics`, `attribution`,
//! `performance/*` and `revenue/*`. Ported from upstream's
//! `src/app/api/websites/[websiteId]` and the schemas of
//! `src/lib/analytics-schema.ts`.

use axum::Json;
use axum::body::Bytes;
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
use super::params::{
  PAGING_SEARCH,
  Params,
};
use super::{
  body,
  ok,
};
use crate::analytics::date::Compare;
use crate::analytics::filters::{
  self,
  Options,
  QueryFilters,
  Range,
};
use crate::analytics::reports::{
  self,
  Attribution,
  BREAKDOWN_FIELDS,
  Funnel,
  Goal,
  Journey,
  RevenueMetric,
  UTM_COLUMNS,
  VITALS,
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
  WEBSITE_UPDATE,
  db_id,
};
use crate::queries::report::{
  self,
  NewReport,
  Report,
};
use crate::queries::website;
use crate::state::AppState;

/// Upstream's `analyticsParams`: required `startAt`/`endAt`, `timezone`,
/// `unit` and `filterParams`.
const ANALYTICS: Options = Options {
  filters: true,
  range: Range::Timestamps,
  ..Options::NONE
};

/// `withDateRange` with `filterParams` (the revenue routes).
const DATE_RANGE: Options = Options {
  filters: true,
  ..Options::NONE
};

fn bad() -> AppError {
  AppError::bad_request()
}

/// `analyticsSchema`'s checks beyond integer timestamps: both at least 0, and
/// `endAt` not before `startAt`.
pub(super) fn check_range(params: &Params) -> AppResult<()> {
  let number = |key: &str| params.get(key).map_or(f64::NAN, js::string_to_number);
  let (start, end) = (number("startAt"), number("endAt"));
  if start >= 0.0 && end >= start {
    Ok(())
  } else {
    Err(bad())
  }
}

/// A required parameter that must be one of `allowed`.
fn one_of<'a>(params: &'a Params, key: &str, allowed: &[&str]) -> AppResult<&'a str> {
  params.get(key).filter(|value| allowed.contains(value)).ok_or_else(bad)
}

/// An optional parameter that must be one of `allowed` if given.
fn optional_one_of<'a>(params: &'a Params, key: &str, allowed: &[&str]) -> AppResult<Option<&'a str>> {
  params
    .get(key)
    .map(|value| allowed.contains(&value).then_some(value).ok_or_else(bad))
    .transpose()
}

/// `jsonQuery`: a parameter holding JSON.
fn json_param(params: &Params, key: &str) -> AppResult<Value> {
  serde_json::from_str(params.get(key).ok_or_else(bad)?).map_err(|_| bad())
}

/// The analytics routes' shared start: validated parameters (400),
/// authentication and the website section (401), then the website's filters.
async fn analytics(
  state: &AppState,
  auth: AppResult<Auth>,
  website_id: &str,
  section: &str,
  params: &Params,
  options: Options,
) -> AppResult<(uuid::Uuid, QueryFilters)> {
  if matches!(options.range, Range::Timestamps) {
    check_range(params)?;
  }
  authorize(state, auth, website_id, &[section], params, options).await
}

// Saved funnels and goals

/// Which saved report type a route serves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
  Funnel,
  Goal,
}

/// The saved report routes a handler serves: [`Funnels`] or [`Goals`].
pub trait Saved {
  const KIND: Kind;
}

/// `/api/websites/{websiteId}/funnels`.
pub struct Funnels;

impl Saved for Funnels {
  const KIND: Kind = Kind::Funnel;
}

/// `/api/websites/{websiteId}/goals`.
pub struct Goals;

impl Saved for Goals {
  const KIND: Kind = Kind::Goal;
}

impl Kind {
  fn name(self) -> &'static str {
    match self {
      Self::Funnel => "funnel",
      Self::Goal => "goal",
    }
  }

  fn section(self) -> &'static str {
    match self {
      Self::Funnel => "funnels",
      Self::Goal => "goals",
    }
  }

  /// The type's parameter schema, as stored; `None` if invalid.
  fn parameters(self, value: &Value) -> Option<Value> {
    match self {
      Self::Funnel => Funnel::parse(value).map(|funnel| funnel.to_json()),
      Self::Goal => Goal::parse(value).map(|goal| goal.to_json()),
    }
  }
}

/// A report definition (`funnelDefinitionSchema`, `goalDefinitionSchema`):
/// name, optional description and the type's parameters.
fn definition(kind: Kind, body: &Bytes) -> AppResult<(String, Option<String>, Value)> {
  let body = body::parse(body);
  let object = body::object(&body)?;
  let name = body::max_len(body::string(object, "name")?, 200)?;
  if name.is_empty() {
    return Err(bad());
  }
  let description = body::optional_string(object, "description")?
    .map(|d| body::max_len(d, 500))
    .transpose()?;
  let parameters = kind
    .parameters(object.get("parameters").unwrap_or(&Value::Null))
    .ok_or_else(bad)?;

  Ok((name.to_owned(), description.map(str::to_owned), parameters))
}

/// The website's report of this type with ID `report_id`; a 404 otherwise
/// (upstream also fails with a 500 on a malformed ID).
async fn find(state: &AppState, kind: Kind, website_id: &str, report_id: &str) -> AppResult<Report> {
  let website_id = db_id(website_id)?;
  report::get(&state.db, db_id(report_id)?)
    .await?
    .filter(|report| report.website_id == website_id && report.kind == kind.name())
    .ok_or(AppError::NotFound)
}

/// `GET .../funnels`: the website's saved funnels (or goals), by name.
pub async fn list<S: Saved>(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let kind = S::KIND;
  let filters = Params::parse(query.as_deref()).filters(PAGING_SEARCH)?;
  let auth = auth?;
  can::require(can::can_view_website_section(&state.db, &auth, &website_id, &[kind.section()]).await?)?;

  Ok(Json(
    report::list(&state.db, db_id(&website_id)?, kind.name(), &filters).await?,
  ))
}

/// `POST .../funnels`: saves a funnel (or goal).
pub async fn create<S: Saved>(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let kind = S::KIND;
  let (name, description, parameters) = definition(kind, &body)?;

  let auth = auth?;
  can::require(can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await?)?;
  // Admins pass for any ID; upstream then saves a report of nothing.
  let Some(found) = website::get(&state.db, &website_id).await? else {
    return Err(AppError::NotFound);
  };

  let created = report::create(
    &state.db,
    &NewReport {
      id: state.new_id(),
      user_id: auth.user()?.id,
      website_id: found.website_id,
      kind: kind.name(),
      name: &name,
      description: description.as_deref().unwrap_or_default(),
      parameters: &parameters,
    },
  )
  .await?;

  Ok(Json(created.to_json()))
}

/// `GET .../funnels/{id}`: the saved funnel (or goal).
pub async fn get<S: Saved>(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, report_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  let kind = S::KIND;
  let found = find(&state, kind, &website_id, &report_id).await?;
  can::require(can::can_view_report(&state.db, &auth, &found).await?)?;

  Ok(Json(found.to_json()))
}

/// `POST .../funnels/{id}`: changes the saved funnel (or goal).
pub async fn update<S: Saved>(
  State(state): State<AppState>,
  Path((website_id, report_id)): Path<(String, String)>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let kind = S::KIND;
  let (name, description, parameters) = definition(kind, &body)?;

  let auth = auth?;
  let found = find(&state, kind, &website_id, &report_id).await?;
  can::require(can::can_change_report(&state.db, &auth, &found, WEBSITE_UPDATE).await?)?;

  let updated = report::update(&state.db, found.report_id, &name, description.as_deref(), &parameters)
    .await?
    .ok_or(AppError::NotFound)?;

  Ok(Json(updated.to_json()))
}

/// `DELETE .../funnels/{id}`: deletes the saved funnel (or goal).
pub async fn delete<S: Saved>(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, report_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  let kind = S::KIND;
  let found = find(&state, kind, &website_id, &report_id).await?;
  can::require(can::can_change_report(&state.db, &auth, &found, WEBSITE_DELETE).await?)?;

  if !report::delete(&state.db, found.report_id).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
}

/// The results of a funnel (or goal) over the filters.
async fn report_stats(
  state: &AppState,
  website_id: uuid::Uuid,
  parameters: &ReportParameters,
  filters: &QueryFilters,
) -> AppResult<Value> {
  match parameters {
    ReportParameters::Funnel(funnel) => Ok(json!(reports::funnel(&state.db, website_id, funnel, filters).await?)),
    ReportParameters::Goal(goal) => reports::goal(&state.db, website_id, goal, filters).await,
  }
}

enum ReportParameters {
  Funnel(Funnel),
  Goal(Goal),
}

/// `GET .../funnels/stats`: the results of an unsaved funnel (or goal) given
/// by the query.
pub async fn stats<S: Saved>(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let kind = S::KIND;
  let params = Params::parse(query.as_deref());
  let parameters = match kind {
    Kind::Funnel => {
      let window = params.get("window").map_or(f64::NAN, js::string_to_number);
      if !(window.is_finite() && window > 0.0) {
        return Err(bad());
      }
      let steps = Funnel::parse_steps(&json_param(&params, "steps")?).ok_or_else(bad)?;
      ReportParameters::Funnel(Funnel { window, steps })
    }
    Kind::Goal => {
      ReportParameters::Goal(Goal {
        kind: params.get("type").ok_or_else(bad)?.to_owned(),
        value: params.get("value").ok_or_else(bad)?.to_owned(),
      })
    }
  };
  let (website_id, filters) = analytics(&state, auth, &website_id, kind.section(), &params, ANALYTICS).await?;

  Ok(Json(report_stats(&state, website_id, &parameters, &filters).await?))
}

/// `GET .../funnels/{id}/stats`: the results of a saved funnel (or goal).
pub async fn saved_stats<S: Saved>(
  State(state): State<AppState>,
  Path((website_id, report_id)): Path<(String, String)>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let kind = S::KIND;
  let params = Params::parse(query.as_deref());
  check_range(&params)?;
  let filters = filters::from_request(&state.db, &params, ANALYTICS).await?;
  let auth = auth?;
  can::require(can::can_view_website_section(&state.db, &auth, &website_id, &[kind.section()]).await?)?;
  let found = find(&state, kind, &website_id, &report_id).await?;
  can::require(can::can_view_report(&state.db, &auth, &found).await?)?;

  let parameters = match kind {
    Kind::Funnel => Funnel::parse(&found.parameters).map(ReportParameters::Funnel),
    Kind::Goal => Goal::parse(&found.parameters).map(ReportParameters::Goal),
  }
  .ok_or_else(bad)?;
  let filters = filters.for_website(&state.db, found.website_id).await?;

  Ok(Json(
    report_stats(&state, found.website_id, &parameters, &filters).await?,
  ))
}

// Unsaved reports

/// `GET .../journeys`: the most common paths through visits.
pub async fn journeys(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  // `z.coerce.number().min(2).max(7)`; a fractional count follows whole steps.
  let steps = params.get("steps").map_or(f64::NAN, js::string_to_number);
  if !(2.0..=7.0).contains(&steps) {
    return Err(bad());
  }
  let journey = Journey {
    steps: steps as usize,
    start_step: params.get("startStep"),
    end_step: params.get("endStep"),
  };
  let (website_id, filters) = analytics(&state, auth, &website_id, "journeys", &params, ANALYTICS).await?;

  Ok(Json(json!(
    reports::journey(&state.db, website_id, &journey, &filters).await?
  )))
}

/// `GET .../retention`: daily retention per first-visit day.
pub async fn retention(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = analytics(&state, auth, &website_id, "retention", &params, ANALYTICS).await?;

  Ok(Json(json!(reports::retention(&state.db, website_id, &filters).await?)))
}

/// `GET .../breakdown`: traffic per combination of the JSON `fields`.
pub async fn breakdown(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let requested = json_param(&params, "fields")?;
  let mut fields: Vec<&str> = Vec::new();
  for field in requested.as_array().ok_or_else(bad)? {
    let field = field
      .as_str()
      .and_then(|f| BREAKDOWN_FIELDS.iter().find(|known| **known == f))
      .ok_or_else(bad)?;
    if !fields.contains(field) {
      fields.push(field);
    }
  }
  // Upstream fails with a 500 without fields.
  if fields.is_empty() {
    return Err(bad());
  }
  let (website_id, filters) = analytics(&state, auth, &website_id, "breakdown", &params, ANALYTICS).await?;

  Ok(Json(json!(
    reports::breakdown(&state.db, website_id, &fields, &filters).await?
  )))
}

/// `GET .../utm/metrics`: page views per value of a UTM parameter.
pub async fn utm_metrics(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let column = one_of(&params, "type", UTM_COLUMNS)?;
  let (website_id, filters) = analytics(&state, auth, &website_id, "utm", &params, ANALYTICS).await?;

  Ok(Json(json!(
    reports::utm(&state.db, website_id, column, &filters).await?
  )))
}

/// `GET .../attribution`: what led the sessions that converted.
pub async fn attribution(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let attribution = Attribution {
    first_click: one_of(&params, "model", &["first-click", "last-click"])? == "first-click",
    is_path: one_of(&params, "type", &["path", "event"])? == "path",
    step: params.get("step").ok_or_else(bad)?,
  };
  let (website_id, filters) = analytics(&state, auth, &website_id, "attribution", &params, ANALYTICS).await?;

  Ok(Json(
    reports::attribution(&state.db, website_id, &attribution, &filters).await?,
  ))
}

/// `GET .../performance/chart`: a web vital's percentiles over time.
pub async fn performance_chart(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let metric = optional_one_of(&params, "metric", VITALS)?.unwrap_or("lcp");
  let (website_id, filters) = analytics(&state, auth, &website_id, "performance", &params, ANALYTICS).await?;

  Ok(Json(
    reports::performance_chart(&state.db, website_id, metric, &filters).await?,
  ))
}

/// `GET .../performance/metrics`: a web vital's percentiles per page path,
/// title, device or browser.
pub async fn performance_metrics(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let metric = optional_one_of(&params, "metric", VITALS)?.unwrap_or("lcp");
  let column = match one_of(&params, "type", &["path", "title", "device", "browser"])? {
    "path" => "url_path",
    "title" => "page_title",
    other => other,
  };
  // `z.coerce.number().int().min(1).max(500)`; by default 500, or all devices.
  let limit = match params.get("limit") {
    Some(value) => {
      let n = js::string_to_number(value);
      if n.fract() != 0.0 || !(1.0..=500.0).contains(&n) {
        return Err(bad());
      }
      Some(n as i64)
    }
    None => (column != "device").then_some(500),
  };
  let (website_id, filters) = analytics(&state, auth, &website_id, "performance", &params, ANALYTICS).await?;

  Ok(Json(json!(
    reports::performance_metrics(&state.db, website_id, metric, (column, limit), &filters).await?
  )))
}

/// `GET .../performance/stats`: every web vital's percentiles.
pub async fn performance_stats(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (website_id, filters) = analytics(&state, auth, &website_id, "performance", &params, ANALYTICS).await?;

  Ok(Json(reports::performance_stats(&state.db, website_id, &filters).await?))
}

/// The revenue routes' required `currency`.
fn currency(params: &Params) -> AppResult<&str> {
  params.get("currency").ok_or_else(bad)
}

/// `GET .../revenue/chart`: revenue per event name over time.
pub async fn revenue_chart(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let currency = currency(&params)?;
  let (website_id, filters) = analytics(&state, auth, &website_id, "revenue", &params, DATE_RANGE).await?;

  Ok(Json(
    reports::revenue_chart(&state.db, website_id, currency, &filters).await?,
  ))
}

/// `GET .../revenue/metrics`: revenue per country, region, referrer or
/// channel.
pub async fn revenue_metrics(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let metric = params.get("type").and_then(RevenueMetric::parse).ok_or_else(bad)?;
  let currency = currency(&params)?;
  let (website_id, filters) = analytics(&state, auth, &website_id, "revenue", &params, DATE_RANGE).await?;

  Ok(Json(json!(
    reports::revenue_metrics(&state.db, website_id, currency, metric, &filters).await?
  )))
}

/// `GET .../revenue/sessions`: the sessions with revenue.
pub async fn revenue_sessions(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let currency = currency(&params)?;
  let options = Options {
    search: true,
    paging: true,
    ..DATE_RANGE
  };
  let (website_id, filters) = analytics(&state, auth, &website_id, "revenue", &params, options).await?;

  Ok(Json(
    reports::revenue_sessions(&state.db, website_id, currency, &filters).await?,
  ))
}

/// `GET .../revenue/stats`: revenue totals, with the comparison period's
/// (`compare`, by default the previous one).
pub async fn revenue_stats(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let currency = currency(&params)?;
  let (website_id, filters) = analytics(&state, auth, &website_id, "revenue", &params, DATE_RANGE).await?;

  let mut stats = reports::revenue_stats(&state.db, website_id, currency, &filters).await?;
  let (start, end) = filters
    .compare
    .unwrap_or(Compare::Prev)
    .range(filters.start_date, filters.end_date);
  let comparison = reports::revenue_stats(&state.db, website_id, currency, &filters.with_range(start, end)).await?;
  if let Value::Object(object) = &mut stats {
    object.insert("comparison".to_owned(), comparison);
  }

  Ok(Json(stats))
}
