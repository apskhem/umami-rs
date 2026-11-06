//! `/api/me/*`, ported from upstream's `src/app/api/me`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use chrono::{
  DateTime,
  Utc,
};
use serde::Serialize;
use serde_json::{
  Value,
  json,
};
use uuid::Uuid;

use super::body;
use super::params::{
  PAGING_SORTING,
  Params,
};
use super::users::website_scope;
use crate::auth::{
  Auth,
  password,
  store,
};
use crate::error::{
  AppError,
  AppResult,
};
use crate::queries::{
  team,
  website,
};
use crate::state::AppState;
use crate::{
  crypto,
  json as js,
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

  let scope = website_scope(&params, user_id);

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
  let user = store::user(&state.db, user_id).await?.ok_or(AppError::Unauthorized)?;

  if !password::verify(current, &user.password).await {
    return Err(AppError::BadRequest("Current password is incorrect".to_owned()));
  }

  let hash = password::hash(new).await;
  let (id, username, role, created_at): (Uuid, String, String, Option<DateTime<Utc>>) = sqlx::query_as(
    r#"update "user" set password = $2, updated_at = now() where user_id = $1
           returning user_id, username, role, created_at"#,
  )
  .bind(user_id)
  .bind(hash)
  .fetch_one(&state.db)
  .await?;

  Ok(Json(json!({
      "id": id,
      "username": username,
      "role": role,
      "createdAt": created_at.map(js::date),
  })))
}

const API_KEY_PREFIX: &str = "umami_";
const API_KEY_LENGTH: usize = 32;
/// How much of a key is kept for display: the prefix and 8 more characters.
const API_KEY_DISPLAY_LENGTH: usize = API_KEY_PREFIX.len() + 8;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct ApiKeySummary {
  #[sqlx(rename = "api_key_id")]
  id: Uuid,
  name: String,
  key_prefix: String,
  #[serde(serialize_with = "js::serialize_date")]
  last_used_at: Option<DateTime<Utc>>,
  #[serde(serialize_with = "js::serialize_date")]
  created_at: Option<DateTime<Utc>>,
}

/// `GET /api/me/api-keys`: the caller's keys, newest first (never the keys themselves).
pub async fn list_api_keys(State(state): State<AppState>, auth: Auth) -> AppResult<Json<Value>> {
  let keys: Vec<ApiKeySummary> = sqlx::query_as(
    "select api_key_id, name, key_prefix, last_used_at, created_at from api_key \
         where user_id = $1 order by created_at desc",
  )
  .bind(auth.user()?.id)
  .fetch_all(&state.db)
  .await?;

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
  let id = crypto::random_uuid(state.config.collect.use_uuidv7);

  let created_at: Option<DateTime<Utc>> = sqlx::query_scalar(
    "insert into api_key (api_key_id, user_id, name, key_hash, key_prefix) \
         values ($1, $2, $3, $4, $5) returning created_at",
  )
  .bind(id)
  .bind(user_id)
  .bind(name)
  .bind(crypto::hash(&[&key]))
  .bind(&key[..API_KEY_DISPLAY_LENGTH])
  .fetch_one(&state.db)
  .await?;

  Ok(Json(json!({
      "id": id,
      "name": name,
      "keyPrefix": &key[..API_KEY_DISPLAY_LENGTH],
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

  let deleted = sqlx::query("delete from api_key where api_key_id = $1 and user_id = $2")
    .bind(key_id)
    .bind(user_id)
    .execute(&state.db)
    .await?;

  if deleted.rows_affected() == 0 {
    return Err(AppError::NotFound);
  }

  Ok(Json(json!({ "ok": true })))
}
