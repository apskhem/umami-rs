//! `/api/users/*`, ported from upstream's `src/app/api/users`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use serde_json::{Value, json};

use super::body;
use super::params::{PAGING_SEARCH_SORTING, PAGING_SORTING, Params};
use crate::auth::{Auth, password, store};
use crate::crypto;
use crate::error::{AppError, AppResult};
use crate::permissions::{self as can, ROLE_ADMIN, ROLE_USER, ROLE_VIEW_ONLY, db_id};
use crate::queries::{team, user, website};
use crate::state::AppState;

/// Upstream's `userRoleParam`.
const USER_ROLES: &[&str] = &[ROLE_ADMIN, ROLE_USER, ROLE_VIEW_ONLY];

/// `POST /api/users`: creates a user (admins only).
pub async fn create(
    State(state): State<AppState>,
    auth: AppResult<Auth>,
    body: Bytes,
) -> AppResult<Json<Value>> {
    let body = body::parse(&body);
    let object = body::object(&body)?;
    let id = body::optional_string(object, "id")?
        .map(body::uuid)
        .transpose()?;
    let username = body::max_len(body::string(object, "username")?, 255)?;
    let password = body::max_len(body::string(object, "password")?, 255)?;
    if body::js_len(password) < 8 {
        return Err(AppError::bad_request());
    }
    let role = body::one_of(body::string(object, "role")?, USER_ROLES)?;

    let auth = auth?;
    if !can::can_create_user(&auth) {
        return Err(AppError::Unauthorized);
    }

    if user::username_taken(&state.db, username).await? {
        return Err(AppError::BadRequest("User already exists".to_owned()));
    }

    let id = id.unwrap_or_else(|| crypto::random_uuid(state.config.collect.use_uuidv7));
    let hash = password::hash(password).await;
    let created = user::create(&state.db, id, &username.to_lowercase(), &hash, role).await?;

    Ok(Json(created))
}

/// `GET /api/users/{userId}`: the user, or `null` if there is none.
pub async fn get(
    State(state): State<AppState>,
    auth: Auth,
    Path(user_id): Path<String>,
) -> AppResult<Json<Value>> {
    if !can::can_view_user(&auth, &user_id) {
        return Err(AppError::Unauthorized);
    }

    let found = store::user(&state.db, db_id(&user_id)?).await?;

    Ok(Json(found.as_ref().map_or(Value::Null, user::to_json)))
}

/// `POST /api/users/{userId}`: changes the password (anyone, for themself) or the
/// username and role (admins).
pub async fn update(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    auth: AppResult<Auth>,
    body: Bytes,
) -> AppResult<Json<Value>> {
    let body = body::parse(&body);
    let object = body::object(&body)?;
    let username = body::optional_string(object, "username")?
        .map(|s| body::max_len(s, 255))
        .transpose()?;
    let password = body::optional_string(object, "password")?
        .map(|s| body::max_len(s, 255))
        .transpose()?;
    if password.is_some_and(|p| body::js_len(p) < 8) {
        return Err(AppError::bad_request());
    }
    let role = body::optional_string(object, "role")?
        .map(|s| body::one_of(s, USER_ROLES))
        .transpose()?;

    let auth = auth?;
    if !can::can_view_user(&auth, &user_id) {
        return Err(AppError::Unauthorized);
    }
    let is_admin = auth.user()?.is_admin;

    let id = db_id(&user_id)?;
    let Some(existing) = store::user(&state.db, id).await? else {
        return Err(AppError::NotFound);
    };

    let mut changes = user::UserUpdate::default();
    if let Some(password) = password.filter(|p| !p.is_empty()) {
        changes.password_hash = Some(password::hash(password).await);
    }
    if is_admin {
        changes.role = role.map(str::to_owned);
        changes.username = username.filter(|u| !u.is_empty()).map(str::to_lowercase);
    }

    if let Some(new_username) = &changes.username
        && *new_username != existing.username
        && let Some(other) = store::user_by_username(&state.db, new_username).await?
        && other.user_id != id
    {
        return Err(AppError::BadRequest("User already exists".to_owned()));
    }

    Ok(Json(user::update(&state.db, id, &changes).await?))
}

/// `DELETE /api/users/{userId}`: deletes a user and everything they own (admins
/// only).
pub async fn delete(
    State(state): State<AppState>,
    auth: Auth,
    Path(user_id): Path<String>,
) -> AppResult<Json<Value>> {
    if !can::can_delete_user(&auth) {
        return Err(AppError::Unauthorized);
    }
    if auth.user()?.id.to_string() == user_id {
        return Err(AppError::BadRequest(
            "You cannot delete yourself.".to_owned(),
        ));
    }

    if !user::delete(&state.db, db_id(&user_id)?).await? {
        return Err(AppError::NotFound);
    }

    Ok(Json(json!({ "ok": true })))
}

/// Upstream's check on `/api/users/{userId}/*` lists: admins, or the user themself.
fn require_self_or_admin(auth: &Auth, user_id: &str) -> AppResult<()> {
    let user = auth.user()?;
    if user.id.to_string() != user_id && !user.is_admin {
        return Err(AppError::Unauthorized);
    }
    Ok(())
}

/// `GET /api/users/{userId}/teams`
pub async fn teams(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    RawQuery(query): RawQuery,
    auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
    let filters = Params::parse(query.as_deref()).filters(PAGING_SORTING)?;
    let auth = auth?;
    require_self_or_admin(&auth, &user_id)?;

    let scope = team::Scope::Member(db_id(&user_id)?);

    Ok(Json(team::list(&state.db, scope, filters).await?))
}

/// `GET /api/users/{userId}/websites`: the user's websites, or with
/// `includeTeams` also those of teams they own or manage.
pub async fn websites(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    RawQuery(query): RawQuery,
    auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
    let params = Params::parse(query.as_deref());
    let filters = params.filters(PAGING_SEARCH_SORTING)?;
    let auth = auth?;
    require_self_or_admin(&auth, &user_id)?;

    let id = db_id(&user_id)?;
    let scope = website_scope(&params, id);

    Ok(Json(website::list(&state.db, scope, filters).await?))
}

/// The website list scope for `includeTeams` (any non-empty value).
pub fn website_scope(params: &Params, user_id: uuid::Uuid) -> website::Scope {
    if params.get("includeTeams").is_some_and(|v| !v.is_empty()) {
        website::Scope::WithTeamAccess(user_id)
    } else {
        website::Scope::Owned(user_id)
    }
}
