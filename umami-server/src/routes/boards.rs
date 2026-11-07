//! `/api/boards` with `/{boardId}`, its `clone` and `shares`,
//! `/api/teams/{teamId}/boards` and the user's own dashboard (`/api/dashboard`),
//! ported from upstream's `src/app/api/boards` and `src/app/api/dashboard`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use serde_json::{
  Map,
  Value,
};

use super::params::{
  PAGING_SEARCH_SORTING,
  Params,
};
use super::{
  body,
  ok,
  shares,
};
use crate::auth::Auth;
use crate::error::{
  AppError,
  AppResult,
};
use crate::permissions::{
  self as can,
  EntityType,
  WEBSITE_DELETE,
  WEBSITE_UPDATE,
  db_id,
};
use crate::queries::board::{
  self,
  Board,
  BoardUpdate,
  DASHBOARD,
  NewBoard,
  Scope,
};
use crate::queries::{
  share,
  team,
};
use crate::state::AppState;
use crate::zod;

/// Upstream's 400 for a board showing what the user cannot view.
const INACCESSIBLE: &str = "Board contains inaccessible entities.";
/// Upstream's 400 for funnel or goal components naming the wrong reports.
const INVALID_REPORTS: &str = "Board contains invalid saved reports.";

fn to_json(board: &Board) -> Json<Value> {
  Json(Value::Object(board.to_json()))
}

/// An optional string of at most `max` characters.
fn optional<'a>(object: &'a Map<String, Value>, key: &str, max: usize) -> AppResult<Option<&'a str>> {
  body::optional_string(object, key)?
    .map(|value| body::max_len(value, max))
    .transpose()
}

/// The optional `parameters` object (`z.object({}).passthrough()`).
fn parameters(object: &Map<String, Value>) -> AppResult<Option<Value>> {
  Ok(body::optional_object(object, "parameters")?.cloned().map(Value::Object))
}

/// The checks upstream runs on a board's new type and parameters: the layout
/// must have upstream's shape (upstream fails with a 500 otherwise), the
/// signed-in user must be able to view every entity on the board, and funnel
/// and goal components must name reports of their website.
async fn check_contents(state: &AppState, auth: &Auth, kind: &str, parameters: &Value) -> AppResult<()> {
  if !board::valid_layout(parameters) {
    return Err(AppError::bad_request());
  }
  if !can::can_view_board_entities(&state.db, auth, kind, parameters).await? {
    return Err(AppError::BadRequest(INACCESSIBLE.to_owned()));
  }
  check_reports(state, kind, parameters).await
}

async fn check_reports(state: &AppState, kind: &str, parameters: &Value) -> AppResult<()> {
  if !board::valid_reports(&state.db, kind, parameters).await? {
    return Err(AppError::BadRequest(INVALID_REPORTS.to_owned()));
  }
  Ok(())
}

/// `GET /api/boards`: the caller's boards.
pub async fn list(
  State(state): State<AppState>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let filters = Params::parse(query.as_deref()).filters(PAGING_SEARCH_SORTING)?;
  let user_id = auth?.user()?.id;

  Ok(Json(board::list(&state.db, Scope::User(user_id), filters).await?))
}

/// `GET /api/teams/{teamId}/boards`: the team's boards.
pub async fn team_list(
  State(state): State<AppState>,
  Path(team_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let filters = Params::parse(query.as_deref()).filters(PAGING_SEARCH_SORTING)?;
  let auth = auth?;
  can::require(can::can_view_team(&state.db, &auth, &team_id).await?)?;

  let scope = Scope::Team(db_id(&team_id)?);

  Ok(Json(board::list(&state.db, scope, filters).await?))
}

/// `POST /api/boards`: creates a board for the caller or for a team.
pub async fn create(State(state): State<AppState>, auth: AppResult<Auth>, body: Bytes) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let kind = body::one_of(
    body::string(object, "type")?,
    &["mixed", "website", "pixel", "link", "open"],
  )?;
  let name = body::max_len(body::string(object, "name")?, 100)?;
  let description = optional(object, "description", 500)?;
  // Accepted, but the board belongs to the caller or the team.
  body::nullable_uuid(object, "userId")?;
  let team_id = body::nullable_uuid(object, "teamId")?;
  let parameters = parameters(object)?.unwrap_or_else(|| Value::Object(Map::new()));
  for key in ["websiteId", "pixelId", "linkId"] {
    if let Some(id) = parameters.get(key)
      && id.as_str().and_then(zod::uuid).is_none()
    {
      return Err(AppError::bad_request());
    }
  }

  let auth = auth?;
  let team_allowed = match team_id {
    Some(team_id) => can::can_create_team_website(&state.db, &auth, &team_id.to_string()).await?,
    None => true,
  };
  if !team_allowed || !can::can_create_website(&auth) {
    return Err(AppError::Unauthorized);
  }
  let user_id = auth.user()?.id;
  // Only admins can name a team they are not in; upstream accepts any ID.
  if let Some(team_id) = team_id
    && !team::exists(&state.db, team_id).await?
  {
    return Err(AppError::team_not_found());
  }
  check_contents(&state, &auth, kind, &parameters).await?;

  let created = board::create(
    &state.db,
    &NewBoard {
      id: state.new_id(),
      kind: board::normalize_type(kind).unwrap_or(kind),
      name,
      description: description.unwrap_or_default(),
      parameters: &parameters,
      user_id: team_id.is_none().then_some(user_id),
      team_id,
    },
  )
  .await?;

  Ok(to_json(&created))
}

