use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::state::AppState;

/// `GET /api/config`: settings the dashboard needs before signing in. Unset
/// optional values are omitted, as upstream's JSON leaves out `undefined`.
pub async fn get(State(state): State<AppState>) -> Json<Value> {
    let public = &state.config.public;
    let mut config = json!({
        "cloudMode": false,
        "faviconUrl": public.favicon_url,
        "linksUrl": public.links_url,
        "pixelsUrl": public.pixels_url,
        "privateMode": public.private_mode,
        "sessionDeletionEnabled": true,
        "telemetryDisabled": public.telemetry_disabled,
        "trackerScriptName": public.tracker_script_name,
        "updatesDisabled": public.updates_disabled,
    });

    config
        .as_object_mut()
        .unwrap()
        .retain(|_, value| !value.is_null());

    Json(config)
}
