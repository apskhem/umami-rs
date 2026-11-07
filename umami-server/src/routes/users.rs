//! `/api/users/*`, ported from upstream's `src/app/api/users`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use serde_json::Value;

use super::params::{
  PAGING_SEARCH_SORTING,
  PAGING_SORTING,
  Params,
};
use super::{
  body,
  ok,
};
use crate::auth::{
  Auth,
  password,
};
use crate::error::{
  AppError,
  AppResult,
};
use crate::permissions::{
  self as can,
  ROLE_ADMIN,
  ROLE_USER,
  ROLE_VIEW_ONLY,
  db_id,
};
use crate::queries::{
  team,
  user,
  website,
};
use crate::state::AppState;

const USER_EXISTS: &str = "User already exists";

/// Upstream's `userRoleParam`.
const USER_ROLES: &[&str] = &[ROLE_ADMIN, ROLE_USER, ROLE_VIEW_ONLY];

/// `POST /api/users`: creates a user (admins only).
pub async fn create(State(state): State<AppState>, auth: AppResult<Auth>, body: Bytes) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let id = body::optional_string(object, "id")?.map(body::uuid).transpose()?;
  let username = body::max_len(body::string(object, "username")?, 255)?;
  let password = body::max_len(body::string(object, "password")?, 255)?;
  if body::js_len(password) < 8 {
    return Err(AppError::bad_request());
  }
  let role = body::one_of(body::string(object, "role")?, USER_ROLES)?;

  let auth = auth?;
  can::require(can::can_create_user(&auth))?;

  if user::username_owner(&state.db, username).await?.is_some() {
    return Err(AppError::BadRequest(USER_EXISTS.to_owned()));
  }

  let id = id.unwrap_or_else(|| state.new_id());
  let hash = password::hash(password).await;
  // A taken ID fails here (a 500 upstream).
  let created = user::create(&state.db, id, &username.to_lowercase(), &hash, role)
    .await
    .map_err(|e| AppError::taken(e, USER_EXISTS))?;

  Ok(Json(created))
}

/// `GET /api/users/{userId}`: the user, or `null` if there is none.
pub async fn get(State(state): State<AppState>, auth: Auth, Path(user_id): Path<String>) -> AppResult<Json<Value>> {
  can::require(can::can_view_user(&auth, &user_id))?;

  let found = user::get(&state.db, db_id(&user_id)?).await?;

  Ok(Json(found.as_ref().map_or(Value::Null, user::User::to_json)))
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
  can::require(can::can_view_user(&auth, &user_id))?;
  let is_admin = auth.user()?.is_admin;

  let id = db_id(&user_id)?;
  let Some(existing) = user::get(&state.db, id).await? else {
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

  // Deleted users keep their usernames, which upstream misses (failing with
  // a 500 on the unique constraint).
  if let Some(new_username) = &changes.username
    && *new_username != existing.username
    && user::username_owner(&state.db, new_username)
      .await?
      .is_some_and(|owner| owner != id)
  {
    return Err(AppError::BadRequest(USER_EXISTS.to_owned()));
  }

  let updated = user::update(&state.db, id, &changes)
    .await
    .map_err(|e| AppError::taken(e, USER_EXISTS))?;

  Ok(Json(updated))
}

/// `DELETE /api/users/{userId}`: deletes a user and everything they own (admins
/// only).
pub async fn delete(State(state): State<AppState>, auth: Auth, Path(user_id): Path<String>) -> AppResult<Json<Value>> {
  can::require(can::can_delete_user(&auth))?;
  if auth.user()?.id.to_string() == user_id {
    return Err(AppError::BadRequest("You cannot delete yourself.".to_owned()));
  }

  if !user::delete(&state.db, db_id(&user_id)?).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
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
  let scope = website::Scope::for_user(id, params.flag("includeTeams"));

  Ok(Json(website::list(&state.db, scope, filters).await?))
}
