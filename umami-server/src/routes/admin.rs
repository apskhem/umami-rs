//! The admin lists under `/api/admin`, ported from upstream's `src/app/api/admin`.

use axum::Json;
use axum::extract::{RawQuery, State};
use serde_json::Value;

use super::params::{PAGING_SEARCH_SORTING, Params};
use crate::auth::Auth;
use crate::error::{AppError, AppResult};
use crate::permissions as can;
use crate::queries::{team, user, website};
use crate::state::AppState;

/// Validates the list parameters, then requires `allowed` (upstream's order).
fn admin_filters(
    query: Option<&str>,
    auth: AppResult<Auth>,
    allowed: fn(&Auth) -> bool,
) -> AppResult<crate::queries::Filters> {
    let filters = Params::parse(query).filters(PAGING_SEARCH_SORTING)?;
    if !allowed(&auth?) {
        return Err(AppError::Unauthorized);
    }
    Ok(filters)
}

/// `GET /api/admin/users`
pub async fn users(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
    let filters = admin_filters(query.as_deref(), auth, can::can_view_users)?;

    Ok(Json(user::list(&state.db, filters).await?))
}

/// `GET /api/admin/teams`: every team, deleted ones included.
pub async fn teams(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
    let filters = admin_filters(query.as_deref(), auth, can::can_view_all_teams)?;

    Ok(Json(
        team::list(&state.db, team::Scope::All, filters).await?,
    ))
}

/// `GET /api/admin/websites`
pub async fn websites(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
    let filters = admin_filters(query.as_deref(), auth, can::can_view_all_websites)?;

    Ok(Json(
        website::list(&state.db, website::Scope::All, filters).await?,
    ))
}
