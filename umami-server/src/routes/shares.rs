//! Share links: `/api/share`, `/api/share/{slug}`, `/api/share/id/{shareId}` and
//! `/api/websites/{websiteId}/shares`, ported from upstream's `src/app/api/share`
//! and `src/app/api/websites/[websiteId]/shares`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use chrono::Utc;
use serde_json::{
  Map,
  Value,
  json,
};

use super::params::{
  PAGING,
  Params,
};
use super::{
  body,
  ok,
};
use crate::analytics::filters::check_filter_params;
use crate::auth::{
  Auth,
  AuthKind,
  AuthUser,
};
use crate::error::{
  AppError,
  AppResult,
};
use crate::permissions::{
  self as can,
  EntityType,
  Owner,
  WEBSITE_DELETE,
  WEBSITE_UPDATE,
  db_id,
};
use crate::queries::board::{
  self,
  EntityIds,
};
use crate::queries::share::{
  self,
  NewShare,
  SLUG_LENGTH,
  SLUG_TAKEN,
};
use crate::queries::{
  team,
  user,
  website,
};
use crate::state::AppState;
use crate::{
  crypto,
  jwt,
  zod,
};

/// Upstream's `SHARE_TOKEN_TYPE`.
const SHARE_TOKEN_TYPE: &str = "share";

/// A share name: `z.string().max(200)`.
fn name(object: &Map<String, Value>) -> AppResult<&str> {
  body::max_len(body::string(object, "name")?, 200)
}

/// The required `parameters` (`anyObjectParam`), as stored.
fn parameters(object: &Map<String, Value>) -> AppResult<Value> {
  Ok(Value::Object(body::object_field(object, "parameters")?.clone()))
}

/// `GET /api/websites/{websiteId}/shares`: the website's share links, newest first.
pub async fn website_list(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  check_filter_params(&params)?;
  let filters = params.filters(PAGING)?;

  let auth = auth?;
  can::require(can::can_view_authenticated_website(&state.db, &auth, &website_id).await?)?;

  Ok(Json(share::list(&state.db, db_id(&website_id)?, &filters).await?))
}

/// `POST /api/websites/{websiteId}/shares`: a new share link for the website,
/// with a random slug.
pub async fn website_create(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let name = name(object)?;
  let parameters = body::optional_object(object, "parameters")?
    .cloned()
    .unwrap_or_default();

  let auth = auth?;
  can::require(can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await?)?;
  // Admins pass for any ID; upstream then shares a website that does not exist.
  let Some(found) = website::get(&state.db, &website_id).await? else {
    return Err(AppError::NotFound);
  };

  let created = share::create(
    &state.db,
    &NewShare {
      id: state.new_id(),
      entity_id: found.website_id,
      kind: EntityType::Website,
      name,
      slug: &crypto::random_chars(SLUG_LENGTH),
      parameters: Value::Object(parameters),
    },
  )
  .await
  .map_err(|e| AppError::taken(e, SLUG_TAKEN))?;

  Ok(Json(created.to_json()))
}

/// `POST /api/share`: a new share link for a website, link, pixel or board.
pub async fn create(State(state): State<AppState>, auth: AppResult<Auth>, body: Bytes) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let entity_id = body::uuid(body::string(object, "entityId")?)?;
  let share_type = zod::coerce_int(object.get("shareType")).ok_or_else(AppError::bad_request)?;
  let name = name(object)?;
  let slug = body::optional_string(object, "slug")?
    .map(|slug| body::max_len(slug, 100))
    .transpose()?;
  let parameters = parameters(object)?;

  let auth = auth?;
  let id = entity_id.to_string();
  can::require(can::can_access_entity(&state.db, &auth, &id, Some(WEBSITE_UPDATE)).await?)?;

  // Upstream shares any ID as any type; a share of a missing entity, or one
  // whose type is not the entity's, can never be opened.
  let Some(entity) = can::entity(&state.db, &id).await? else {
    return Err(AppError::NotFound);
  };
  if EntityType::from_code(share_type) != Some(entity.kind) {
    return Err(AppError::BadRequest(
      "The share type does not match the entity.".to_owned(),
    ));
  }

  let random;
  let slug = match slug.filter(|slug| !slug.is_empty()) {
    Some(slug) => slug,
    None => {
      random = crypto::random_chars(SLUG_LENGTH);
      &random
    }
  };

  let created = share::create(
    &state.db,
    &NewShare {
      id: state.new_id(),
      entity_id,
      kind: entity.kind,
      name,
      slug,
      parameters,
    },
  )
  .await
  .map_err(|e| AppError::taken(e, SLUG_TAKEN))?;

  Ok(Json(created.to_json()))
}

/// The share with ID `share_id`; a 404 if there is none (upstream fails with
/// a 500).
async fn find(state: &AppState, share_id: &str) -> AppResult<share::Share> {
  share::get(&state.db, db_id(share_id)?).await?.ok_or(AppError::NotFound)
}

/// `GET /api/share/id/{shareId}`: the share link.
pub async fn get(State(state): State<AppState>, auth: Auth, Path(share_id): Path<String>) -> AppResult<Json<Value>> {
  let found = find(&state, &share_id).await?;
  let entity_id = found.entity_id.to_string();
  can::require(can::can_access_entity(&state.db, &auth, &entity_id, None).await?)?;

  Ok(Json(found.to_json()))
}

