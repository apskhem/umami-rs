//! Website analytics routes under `/api/websites/{websiteId}`, ported from
//! upstream's `src/app/api/websites/[websiteId]` (stats, pageviews, metrics,
//! active visitors, date range and filter values).

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde_json::{Value, json};
use uuid::Uuid;

use super::params::Params;
use crate::analytics::date::Compare;
use crate::analytics::filters::{
    self, EVENT_COLUMNS, FilterValue, Options, QueryFilters, SESSION_COLUMNS,
};
use crate::analytics::queries::{self, DEFAULT_LIMIT, Page};
use crate::auth::Auth;
use crate::error::{AppError, AppResult};
use crate::json as js;
use crate::permissions::{self as can, db_id};
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
    if !can::can_view_website_section(&state.db, &auth, website_id, sections).await? {
        return Err(AppError::Unauthorized);
    }

    let website_id = db_id(website_id)?;
    let filters = filters.for_website(&state.db, website_id).await?;

    Ok((website_id, filters))
}

const FILTERS: Options = Options {
    search: false,
    filters: true,
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
    let (website_id, filters) =
        authorize(&state, auth, &website_id, OVERVIEW, &params, FILTERS).await?;

    let mut data = queries::website_stats(&state.db, website_id, &filters).await?;
    let (start, end) = filters
        .compare
        .unwrap_or(Compare::Prev)
        .range(filters.start_date, filters.end_date);
    let comparison =
        queries::website_stats(&state.db, website_id, &filters.with_range(start, end)).await?;

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
    let (website_id, filters) =
        authorize(&state, auth, &website_id, OVERVIEW, &params, FILTERS).await?;

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
        return Ok(Json(
            json!({ "pageviews": pageviews, "sessions": sessions }),
        ));
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
        let number = crate::collect::js::string_to_number(value);
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
    let (website_id, mut filters) =
        authorize(&state, auth, &website_id, BREAKDOWNS, &params, METRICS).await?;
    let db = &state.db;

    let rows = match metric(&kind, &mut filters).ok_or_else(AppError::bad_request)? {
        Metric::Session => queries::session_metrics(db, website_id, &kind, page, &filters).await?,
        Metric::Event => queries::event_metrics(db, website_id, &kind, page, &filters).await?,
        Metric::Pageview => {
            queries::pageview_metrics(db, website_id, &kind, page, &filters).await?
        }
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
    let (website_id, mut filters) =
        authorize(&state, auth, &website_id, OVERVIEW, &params, METRICS).await?;
    let db = &state.db;

    let rows = match metric(&kind, &mut filters).ok_or_else(AppError::bad_request)? {
        Metric::Session => {
            queries::session_expanded_metrics(db, website_id, &kind, page, &filters).await?
        }
        Metric::Event => {
            queries::event_expanded_metrics(db, website_id, &kind, page, &filters).await?
        }
        Metric::Pageview => {
            queries::pageview_expanded_metrics(db, website_id, &kind, page, &filters).await?
        }
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
    if !can::can_view_website_section(&state.db, &auth, &website_id, &sections).await? {
        return Err(AppError::Unauthorized);
    }

    Ok(Json(
        queries::active_visitors(&state.db, db_id(&website_id)?).await?,
    ))
}

/// `GET /api/websites/{websiteId}/daterange`: when the website's data starts
/// and ends.
pub async fn date_range(
    State(state): State<AppState>,
    auth: Auth,
    Path(website_id): Path<String>,
) -> AppResult<Json<Value>> {
    if !can::can_view_shared_website(&state.db, &auth, &website_id).await? {
        return Err(AppError::Unauthorized);
    }

    Ok(Json(
        queries::date_range(&state.db, db_id(&website_id)?).await?,
    ))
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
        filters: false,
    };
    let (website_id, filters) =
        authorize(&state, auth, &website_id, BREAKDOWNS, &params, options).await?;

    let column = filters::filter_column(kind).expect("every value field has a column");
    let rows = queries::values(&state.db, website_id, column, &filters).await?;

    Ok(Json(Value::Array(rows)))
}
