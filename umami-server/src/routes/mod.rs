use axum::extract::{
  Request,
  State,
};
use axum::http::{
  HeaderMap,
  HeaderName,
  HeaderValue,
  Method,
  StatusCode,
  header,
};
use axum::middleware::Next;
use axum::response::{
  IntoResponse,
  Response,
};
use axum::routing::{
  delete,
  get,
  post,
};
use axum::{
  Json,
  Router,
};
use serde_json::{
  Value,
  json,
};

use self::links::{
  Links,
  Pixels,
};
use self::reports::{
  Funnels,
  Goals,
};
use crate::collect;
use crate::config::Config;
use crate::state::AppState;
use crate::tracker::TrackerScript;

mod admin;
mod analytics;
mod annotations;
mod auth;
mod boards;
mod body;
mod charts;
mod config;
mod data;
mod heartbeat;
mod links;
mod me;
pub(crate) mod params;
mod reports;
mod segments;
mod shares;
mod teams;
mod two_factor;
mod users;
mod websites;

/// Upstream's `ok()`: `{"ok": true}`.
fn ok() -> Json<Value> {
  Json(json!({ "ok": true }))
}

/// Routes relative to `BASE_PATH`. API paths mirror upstream's `src/app/api/**/route.ts`.
pub fn router(config: &Config) -> Router<AppState> {
  let mut router = Router::new()
    .route("/api/heartbeat", get(heartbeat::get))
    .route("/api/config", get(config::get))
    .route("/api/auth/login", post(auth::login))
    .route("/api/auth/verify", post(auth::verify))
    .route("/api/auth/logout", post(auth::logout))
    .route("/api/auth/sso", post(auth::sso))
    .route("/api/me", get(me::get))
    .route("/api/me/password", post(me::change_password))
    .route("/api/me/api-keys", get(me::list_api_keys).post(me::create_api_key))
    .route("/api/me/api-keys/{keyId}", delete(me::delete_api_key))
    .route("/api/me/teams", get(me::teams))
    .route("/api/me/websites", get(me::websites))
    .route("/api/users", post(users::create))
    .route(
      "/api/users/{userId}",
      get(users::get).post(users::update).delete(users::delete),
    )
    .route("/api/users/{userId}/teams", get(users::teams))
    .route("/api/users/{userId}/websites", get(users::websites))
    .route("/api/teams", get(teams::list).post(teams::create))
    .route("/api/teams/join", post(teams::join))
    .route(
      "/api/teams/{teamId}",
      get(teams::get).post(teams::update).delete(teams::delete),
    )
    .route("/api/teams/{teamId}/users", get(teams::members).post(teams::add_member))
    .route(
      "/api/teams/{teamId}/users/{userId}",
      get(teams::member)
        .post(teams::update_member)
        .delete(teams::remove_member),
    )
    .route("/api/teams/{teamId}/websites", get(teams::websites))
    .route("/api/websites", get(websites::list).post(websites::create))
    .route(
      "/api/websites/{websiteId}",
      get(websites::get).post(websites::update).delete(websites::delete),
    )
    .route("/api/websites/{websiteId}/stats", get(analytics::stats))
    .route("/api/websites/{websiteId}/pageviews", get(analytics::pageviews))
    .route("/api/websites/{websiteId}/metrics", get(analytics::metrics))
    .route(
      "/api/websites/{websiteId}/metrics/expanded",
      get(analytics::expanded_metrics),
    )
    .route("/api/websites/{websiteId}/active", get(analytics::active))
    .route("/api/websites/{websiteId}/daterange", get(analytics::date_range))
    .route("/api/websites/{websiteId}/values", get(analytics::values))
    .route("/api/websites/{websiteId}/events", get(analytics::events))
    .route("/api/websites/{websiteId}/events/series", get(analytics::event_series))
    .route("/api/websites/{websiteId}/events/stats", get(analytics::event_stats))
    .route("/api/websites/{websiteId}/sessions", get(analytics::sessions))
    .route(
      "/api/websites/{websiteId}/sessions/stats",
      get(analytics::session_stats),
    )
    .route("/api/websites/{websiteId}/sessions/weekly", get(analytics::weekly))
    .route(
      "/api/websites/{websiteId}/sessions/{sessionId}",
      get(analytics::session).delete(analytics::delete_session),
    )
    .route(
      "/api/websites/{websiteId}/sessions/{sessionId}/activity",
      get(analytics::session_activity),
    )
    .route(
      "/api/websites/{websiteId}/sessions/{sessionId}/properties",
      get(analytics::session_properties),
    )
    .route("/api/websites/{websiteId}/event-data", get(data::event_data))
    .route(
      "/api/websites/{websiteId}/event-data/{eventId}",
      get(data::event_data_by_id),
    )
    .route(
      "/api/websites/{websiteId}/event-data/events",
      get(data::event_data_events),
    )
    .route(
      "/api/websites/{websiteId}/event-data/fields",
      get(data::event_data_fields),
    )
    .route(
      "/api/websites/{websiteId}/event-data/properties",
      get(data::event_data_properties),
    )
    .route(
      "/api/websites/{websiteId}/event-data/stats",
      get(data::event_data_stats),
    )
    .route(
      "/api/websites/{websiteId}/event-data/values",
      get(data::event_data_values),
    )
    .route(
      "/api/websites/{websiteId}/event-data-pivot",
      get(data::event_data_pivot),
    )
    .route(
      "/api/websites/{websiteId}/event-data-pivot/array-series",
      get(data::event_array_series),
    )
    .route(
      "/api/websites/{websiteId}/event-data-pivot/date-series",
      get(data::event_date_series),
    )
    .route(
      "/api/websites/{websiteId}/event-data-pivot/numeric-series",
      get(data::event_numeric_series),
    )
    .route(
      "/api/websites/{websiteId}/event-data-pivot/numeric-stats",
      get(data::event_numeric_stats),
    )
    .route(
      "/api/websites/{websiteId}/event-data-pivot/property-series",
      get(data::event_property_series),
    )
    .route(
      "/api/websites/{websiteId}/session-data-pivot",
      get(data::session_data_pivot),
    )
    .route(
      "/api/websites/{websiteId}/session-data/array-series",
      get(data::session_array_series),
    )
    .route(
      "/api/websites/{websiteId}/session-data/date-series",
      get(data::session_date_series),
    )
    .route(
      "/api/websites/{websiteId}/session-data/numeric-series",
      get(data::session_numeric_series),
    )
    .route(
      "/api/websites/{websiteId}/session-data/numeric-stats",
      get(data::session_numeric_stats),
    )
    .route(
      "/api/websites/{websiteId}/session-data/properties",
      get(data::session_data_properties),
    )
    .route(
      "/api/websites/{websiteId}/session-data/property-series",
      get(data::session_property_series),
    )
    .route(
      "/api/websites/{websiteId}/session-data/stats",
      get(data::session_activity_stats),
    )
    .route(
      "/api/websites/{websiteId}/session-data/values",
      get(data::session_data_values),
    )
    .route(
      "/api/websites/{websiteId}/funnels",
      get(reports::list::<Funnels>).post(reports::create::<Funnels>),
    )
    .route(
      "/api/websites/{websiteId}/funnels/stats",
      get(reports::stats::<Funnels>),
    )
    .route(
      "/api/websites/{websiteId}/funnels/{funnelId}",
      get(reports::get::<Funnels>)
        .post(reports::update::<Funnels>)
        .delete(reports::delete::<Funnels>),
    )
    .route(
      "/api/websites/{websiteId}/funnels/{funnelId}/stats",
      get(reports::saved_stats::<Funnels>),
    )
    .route(
      "/api/websites/{websiteId}/goals",
      get(reports::list::<Goals>).post(reports::create::<Goals>),
    )
    .route("/api/websites/{websiteId}/goals/stats", get(reports::stats::<Goals>))
    .route(
      "/api/websites/{websiteId}/goals/{goalId}",
      get(reports::get::<Goals>)
        .post(reports::update::<Goals>)
        .delete(reports::delete::<Goals>),
    )
    .route(
      "/api/websites/{websiteId}/goals/{goalId}/stats",
      get(reports::saved_stats::<Goals>),
    )
    .route("/api/websites/{websiteId}/journeys", get(reports::journeys))
    .route("/api/websites/{websiteId}/retention", get(reports::retention))
    .route("/api/websites/{websiteId}/breakdown", get(reports::breakdown))
    .route("/api/websites/{websiteId}/utm/metrics", get(reports::utm_metrics))
    .route("/api/websites/{websiteId}/attribution", get(reports::attribution))
    .route(
      "/api/websites/{websiteId}/performance/chart",
      get(reports::performance_chart),
    )
    .route(
      "/api/websites/{websiteId}/performance/metrics",
      get(reports::performance_metrics),
    )
    .route(
      "/api/websites/{websiteId}/performance/stats",
      get(reports::performance_stats),
    )
    .route("/api/websites/{websiteId}/revenue/chart", get(reports::revenue_chart))
    .route(
      "/api/websites/{websiteId}/revenue/metrics",
      get(reports::revenue_metrics),
    )
    .route(
      "/api/websites/{websiteId}/revenue/sessions",
      get(reports::revenue_sessions),
    )
    .route("/api/websites/{websiteId}/revenue/stats", get(reports::revenue_stats))
    .route(
      "/api/websites/{websiteId}/annotations",
      get(annotations::list).post(annotations::create),
    )
    .route(
      "/api/websites/{websiteId}/annotations/{annotationId}",
      get(annotations::get)
        .post(annotations::update)
        .delete(annotations::delete),
    )
    .route("/api/realtime/{websiteId}", get(analytics::realtime))
    .route("/api/websites/{websiteId}/reset", post(websites::reset))
    .route(
      "/api/websites/{websiteId}/shares",
      get(shares::website_list).post(shares::website_create),
    )
    .route(
      "/api/websites/{websiteId}/segments",
      get(segments::list).post(segments::create),
    )
    .route(
      "/api/websites/{websiteId}/segments/{segmentId}",
      get(segments::get).post(segments::update).delete(segments::delete),
    )
    .route("/api/share", post(shares::create))
    .route("/api/share/{slug}", get(shares::resolve))
    .route(
      "/api/share/id/{shareId}",
      get(shares::get).post(shares::update).delete(shares::delete),
    )
    .route("/api/websites/{websiteId}/transfer", post(websites::transfer))
    .route("/api/admin/users", get(admin::users))
    .route("/api/admin/teams", get(admin::teams))
    .route("/api/admin/websites", get(admin::websites))
    .route("/api/2fa/status", get(two_factor::status))
    .route("/api/2fa/setup/initiate", post(two_factor::initiate))
    .route("/api/2fa/setup/confirm", post(two_factor::confirm))
    .route("/api/2fa/setup/cancel", post(two_factor::cancel))
    .route("/api/2fa/disable", post(two_factor::disable))
    .route("/api/2fa/verify", post(two_factor::verify))
    .route("/api/admin/2fa/global", post(two_factor::set_global))
    .route(
      "/api/admin/users/{userId}/2fa",
      get(two_factor::user_status)
        .post(two_factor::set_user)
        .delete(two_factor::reset_user),
    )
    .route("/api/admin/teams/{teamId}/2fa", post(two_factor::set_team))
    .route("/api/websites/charts", get(websites::charts))
    .route("/api/boards", get(boards::list).post(boards::create))
    .route(
      "/api/boards/{boardId}",
      get(boards::get).post(boards::update).delete(boards::delete),
    )
    .route("/api/boards/{boardId}/clone", post(boards::clone))
    .route(
      "/api/boards/{boardId}/shares",
      get(boards::share_list).post(boards::share_create),
    )
    .route("/api/teams/{teamId}/boards", get(boards::team_list))
    .route("/api/dashboard", get(boards::dashboard).post(boards::save_dashboard))
    .route("/api/links", get(links::list::<Links>).post(links::create::<Links>))
    .route("/api/links/charts", get(links::charts::<Links>))
    .route(
      "/api/links/{linkId}",
      get(links::get::<Links>)
        .post(links::update::<Links>)
        .delete(links::delete::<Links>),
    )
    .route(
      "/api/links/{linkId}/shares",
      get(links::share_list::<Links>).post(links::share_create::<Links>),
    )
    .route("/api/teams/{teamId}/links", get(links::team_list::<Links>))
    .route("/api/pixels", get(links::list::<Pixels>).post(links::create::<Pixels>))
    .route("/api/pixels/charts", get(links::charts::<Pixels>))
    .route(
      "/api/pixels/{pixelId}",
      get(links::get::<Pixels>)
        .post(links::update::<Pixels>)
        .delete(links::delete::<Pixels>),
    )
    .route(
      "/api/pixels/{pixelId}/shares",
      get(links::share_list::<Pixels>).post(links::share_create::<Pixels>),
    )
    .route("/api/teams/{teamId}/pixels", get(links::team_list::<Pixels>))
    .route("/q/{slug}", get(collect::link))
    .route("/p/{slug}", get(collect::pixel))
    .route("/api/send", post(collect::send))
    .route("/api/batch", post(collect::batch));

  if let Some(alias) = config.collect.endpoint_alias.as_deref()
    && alias != "/api/send"
    && alias != "/api/batch"
  {
    router = router.route(alias, post(collect::send));
  }

  let script = TrackerScript::new(config.collect.endpoint_alias.as_deref());
  let serve_script = get(move |headers: HeaderMap| async move { script.respond(&headers) });

  for path in std::iter::once("/script.js").chain(config.tracker_script_aliases.iter().map(String::as_str)) {
    router = router.route(path, serve_script.clone());
  }

  router
}

/// Upstream's API response headers (`next.config.ts`): permissive CORS so trackers
/// on any site can post events, and no caching. `OPTIONS` preflights are answered
/// directly, as Next.js does for route handlers.
pub async fn api_headers(State(state): State<AppState>, request: Request, next: Next) -> Response {
  let path = request.uri().path();
  let is_api = path.starts_with("/api/") || state.config.collect.endpoint_alias.as_deref() == Some(path);

  if !is_api {
    return next.run(request).await;
  }

  let mut response = if request.method() == Method::OPTIONS {
    StatusCode::NO_CONTENT.into_response()
  } else {
    next.run(request).await
  };

  let max_age = HeaderValue::from_str(&state.config.cors_max_age).unwrap_or_else(|_| HeaderValue::from_static("86400"));
  let headers: [(HeaderName, HeaderValue); 5] = [
    (header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*")),
    (header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("*")),
    (
      header::ACCESS_CONTROL_ALLOW_METHODS,
      HeaderValue::from_static("GET, DELETE, POST, PUT"),
    ),
    (header::ACCESS_CONTROL_MAX_AGE, max_age),
    (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
  ];

  for (name, value) in headers {
    response.headers_mut().entry(name).or_insert(value);
  }

  response
}
