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
use uuid::Uuid;

use super::body;
use super::params::{
  PAGING,
  Params,
};
use crate::auth::{
  Auth,
  AuthKind,
  AuthUser,
  store,
};
use crate::collect::js;
use crate::error::{
  AppError,
  AppResult,
};
use crate::permissions::{
  self as can,
  ENTITY_BOARD,
  ENTITY_LINK,
  ENTITY_PIXEL,
  ENTITY_WEBSITE,
  WEBSITE_DELETE,
  WEBSITE_UPDATE,
  db_id,
};
use crate::queries::share::{
  self,
  NewShare,
};
use crate::queries::{
  team,
  website,
};
use crate::state::AppState;
use crate::{
  crypto,
  jwt,
  zod,
};

const SLUG_TAKEN: &str = "That share ID is already taken.";
/// Upstream's `SHARE_TOKEN_TYPE`.
const SHARE_TOKEN_TYPE: &str = "share";
/// Length of the slugs upstream generates.
const SLUG_LENGTH: usize = 16;

/// `anyObjectParam`: a JSON object, whatever its values.
fn object_param<'a>(object: &'a Map<String, Value>, key: &str) -> AppResult<Option<&'a Value>> {
  match object.get(key) {
    None => Ok(None),
    Some(value @ Value::Object(_)) => Ok(Some(value)),
    Some(_) => Err(AppError::bad_request()),
  }
}

/// A share name: `z.string().max(200)`.
fn name(object: &Map<String, Value>) -> AppResult<&str> {
  body::max_len(body::string(object, "name")?, 200)
}

/// `z.coerce.number().int()`, for the entity types upstream's `INTEGER` column
/// can hold.
fn coerce_int(value: Option<&Value>) -> AppResult<i32> {
  let number = match value {
    Some(Value::Number(n)) => n.as_f64(),
    Some(Value::String(s)) => Some(js::string_to_number(s)),
    Some(Value::Bool(b)) => Some(f64::from(u8::from(*b))),
    Some(Value::Null) => Some(0.0),
    // `Number(undefined)` and objects are `NaN`.
    _ => None,
  };

  number
    .filter(|n| n.fract() == 0.0 && *n >= f64::from(i32::MIN) && *n <= f64::from(i32::MAX))
    .map(|n| n as i32)
    .ok_or_else(AppError::bad_request)
}

/// Validates upstream's `filterParams`, which the share list's schema includes
/// but does not use.
fn check_filter_params(params: &Params) -> AppResult<()> {
  for key in ["segment", "cohort"] {
    if params.get(key).is_some_and(|value| zod::uuid(value).is_none()) {
      return Err(AppError::bad_request());
    }
  }
  if params
    .get("eventType")
    .is_some_and(|value| zod::positive_int(value).is_none())
  {
    return Err(AppError::bad_request());
  }
  if params.get("match").is_some_and(|value| !matches!(value, "all" | "any")) {
    return Err(AppError::bad_request());
  }

  Ok(())
}

