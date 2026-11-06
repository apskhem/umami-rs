use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::collect;
use crate::config::Config;
use crate::state::AppState;
use crate::tracker::TrackerScript;

mod heartbeat;

/// Routes relative to `BASE_PATH`. API paths mirror upstream's `src/app/api/**/route.ts`.
pub fn router(config: &Config) -> Router<AppState> {
    let mut router = Router::new()
        .route("/api/heartbeat", get(heartbeat::get))
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

    for path in std::iter::once("/script.js")
        .chain(config.tracker_script_aliases.iter().map(String::as_str))
    {
        router = router.route(path, serve_script.clone());
    }

    router
}

/// Upstream's API response headers (`next.config.ts`): permissive CORS so trackers
/// on any site can post events, and no caching. `OPTIONS` preflights are answered
/// directly, as Next.js does for route handlers.
pub async fn api_headers(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let is_api =
        path.starts_with("/api/") || state.config.collect.endpoint_alias.as_deref() == Some(path);

    if !is_api {
        return next.run(request).await;
    }

    let mut response = if request.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };

    let max_age = HeaderValue::from_str(&state.config.cors_max_age)
        .unwrap_or_else(|_| HeaderValue::from_static("86400"));
    let headers: [(HeaderName, HeaderValue); 5] = [
        (
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        ),
        (
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("*"),
        ),
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
