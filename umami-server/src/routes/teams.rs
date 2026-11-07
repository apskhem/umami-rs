//! `/api/teams/*`, ported from upstream's `src/app/api/teams`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use axum::http::StatusCode;
use serde_json::{
  Value,
  json,
};

use super::params::{
  PAGING_SEARCH,
  PAGING_SEARCH_SORTING,
  PAGING_SORTING,
  Params,
};
use super::{
  body,
  ok,
};
use crate::auth::Auth;
use crate::crypto;
use crate::error::{
  AppError,
  AppResult,
};
use crate::permissions::{
  self as can,
  TEAM_DELETE,
  TEAM_UPDATE,
  db_id,
  team_role_rank,
};
use crate::queries::{
  team,
  user,
  website,
};
use crate::state::AppState;

/// Upstream's `teamRoleParam`: the roles a member can be given (not owner).
const TEAM_ROLES: &[&str] = &[team::ROLE_MEMBER, "team-view-only", team::ROLE_MANAGER];

const NOT_OWNER_OR_MANAGER: &str = "You must be the owner/manager of this team.";
const NOT_MEMBER: &str = "The User does not exists on this team.";

/// `GET /api/teams`: the caller's teams.
pub async fn list(
  State(state): State<AppState>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let filters = Params::parse(query.as_deref()).filters(PAGING_SORTING)?;
  let user_id = auth?.user()?.id;

  Ok(Json(
    team::list(&state.db, team::Scope::Member(user_id), filters).await?,
  ))
}

/// `POST /api/teams`: creates a team owned by the caller (or, for admins, by
/// `ownerId`). Returns `[team, ownerMembership]`.
pub async fn create(State(state): State<AppState>, auth: AppResult<Auth>, body: Bytes) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let name = body::max_len(body::string(object, "name")?, 50)?;
  let owner_id = body::optional_string(object, "ownerId")?.map(body::uuid).transpose()?;

  let auth = auth?;
  can::require(can::can_create_team(&auth))?;
  let user = auth.user()?;
  let owner = owner_id.filter(|_| user.is_admin).unwrap_or(user.id);
  // Upstream accepts an owner that does not exist.
  if owner != user.id && user::get(&state.db, owner).await?.is_none() {
    return Err(AppError::user_not_found());
  }

  let access_code = format!("team_{}", crypto::random_chars(16));
  let (created, member) = team::create(&state.db, state.new_id(), name, &access_code, (state.new_id(), owner)).await?;

  Ok(Json(json!([created.to_json(), member.to_json()])))
}

/// `POST /api/teams/join`: joins a team by its access code, as a member.
pub async fn join(State(state): State<AppState>, auth: AppResult<Auth>, body: Bytes) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let access_code = body::max_len(body::string(body::object(&body)?, "accessCode")?, 50)?;
  let user_id = auth?.user()?.id;

  let Some(found) = team::by_access_code(&state.db, access_code).await? else {
    return Err(AppError::coded(
      StatusCode::NOT_FOUND,
      "team-not-found",
      "Team not found.",
    ));
  };
  if team::team_user(&state.db, found.team_id, user_id).await?.is_some() {
    return Err(AppError::BadRequest("User is already a team member.".to_owned()));
  }

  let member = team::add_member(&state.db, state.new_id(), user_id, found.team_id, team::ROLE_MEMBER).await?;

  Ok(Json(Value::Object(member.to_json())))
}

/// `GET /api/teams/{teamId}`: the team and its memberships.
pub async fn get(State(state): State<AppState>, auth: Auth, Path(team_id): Path<String>) -> AppResult<Json<Value>> {
  can::require(can::can_view_team(&state.db, &auth, &team_id).await?)?;

  match team::get_with_members(&state.db, db_id(&team_id)?).await? {
    Some(found) => Ok(Json(found)),
    None => Err(AppError::coded(StatusCode::NOT_FOUND, "not-found", "Team not found.")),
  }
}