fn new_id(state: &AppState) -> Uuid {
  crypto::random_uuid(state.config.collect.use_uuidv7)
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
  if !can::can_view_authenticated_website(&state.db, &auth, &website_id).await? {
    return Err(AppError::Unauthorized);
  }

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
  let parameters = object_param(object, "parameters")?.cloned().unwrap_or(json!({}));

  let auth = auth?;
  if !can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await? {
    return Err(AppError::Unauthorized);
  }
  // Admins pass for any ID; upstream then shares a website that does not exist.
  let Some(found) = website::get(&state.db, &website_id).await? else {
    return Err(AppError::NotFound);
  };

  let created = share::create(
    &state.db,
    &NewShare {
      id: new_id(&state),
      entity_id: found.website_id,
      share_type: ENTITY_WEBSITE,
      name,
      slug: &crypto::random_chars(SLUG_LENGTH),
      parameters: &parameters,
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
  let share_type = coerce_int(object.get("shareType"))?;
  let name = name(object)?;
  let slug = body::optional_string(object, "slug")?
    .map(|slug| body::max_len(slug, 100))
    .transpose()?;
  let parameters = object_param(object, "parameters")?.ok_or_else(AppError::bad_request)?;

  let auth = auth?;
  let id = entity_id.to_string();
  if !can::can_access_entity(&state.db, &auth, &id, Some(WEBSITE_UPDATE)).await? {
    return Err(AppError::Unauthorized);
  }

  // Upstream shares any ID as any type; a share of a missing entity, or one
  // whose type is not the entity's, can never be opened.
  let Some(entity) = can::entity(&state.db, &id).await? else {
    return Err(AppError::NotFound);
  };
  if entity.kind != share_type {
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
      id: new_id(&state),
      entity_id,
      share_type,
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
  if !can::can_access_entity(&state.db, &auth, &found.entity_id.to_string(), None).await? {
    return Err(AppError::Unauthorized);
  }

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
  let parameters = object_param(object, "parameters")?.ok_or_else(AppError::bad_request)?;

  let auth = auth?;
  let found = find(&state, &share_id).await?;
  let entity_id = found.entity_id.to_string();
  if !can::can_access_entity(&state.db, &auth, &entity_id, Some(WEBSITE_UPDATE)).await? {
    return Err(AppError::Unauthorized);
  }

  let updated = share::update(&state.db, found.share_id, name, slug, parameters)
    .await
    .map_err(|e| AppError::taken(e, SLUG_TAKEN))?
    .ok_or(AppError::NotFound)?;

  Ok(Json(updated.to_json()))
}

/// `DELETE /api/share/id/{shareId}`: deletes the share link.
pub async fn delete(State(state): State<AppState>, auth: Auth, Path(share_id): Path<String>) -> AppResult<Json<Value>> {
  let found = find(&state, &share_id).await?;
  let entity_id = found.entity_id.to_string();
  if !can::can_access_entity(&state.db, &auth, &entity_id, Some(WEBSITE_DELETE)).await? {
    return Err(AppError::Unauthorized);
  }

  if !share::delete(&state.db, found.share_id).await? {
    return Err(AppError::NotFound);
  }

  Ok(Json(json!({ "ok": true })))
}

/// `GET /api/share/{slug}`: opens a share link, without signing in. Returns what
/// it shares and the share token that grants access to it.
///
/// Upstream also attaches the owner's white-label settings, which it keeps only
/// in Redis; umami-rs has no Redis, as upstream without it.
pub async fn resolve(State(state): State<AppState>, Path(slug): Path<String>) -> AppResult<Json<Value>> {
  let found = share::by_slug(&state.db, &slug).await?.ok_or(AppError::NotFound)?;
  let entity_id = found.entity_id.to_string();

  let mut data = Map::new();
  data.insert("shareId".to_owned(), json!(found.share_id));
  data.insert("shareType".to_owned(), json!(found.share_type));
  data.insert("parameters".to_owned(), found.parameters.clone());

  let table = match found.share_type {
    ENTITY_WEBSITE => "website",
    ENTITY_LINK => "link",
    ENTITY_PIXEL => "pixel",
    ENTITY_BOARD => "board",
    _ => return Err(AppError::NotFound),
  };
  let Some(entity) = owned(&state, table, found.entity_id).await? else {
    return Err(AppError::NotFound);
  };

  match found.share_type {
    ENTITY_BOARD => {
      let (kind, parameters): (String, Value) =
        sqlx::query_as("select type, parameters from board where board_id = $1")
          .bind(found.entity_id)
          .fetch_one(&state.db)
          .await?;
      let ids = board_entity_ids(Some(&kind), &parameters);
      let [websites, pixels, links] = shared_board_entities(&state, &entity, ids).await?;

      data.insert("boardId".to_owned(), json!(entity_id));
      data.insert("websiteIds".to_owned(), json!(websites));
      data.insert("pixelIds".to_owned(), json!(pixels));
      data.insert("linkIds".to_owned(), json!(links));
    }
    ENTITY_WEBSITE => {
      data.insert("websiteId".to_owned(), json!(entity_id));
    }
    // Upstream also names a link or pixel as `websiteId`, which the
    // dashboard's routes take it as.
    ENTITY_PIXEL => {
      data.insert("websiteId".to_owned(), json!(entity_id));
      data.insert("pixelId".to_owned(), json!(entity_id));
    }
    _ => {
      data.insert("websiteId".to_owned(), json!(entity_id));
      data.insert("linkId".to_owned(), json!(entity_id));
    }
  }

  let mut claims = data.clone();
  claims.insert("type".to_owned(), json!(SHARE_TOKEN_TYPE));
  claims.insert("iat".to_owned(), json!(Utc::now().timestamp()));
  data.insert("token".to_owned(), json!(jwt::sign(&claims, &state.config.secret)));

  Ok(Json(Value::Object(data)))
}

/// The owner of a website, link, pixel or board (by `table`), deleted or not.
async fn owned(state: &AppState, table: &str, id: Uuid) -> sqlx::Result<Option<Owner>> {
  let owner: Option<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select user_id, team_id from {table} where {table}_id = $1"
  )))
  .bind(id)
  .fetch_optional(&state.db)
  .await?;

  Ok(owner.map(|(user_id, team_id)| Owner { user_id, team_id }))
}

