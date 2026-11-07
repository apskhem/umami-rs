//! `/api/auth/*`, ported from upstream's `src/app/api/auth`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{
  Value,
  json,
};

use crate::auth::{
  self,
  Auth,
  password,
};
use crate::error::{
  AppError,
  AppResult,
};
use crate::queries::{
  team,
  user,
};
use crate::state::AppState;
use crate::{
  js,
  queries,
};

/// Upstream's seeded password hash, checked against when a username is unknown so
/// that unknown and known usernames take the same time to reject.
const DUMMY_HASH: &str = "$2b$10$BUli0c.muyCW1ErNJc3jL.vFRFtFJWrT8/GcR4A.sUdCznaXiqFXa";

/// `POST /api/auth/login`
pub async fn login(State(state): State<AppState>, body: Bytes) -> AppResult<Json<Value>> {
  let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
  let (Some(username), Some(password)) = (body["username"].as_str(), body["password"].as_str()) else {
    return Err(AppError::bad_request());
  };

  let incorrect = || AppError::unauthorized_code("incorrect-username-password");

  let Some(user) = user::by_username(&state.db, username).await? else {
    password::verify(password, DUMMY_HASH).await;
    return Err(incorrect());
  };
  if !password::verify(password, &user.password).await {
    return Err(incorrect());
  }

  let secret = &state.config.secret;

  if queries::two_factor::enabled(&state.db, user.user_id).await? {
    if state.config.two_factor_key.is_none() {
      return Err(two_factor_not_configured());
    }

    return Ok(Json(json!({
        "requiresTwoFactor": true,
        "partialToken": auth::partial_token(&user, secret),
    })));
  }

  let token = auth::session_token(&user, secret);
  let teams = team::for_user(&state.db, user.user_id).await?;

  // Upstream echoes the username as submitted (`ADMIN`), not as stored.
  Ok(Json(json!({
      "token": token,
      "user": {
          "id": user.user_id,
          "username": user.username,
          "role": user.role,
          "createdAt": user.created_at.map(js::date),
          "isAdmin": user.role == "admin",
          "teams": teams,
      },
  })))
}

/// `POST /api/auth/verify`: the current user and their teams.
pub async fn verify(State(state): State<AppState>, auth: Auth) -> AppResult<Json<Value>> {
  let user = auth.user()?;
  let teams = team::for_user(&state.db, user.id).await?;

  let mut body = json!(user);
  body["teams"] = json!(teams);

  Ok(Json(body))
}

/// `POST /api/auth/logout`: session tokens are stateless (as upstream without
/// Redis), so this only confirms the caller was signed in.
pub async fn logout(_auth: Auth) -> Json<Value> {
  Json(json!({ "ok": true }))
}

/// How long a token from `/api/auth/sso` lasts, as upstream's Redis sessions.
const SSO_TOKEN_SECONDS: i64 = 24 * 60 * 60;

/// `POST /api/auth/sso`: a fresh session token for the signed-in user, valid for
/// a day. Upstream stores this session in Redis and fails without it; this
/// token carries its own expiry, so no store is needed.
pub async fn sso(State(state): State<AppState>, auth: Auth) -> AppResult<Json<Value>> {
  let user = auth.user()?;
  let stored = user::get(&state.db, user.id).await?.ok_or(AppError::Unauthorized)?;
  let token = auth::expiring_session_token(&stored, &state.config.secret, SSO_TOKEN_SECONDS);

  Ok(Json(json!({ "user": user, "token": token })))
}

fn two_factor_not_configured() -> AppError {
  AppError::Coded {
    status: StatusCode::SERVICE_UNAVAILABLE,
    code: "two-factor-error-not-configured",
    message: "TWO_FACTOR_ENCRYPTION_KEY is missing or invalid".to_owned(),
  }
}