/// `POST /api/share/id/{shareId}`: renames the share link or changes its slug
/// or parameters.
pub async fn update(
  State(state): State<AppState>,
  Path(share_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let name = name(object)?;
  let slug = body::max_len(body::string(object, "slug")?, 100)?;
  let parameters = parameters(object)?;

  let auth = auth?;
  let found = find(&state, &share_id).await?;
  let entity_id = found.entity_id.to_string();
  can::require(can::can_access_entity(&state.db, &auth, &entity_id, Some(WEBSITE_UPDATE)).await?)?;

  let updated = share::update(&state.db, found.share_id, name, slug, &parameters)
    .await
    .map_err(|e| AppError::taken(e, SLUG_TAKEN))?
    .ok_or(AppError::NotFound)?;

  Ok(Json(updated.to_json()))
}

/// `DELETE /api/share/id/{shareId}`: deletes the share link.
pub async fn delete(State(state): State<AppState>, auth: Auth, Path(share_id): Path<String>) -> AppResult<Json<Value>> {
  let found = find(&state, &share_id).await?;
  let entity_id = found.entity_id.to_string();
  can::require(can::can_access_entity(&state.db, &auth, &entity_id, Some(WEBSITE_DELETE)).await?)?;

  if !share::delete(&state.db, found.share_id).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
}

/// `GET /api/share/{slug}`: opens a share link, without signing in. Returns what
/// it shares and the share token that grants access to it.
///
/// Upstream also attaches the owner's white-label settings, which it keeps only
/// in Redis; umami-rs has no Redis, as upstream without it.
pub async fn resolve(State(state): State<AppState>, Path(slug): Path<String>) -> AppResult<Json<Value>> {
  let found = share::by_slug(&state.db, &slug).await?.ok_or(AppError::NotFound)?;
  let kind = EntityType::from_code(found.share_type).ok_or(AppError::NotFound)?;
  let entity_id = json!(found.entity_id);

  let mut data = Map::new();
  data.insert("shareId".to_owned(), json!(found.share_id));
  data.insert("shareType".to_owned(), json!(found.share_type));
  data.insert("parameters".to_owned(), found.parameters.clone());

  if kind == EntityType::Board {
    let board = board::get(&state.db, found.entity_id)
      .await?
      .ok_or(AppError::NotFound)?;
    let ids = shared_board_entities(&state, board.owner(), board.entity_ids()).await?;

    data.insert("boardId".to_owned(), entity_id);
    data.insert("websiteIds".to_owned(), json!(ids.websites));
    data.insert("pixelIds".to_owned(), json!(ids.pixels));
    data.insert("linkIds".to_owned(), json!(ids.links));
  } else {
    if can::owner(&state.db, kind, found.entity_id).await?.is_none() {
      return Err(AppError::NotFound);
    }
    // Upstream also names a link or pixel as `websiteId`, which the
    // dashboard's routes take it as.
    data.insert("websiteId".to_owned(), entity_id.clone());
    match kind {
      EntityType::Pixel => data.insert("pixelId".to_owned(), entity_id),
      EntityType::Link => data.insert("linkId".to_owned(), entity_id),
      _ => None,
    };
  }

  let mut claims = data.clone();
  claims.insert("type".to_owned(), json!(SHARE_TOKEN_TYPE));
  claims.insert("iat".to_owned(), json!(Utc::now().timestamp()));
  data.insert("token".to_owned(), json!(jwt::sign(&claims, &state.config.secret)));

  Ok(Json(Value::Object(data)))
}

/// Upstream's `filterBoardEntityIdsForShare`: the board's entities that its
/// owner may share. A team board shares what the team or its members own; a
/// user's board what that user can view.
async fn shared_board_entities(state: &AppState, board: Owner, ids: EntityIds) -> AppResult<EntityIds> {
  let mut shared = EntityIds::default();
  let lists = [
    (EntityType::Website, ids.websites, &mut shared.websites),
    (EntityType::Pixel, ids.pixels, &mut shared.pixels),
    (EntityType::Link, ids.links, &mut shared.links),
  ];

  if let Some(team_id) = board.team_id {
    let members = team::member_ids(&state.db, team_id).await?;

    for (kind, ids, shared) in lists {
      for id in ids {
        // Upstream's lookups fail (and drop the ID) for anything but a UUID.
        let Some(uuid) = zod::uuid(&id) else { continue };
        let owned_by_team = can::owner(&state.db, kind, uuid).await?.is_some_and(|owner| {
          owner.team_id == Some(team_id) || owner.user_id.is_some_and(|user| members.contains(&user))
        });
        if owned_by_team {
          shared.push(id);
        }
      }
    }
    return Ok(shared);
  }

  let Some(user_id) = board.user_id else {
    return Ok(shared);
  };
  let Some(user) = user::get(&state.db, user_id).await? else {
    return Ok(shared);
  };
  let auth = Auth {
    token: None,
    user: Some(AuthUser::new(user)),
    share_token: None,
    kind: AuthKind::Session,
  };

  for (kind, ids, shared) in lists {
    for id in ids {
      let allowed = match kind {
        EntityType::Website => can::can_view_website(&state.db, &auth, &id).await,
        EntityType::Pixel => can::can_view_pixel(&state.db, &auth, &id).await,
        _ => can::can_view_link(&state.db, &auth, &id).await,
      };
      // Upstream drops IDs whose check fails.
      if allowed.unwrap_or(false) {
        shared.push(id);
      }
    }
  }

  Ok(shared)
}