struct Owner {
  user_id: Option<Uuid>,
  team_id: Option<Uuid>,
}

/// The websites, pixels and links a board shows (upstream's `getBoardEntityIds`),
/// each list in first-seen order without duplicates.
fn board_entity_ids(kind: Option<&str>, parameters: &Value) -> [Vec<String>; 3] {
  let mut ids: [Vec<String>; 3] = Default::default();
  let mut add = |slot: usize, id: Option<&Value>| {
    // Upstream checks truthiness, then collects into a `Set`.
    if let Some(id) = id.and_then(Value::as_str).filter(|id| !id.is_empty())
      && !ids[slot].iter().any(|seen| seen == id)
    {
      ids[slot].push(id.to_owned());
    }
  };
  let slot = |entity_type: &str| {
    match entity_type {
      "website" => Some(0),
      "pixel" => Some(1),
      "link" => Some(2),
      _ => None,
    }
  };

  // `getBoardType`: a known type, `open` as `mixed`, else inferred from the
  // parameters (`getLegacyBoardType`).
  let legacy = if truthy(&parameters["pixelId"]) {
    "pixel"
  } else if truthy(&parameters["linkId"]) {
    "link"
  } else if truthy(&parameters["websiteId"]) {
    "website"
  } else {
    "mixed"
  };
  let board_type = match kind {
    Some("open") => "mixed",
    Some(known @ ("dashboard" | "mixed" | "website" | "pixel" | "link")) => known,
    _ => legacy,
  };
  if let Some(slot) = slot(board_type) {
    add(slot, parameters.get(format!("{board_type}Id")));
  }

  add(0, parameters.get("websiteId"));
  add(1, parameters.get("pixelId"));
  add(2, parameters.get("linkId"));

  let rows = parameters["rows"].as_array().into_iter().flatten();
  for column in rows.flat_map(|row| row["columns"].as_array().into_iter().flatten()) {
    let component = &column["component"];
    // `getComponentEntity`
    let (entity_type, entity_id) = if truthy(&component["entityType"]) && truthy(&component["entityId"]) {
      (component["entityType"].as_str(), component.get("entityId"))
    } else if truthy(&component["websiteId"]) {
      (Some("website"), component.get("websiteId"))
    } else {
      (None, None)
    };
    if let Some(slot) = entity_type.and_then(slot) {
      add(slot, entity_id);
    }
  }

  ids
}

/// JavaScript truthiness.
fn truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(b) => *b,
    Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
    Value::String(s) => !s.is_empty(),
    _ => true,
  }
}

