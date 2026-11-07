//! `GET /api/scripts/telemetry` (and `/telemetry.js`, upstream's rewrite of
//! it): the dashboard's anonymous usage ping, ported from upstream's
//! `src/app/api/scripts/telemetry/route.ts`.

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;

use crate::state::AppState;

/// Upstream's `TELEMETRY_PIXEL`.
const PIXEL: &str = "https://i.umami.is/a.png";

/// A script that loads upstream's telemetry pixel with the Umami version, or
/// an empty one under `DISABLE_TELEMETRY` or `PRIVATE_MODE`.
pub async fn script(State(state): State<AppState>) -> impl IntoResponse {
  let public = &state.config.public;
  let body = if public.telemetry_disabled || public.private_mode {
    "/* telemetry disabled */".to_owned()
  } else {
    // Upstream's template with its whitespace runs removed.
    format!(
      "(()=>{{const i=document.createElement('img');i.setAttribute('src','{PIXEL}?v={}');\
       i.setAttribute('style','width:0;height:0;position:absolute;pointer-events:none;');\
       document.body.appendChild(i);}})();",
      umami_db::UMAMI_VERSION
    )
  };

  ([(header::CONTENT_TYPE, "text/javascript")], body)
}
