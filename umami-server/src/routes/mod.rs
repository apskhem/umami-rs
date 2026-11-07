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

use crate::collect;
use crate::config::Config;
use crate::state::AppState;
use crate::tracker::TrackerScript;

mod admin;
mod analytics;
mod auth;
mod body;
mod config;
mod heartbeat;
mod me;
pub(crate) mod params;
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