/// Upstream's `filterBoardEntityIdsForShare`: the board's entities that its
/// owner may share. A team board shares what the team or its members own; a
/// user's board what that user can view.
async fn shared_board_entities(state: &AppState, board: &Owner, ids: [Vec<String>; 3]) -> AppResult<[Vec<String>; 3]> {
  let tables = ["website", "pixel", "link"];
  let mut shared: [Vec<String>; 3] = Default::default();

  if let Some(team_id) = board.team_id {
    let members: Vec<Uuid> = sqlx::query_scalar("select user_id from team_user where team_id = $1")
      .bind(team_id)
      .fetch_all(&state.db)
      .await?;

    for (slot, list) in ids.into_iter().enumerate() {
      for id in list {
        let Some(uuid) = zod::uuid(&id) else { continue };
        let owned_by_team = owned(state, tables[slot], uuid)
          .await?
          .is_some_and(|e| e.team_id == Some(team_id) || e.user_id.is_some_and(|u| members.contains(&u)));
        if owned_by_team {
          shared[slot].push(id);
        }
      }
    }
    return Ok(shared);
  }

  let Some(user_id) = board.user_id else {
    return Ok(shared);
  };
  let Some(user) = store::user(&state.db, user_id).await? else {
    return Ok(shared);
  };
  let auth = Auth {
    token: None,
    user: Some(AuthUser::new(user)),
    share_token: None,
    kind: AuthKind::Session,
  };

  for (slot, list) in ids.into_iter().enumerate() {
    for id in list {
      let allowed = if slot == 0 {
        can::can_view_website(&state.db, &auth, &id).await
      } else {
        can_view_owned(state, &auth, tables[slot], &id).await
      };
      // Upstream drops IDs whose check fails.
      if allowed.unwrap_or(false) {
        shared[slot].push(id);
      }
    }
  }

  Ok(shared)
}

/// Upstream's `canViewLink` and `canViewPixel` for a signed-in user.
async fn can_view_owned(state: &AppState, auth: &Auth, table: &str, id: &str) -> AppResult<bool> {
  let Some(user) = &auth.user else {
    return Ok(false);
  };
  if user.is_admin {
    return Ok(true);
  }
  let Some(entity) = owned(state, table, db_id(id)?).await? else {
    return Ok(false);
  };

  if let Some(owner) = entity.user_id {
    return Ok(owner == user.id);
  }
  if let Some(team_id) = entity.team_id {
    return Ok(team::team_user(&state.db, team_id, user.id).await?.is_some());
  }

  Ok(false)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn coerces_share_types_like_zod() {
    assert_eq!(coerce_int(Some(&json!(1))).unwrap(), 1);
    assert_eq!(coerce_int(Some(&json!("4"))).unwrap(), 4);
    assert_eq!(coerce_int(Some(&json!(" 2 "))).unwrap(), 2);
    assert_eq!(coerce_int(Some(&json!(true))).unwrap(), 1);
    assert_eq!(coerce_int(Some(&json!(null))).unwrap(), 0);
    assert_eq!(coerce_int(Some(&json!(""))).unwrap(), 0);

    for invalid in [json!(1.5), json!("x"), json!({}), json!(1e10)] {
      assert!(coerce_int(Some(&invalid)).is_err(), "{invalid}");
    }
    assert!(coerce_int(None).is_err());
  }

  #[test]
  fn collects_board_entities_like_upstream() {
    let parameters = json!({
        "websiteId": "w1",
        "rows": [
            { "columns": [
                { "component": { "entityType": "pixel", "entityId": "p1" } },
                { "component": { "websiteId": "w2" } },
                { "component": { "entityType": "link", "entityId": "" , "websiteId": "w1" } },
                { "component": { "entityType": "other", "entityId": "x" } },
            ] },
            { "columns": [{ "component": { "entityType": "link", "entityId": "l1" } }] },
            { "columns": null },
        ],
    });

    assert_eq!(
      board_entity_ids(Some("mixed"), &parameters),
      [
        vec!["w1".to_owned(), "w2".to_owned()],
        vec!["p1".to_owned()],
        vec!["l1".to_owned()]
      ]
    );
    // An unknown type falls back to the parameters' entity.
    assert_eq!(
      board_entity_ids(Some("x"), &json!({ "linkId": "l1" })),
      [vec![], vec![], vec!["l1".to_owned()]]
    );
    assert_eq!(board_entity_ids(None, &json!(null)), <[Vec<String>; 3]>::default());
  }
}