/// `POST /api/teams/{teamId}`: renames the team or changes its access code.
pub async fn update(
  State(state): State<AppState>,
  Path(team_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let name = body::optional_string(object, "name")?
    .map(|s| body::max_len(s, 50))
    .transpose()?;
  let access_code = body::optional_string(object, "accessCode")?
    .map(|s| body::max_len(s, 50))
    .transpose()?;

  let auth = auth?;
  if !can::can_manage_team(&state.db, &auth, &team_id, TEAM_UPDATE).await? {
    return Err(AppError::unauthorized_message(NOT_OWNER_OR_MANAGER));
  }

  let updated = team::update(&state.db, db_id(&team_id)?, name, access_code)
    .await
    .map_err(|e| AppError::taken(e, "That access code is already taken."))?
    .ok_or(AppError::NotFound)?;

  Ok(Json(Value::Object(updated.to_json())))
}

/// `DELETE /api/teams/{teamId}`
pub async fn delete(State(state): State<AppState>, auth: Auth, Path(team_id): Path<String>) -> AppResult<Json<Value>> {
  if !can::can_manage_team(&state.db, &auth, &team_id, TEAM_DELETE).await? {
    return Err(AppError::unauthorized_message(NOT_OWNER_OR_MANAGER));
  }

  if !team::delete(&state.db, db_id(&team_id)?).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
}

/// `GET /api/teams/{teamId}/users`: the team's members.
pub async fn members(
  State(state): State<AppState>,
  Path(team_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let filters = Params::parse(query.as_deref()).filters(PAGING_SEARCH)?;
  let auth = auth?;
  if !can::can_view_team(&state.db, &auth, &team_id).await? {
    return Err(AppError::unauthorized_message("You must be a member of this team."));
  }

  Ok(Json(team::members_page(&state.db, db_id(&team_id)?, filters).await?))
}

/// `POST /api/teams/{teamId}/users`: adds a user to the team.
pub async fn add_member(
  State(state): State<AppState>,
  Path(team_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let user_id = body::uuid(body::string(object, "userId")?)?;
  let role = body::one_of(body::string(object, "role")?, TEAM_ROLES)?;

  let auth = auth?;
  if !can::can_manage_team(&state.db, &auth, &team_id, TEAM_UPDATE).await? {
    return Err(AppError::unauthorized_message(NOT_OWNER_OR_MANAGER));
  }

  // Upstream adds users that do not exist, which then list with `user: null`.
  if user::get(&state.db, user_id).await?.is_none() {
    return Err(AppError::user_not_found());
  }
  let team_id = db_id(&team_id)?;
  if team::team_user(&state.db, team_id, user_id).await?.is_some() {
    return Err(AppError::BadRequest("User is already a member of the Team.".to_owned()));
  }

  let member = team::add_member(&state.db, state.new_id(), user_id, team_id, role).await?;

  Ok(Json(Value::Object(member.to_json())))
}

/// `GET /api/teams/{teamId}/users/{userId}`: the membership, or `null`.
pub async fn member(
  State(state): State<AppState>,
  auth: Auth,
  Path((team_id, user_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  if !can::can_manage_team(&state.db, &auth, &team_id, TEAM_UPDATE).await? {
    return Err(AppError::unauthorized_message(NOT_OWNER_OR_MANAGER));
  }

  let member = team::team_user(&state.db, db_id(&team_id)?, db_id(&user_id)?).await?;

  Ok(Json(member.map_or(Value::Null, |m| Value::Object(m.to_json()))))
}

/// Upstream's rank check: non-admins may only change members ranked below them.
async fn outranks(state: &AppState, auth: &Auth, team_id: uuid::Uuid, target: &team::TeamUser) -> AppResult<bool> {
  let user = auth.user()?;
  if user.is_admin {
    return Ok(true);
  }

  let actor = team::team_user(&state.db, team_id, user.id).await?;

  Ok(team_role_rank(actor.as_ref().map(|a| a.role.as_str())) > team_role_rank(Some(&target.role)))
}

/// `POST /api/teams/{teamId}/users/{userId}`: changes a member's role.
pub async fn update_member(
  State(state): State<AppState>,
  Path((team_id, user_id)): Path<(String, String)>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let role = body::one_of(body::string(body::object(&body)?, "role")?, TEAM_ROLES)?;

  let auth = auth?;
  if !can::can_manage_team(&state.db, &auth, &team_id, TEAM_UPDATE).await? {
    return Err(AppError::unauthorized_message(NOT_OWNER_OR_MANAGER));
  }

  let team_id = db_id(&team_id)?;
  let Some(target) = team::team_user(&state.db, team_id, db_id(&user_id)?).await? else {
    return Err(AppError::BadRequest(NOT_MEMBER.to_owned()));
  };
  if !outranks(&state, &auth, team_id, &target).await? {
    return Err(AppError::unauthorized_message(
      "You do not have permission to modify this user.",
    ));
  }

  let member = team::set_member_role(&state.db, target.team_user_id, role).await?;

  Ok(Json(Value::Object(member.to_json())))
}

/// `DELETE /api/teams/{teamId}/users/{userId}`: removes a member (or leaves the team).
pub async fn remove_member(
  State(state): State<AppState>,
  auth: Auth,
  Path((team_id, user_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  if !can::can_delete_team_user(&state.db, &auth, &team_id, &user_id).await? {
    return Err(AppError::unauthorized_message(NOT_OWNER_OR_MANAGER));
  }

  let team_id = db_id(&team_id)?;
  let Some(target) = team::team_user(&state.db, team_id, db_id(&user_id)?).await? else {
    return Err(AppError::BadRequest(NOT_MEMBER.to_owned()));
  };

  let user = auth.user()?;
  let cannot_remove = "You do not have permission to remove this user.";
  if !user.is_admin && target.role == team::ROLE_OWNER {
    return Err(AppError::unauthorized_message(cannot_remove));
  }
  if user_id != user.id.to_string() && !outranks(&state, &auth, team_id, &target).await? {
    return Err(AppError::unauthorized_message(cannot_remove));
  }

  team::remove_member(&state.db, team_id, target.user_id).await?;

  Ok(ok())
}

/// `GET /api/teams/{teamId}/websites`
pub async fn websites(
  State(state): State<AppState>,
  Path(team_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let filters = Params::parse(query.as_deref()).filters(PAGING_SEARCH_SORTING)?;
  let auth = auth?;
  can::require(can::can_view_team(&state.db, &auth, &team_id).await?)?;

  let scope = website::Scope::Team(db_id(&team_id)?);

  Ok(Json(website::list(&state.db, scope, filters).await?))
}
