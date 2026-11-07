//! `/api/me/*`, ported from upstream's `src/app/api/me`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use serde_json::{
  Value,
  json,
};
use uuid::Uuid;

use super::params::{
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
use crate::queries::{
  api_key,
  team,
  user,
  website,
};
use crate::state::AppState;
use crate::{
  crypto,
  js,
};

/// `GET /api/me`: the caller's auth context.
pub async fn get(auth: Auth) -> Json<Value> {
  Json(auth.to_json())
}

/// `GET /api/me/teams`
pub async fn teams(
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

/// `GET /api/me/websites`: like `/api/websites`, without search.
pub async fn websites(
  State(state): State<AppState>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let filters = params.filters(PAGING_SORTING)?;
  let user_id = auth?.user()?.id;

  let scope = website::Scope::for_user(user_id, params.flag("includeTeams"));

  Ok(Json(website::list(&state.db, scope, filters).await?))
}

/// `POST /api/me/password`. Changing the password also signs the user out
/// everywhere, since session tokens are bound to the password hash.
pub async fn change_password(
  State(state): State<AppState>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let current = body::string(object, "currentPassword")?;
  let new = body::string(object, "newPassword")?;
  if body::js_len(new) < 8 {
    return Err(AppError::bad_request());
  }

  let user_id = auth?.user()?.id;
  let user = user::get(&state.db, user_id).await?.ok_or(AppError::Unauthorized)?;

  if !password::verify(current, &user.password).await {
    return Err(AppError::BadRequest("Current password is incorrect".to_owned()));
  }

  let changes = user::UserUpdate {
    password_hash: Some(password::hash(new).await),
    ..user::UserUpdate::default()
  };

  Ok(Json(user::update(&state.db, user_id, &changes).await?))
}

const API_KEY_PREFIX: &str = "umami_";
const API_KEY_LENGTH: usize = 32;
/// How much of a key is kept for display: the prefix and 8 more characters.
const API_KEY_DISPLAY_LENGTH: usize = API_KEY_PREFIX.len() + 8;

/// `GET /api/me/api-keys`: the caller's keys, newest first (never the keys themselves).
pub async fn list_api_keys(State(state): State<AppState>, auth: Auth) -> AppResult<Json<Value>> {
  let keys = api_key::list(&state.db, auth.user()?.id).await?;

  Ok(Json(json!(keys)))
}

/// `POST /api/me/api-keys`: creates a key. Its plaintext is returned only here;
/// only its hash is stored.
pub async fn create_api_key(
  State(state): State<AppState>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let name = body::string(body::object(&body)?, "name")?.trim();
  if !(1..=255).contains(&body::js_len(name)) {
    return Err(AppError::bad_request());
  }
  let user_id = auth?.user()?.id;

  let key = format!("{API_KEY_PREFIX}{}", crypto::random_chars(API_KEY_LENGTH));
  let id = state.new_id();

  let prefix = &key[..API_KEY_DISPLAY_LENGTH];
  let created_at = api_key::create(&state.db, id, user_id, name, &crypto::hash(&[&key]), prefix).await?;

  Ok(Json(json!({
      "id": id,
      "name": name,
      "keyPrefix": prefix,
      "createdAt": created_at.map(js::date),
      "key": key,
  })))
}

/// `DELETE /api/me/api-keys/{keyId}`
pub async fn delete_api_key(
  State(state): State<AppState>,
  auth: Auth,
  Path(key_id): Path<String>,
) -> AppResult<Json<Value>> {
  let user_id = auth.user()?.id;
  let key_id: Uuid = key_id.parse().map_err(|_| AppError::NotFound)?;

  if !api_key::delete(&state.db, key_id, user_id).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
}