/// `GET /api/boards/{boardId}`: the board, or `null` if there is none.
pub async fn get(State(state): State<AppState>, auth: Auth, Path(board_id): Path<String>) -> AppResult<Json<Value>> {
  can::require(can::can_view_board(&state.db, &auth, &board_id).await?)?;

  match board::get(&state.db, db_id(&board_id)?).await? {
    Some(found) => Ok(to_json(&found)),
    None => Ok(Json(Value::Null)),
  }
}

/// `POST /api/boards/{boardId}`: changes the board's type, name, description
/// or layout.
pub async fn update(
  State(state): State<AppState>,
  Path(board_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let kind = body::optional_string(object, "type")?
    .map(|kind| body::one_of(kind, &[DASHBOARD, "mixed", "website", "pixel", "link", "open"]))
    .transpose()?
    .and_then(board::normalize_type);
  let name = optional(object, "name", 200)?;
  let description = optional(object, "description", 500)?;
  let parameters = parameters(object)?;

  let auth = auth?;
  can::require(can::can_change_board(&state.db, &auth, &board_id, WEBSITE_UPDATE).await?)?;
  let id = db_id(&board_id)?;

  if kind.is_some() || parameters.is_some() {
    let Some(current) = board::get(&state.db, id).await? else {
      return Err(AppError::Unauthorized);
    };
    let next_kind = kind.unwrap_or(&current.kind);
    let next_parameters = parameters.as_ref().unwrap_or(&current.parameters);
    check_contents(&state, &auth, next_kind, next_parameters).await?;
  }

  let changes = BoardUpdate {
    kind,
    name,
    description,
    parameters: parameters.as_ref(),
  };
  // Admins pass for any ID, where upstream then fails with a 500.
  let updated = board::update(&state.db, id, &changes)
    .await?
    .ok_or(AppError::NotFound)?;

  Ok(to_json(&updated))
}

/// `DELETE /api/boards/{boardId}`: deletes the board and its share links.
pub async fn delete(State(state): State<AppState>, auth: Auth, Path(board_id): Path<String>) -> AppResult<Json<Value>> {
  can::require(can::can_change_board(&state.db, &auth, &board_id, WEBSITE_DELETE).await?)?;

  if !board::delete(&state.db, db_id(&board_id)?).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
}

/// `POST /api/boards/{boardId}/clone`: a copy of the board, for the same owner,
/// without its invalid report references.
pub async fn clone(
  State(state): State<AppState>,
  Path(board_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let name = optional(object, "name", 200)?;
  let description = optional(object, "description", 500)?;
  let parameters = parameters(object)?;

  let auth = auth?;
  can::require(can::can_change_board(&state.db, &auth, &board_id, WEBSITE_UPDATE).await?)?;
  let Some(source) = board::get(&state.db, db_id(&board_id)?).await? else {
    return Err(AppError::Unauthorized);
  };
  let Some(kind) = board::normalize_type(&source.kind).filter(|kind| *kind != DASHBOARD) else {
    return Err(AppError::BadRequest("Board cannot be cloned.".to_owned()));
  };

  let team_allowed = match source.team_id {
    Some(team_id) => can::can_create_team_website(&state.db, &auth, &team_id.to_string()).await?,
    None => true,
  };
  if !team_allowed || !can::can_create_website(&auth) {
    return Err(AppError::Unauthorized);
  }
  let user_id = auth.user()?.id;

  let parameters = match parameters {
    Some(parameters) => parameters,
    None if source.parameters.is_null() => Value::Object(Map::new()),
    None => source.parameters.clone(),
  };
  if !board::valid_layout(&parameters) {
    return Err(AppError::bad_request());
  }
  if !can::can_view_board_entities(&state.db, &auth, kind, &parameters).await? {
    return Err(AppError::BadRequest(INACCESSIBLE.to_owned()));
  }
  let parameters = board::strip_invalid_reports(&state.db, kind, &parameters).await?;
  check_reports(&state, kind, &parameters).await?;

  let created = board::create(
    &state.db,
    &NewBoard {
      id: state.new_id(),
      kind,
      name: name.unwrap_or(&source.name),
      description: description.unwrap_or(&source.description),
      parameters: &parameters,
      user_id: match source.team_id {
        Some(_) => None,
        None => Some(source.user_id.unwrap_or(user_id)),
      },
      team_id: source.team_id,
    },
  )
  .await?;

  Ok(to_json(&created))
}

/// `GET /api/boards/{boardId}/shares`: the board's share links, newest first.
pub async fn share_list(
  State(state): State<AppState>,
  Path(board_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let filters = shares::list_filters(query.as_deref())?;

  let auth = auth?;
  can::require(can::can_view_board(&state.db, &auth, &board_id).await?)?;

  Ok(Json(share::list(&state.db, db_id(&board_id)?, &filters).await?))
}

/// `POST /api/boards/{boardId}/shares`: a new share link for the board, with a
/// random slug.
pub async fn share_create(
  State(state): State<AppState>,
  Path(board_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let (name, parameters) = shares::new_share(body::object(&body)?)?;

  let auth = auth?;
  can::require(can::can_change_board(&state.db, &auth, &board_id, WEBSITE_UPDATE).await?)?;
  // Admins pass for any ID; upstream then shares a board that does not exist.
  let Some(found) = board::get(&state.db, db_id(&board_id)?).await? else {
    return Err(AppError::NotFound);
  };

  shares::create_random(&state, found.board_id, EntityType::Board, name, parameters).await
}

/// The caller's dashboard (the board whose ID is theirs), if any; a 401 if
/// that board is someone else's.
async fn own_dashboard(state: &AppState, auth: &Auth) -> AppResult<Option<Board>> {
  let user_id = auth.user()?.id;
  let found = board::get(&state.db, user_id).await?;

  if found.as_ref().is_some_and(|board| board.user_id != Some(user_id)) {
    return Err(AppError::Unauthorized);
  }
  Ok(found)
}

/// `GET /api/dashboard`: the caller's dashboard, or `null` if they have none.
pub async fn dashboard(State(state): State<AppState>, auth: Auth) -> AppResult<Json<Value>> {
  match own_dashboard(&state, &auth).await? {
    Some(found) => Ok(to_json(&found)),
    None => Ok(Json(Value::Null)),
  }
}

/// `POST /api/dashboard`: saves the caller's dashboard, creating it if needed.
pub async fn save_dashboard(
  State(state): State<AppState>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let name = optional(object, "name", 200)?;
  let description = optional(object, "description", 500)?;
  let parameters = parameters(object)?.unwrap_or_else(|| Value::Object(Map::new()));

  let auth = auth?;
  let existing = own_dashboard(&state, &auth).await?;
  let user_id = auth.user()?.id;

  if !board::valid_layout(&parameters) {
    return Err(AppError::bad_request());
  }
  let kind = existing.as_ref().map_or(DASHBOARD, |board| board.kind.as_str());
  check_reports(&state, kind, &parameters).await?;

  let saved = match existing {
    Some(_) => {
      let changes = BoardUpdate {
        kind: None,
        name,
        description,
        parameters: Some(&parameters),
      };
      board::update(&state.db, user_id, &changes)
        .await?
        .ok_or(AppError::NotFound)?
    }
    // Upstream fails with a 500 without a name and description.
    None => {
      board::create(
        &state.db,
        &NewBoard {
          id: user_id,
          kind: DASHBOARD,
          name: name.unwrap_or_default(),
          description: description.unwrap_or_default(),
          parameters: &parameters,
          user_id: Some(user_id),
          team_id: None,
        },
      )
      .await?
    }
  };

  Ok(to_json(&saved))
}
