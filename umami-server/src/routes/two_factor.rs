//! `/api/2fa/*` and the admin 2FA policy routes, ported from upstream's
//! `src/app/api/2fa` and `src/app/api/admin/**/2fa`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  State,
};
use axum::http::{
  HeaderMap,
  StatusCode,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use qrcode::render::svg;
use qrcode::{
  EcLevel,
  QrCode,
};
use serde_json::{
  Value,
  json,
};
use uuid::Uuid;

use super::body;
use crate::auth::{
  self,
  Auth,
  PARTIAL_AUTH_TOKEN_TYPE,
  password,
  store,
  totp,
  two_factor,
};
use crate::error::{
  AppError,
  AppResult,
};
use crate::state::AppState;
use crate::{
  json as js,
  jwt,
};

fn key(state: &AppState) -> AppResult<&[u8; 32]> {
  state
    .config
    .two_factor_key
    .as_ref()
    .ok_or_else(two_factor::not_configured)
}

/// A six-character `token` field (`z.string().length(6)`).
fn six_char_token(object: &serde_json::Map<String, Value>) -> AppResult<&str> {
  let token = body::string(object, "token")?;
  if body::js_len(token) != 6 {
    return Err(AppError::bad_request());
  }
  Ok(token)
}

/// `GET /api/2fa/status`
pub async fn status(State(state): State<AppState>, auth: Auth) -> AppResult<Json<Value>> {
  let user = auth.user()?;
  let enabled = two_factor::get(&state.db, user.id)
    .await?
    .is_some_and(|tf| tf.is_enabled);
  let global = two_factor::global_required(&state.db).await?;

  // 2FA cannot be set up without an encryption key, so it is never required.
  if state.config.two_factor_key.is_none() {
    return Ok(Json(json!({
        "isEnabled": enabled,
        "isRequired": false,
        "requiredReason": null,
        "isConfigured": false,
        "globalRequired": global,
    })));
  }

  let reason = two_factor::requirement(&state.db, user.id).await?;

  Ok(Json(json!({
      "isEnabled": enabled,
      "isRequired": reason.is_some(),
      "requiredReason": reason,
      "isConfigured": true,
      "globalRequired": reason == Some("global"),
  })))
}

/// `POST /api/2fa/setup/initiate`: a new secret for the authenticator app.
pub async fn initiate(State(state): State<AppState>, auth: Auth) -> AppResult<Json<Value>> {
  let user = auth.user()?;
  let key = key(&state)?;

  let Some(user) = store::user(&state.db, user.id).await? else {
    return Err(AppError::BadRequest("User not found".to_owned()));
  };
  if two_factor::get(&state.db, user.user_id)
    .await?
    .is_some_and(|tf| tf.is_enabled)
  {
    return Err(AppError::coded(
      StatusCode::BAD_REQUEST,
      "two-factor-error-already-enabled",
      "2FA is already enabled",
    ));
  }

  let secret = totp::generate_secret();
  let uri = totp::otpauth_uri(&secret, &user.username);
  two_factor::save_pending(&state.db, user.user_id, &two_factor::encrypt_secret(key, &secret)).await?;

  // The plaintext secret is returned once, for manual entry.
  Ok(Json(
    json!({ "qrCodeDataUrl": qr_code_data_url(&uri), "manualKey": secret }),
  ))
}

/// The URI as a QR code image. Upstream renders a PNG; this is an SVG of the same
/// code, which browsers display the same way.
fn qr_code_data_url(uri: &str) -> String {
  let svg = QrCode::with_error_correction_level(uri, EcLevel::H)
    .expect("an otpauth URI fits in a QR code")
    .render::<svg::Color>()
    .min_dimensions(200, 200)
    .build();

  format!("data:image/svg+xml;base64,{}", BASE64.encode(svg))
}

/// `POST /api/2fa/setup/confirm`: enables 2FA once the user proves the app works.
pub async fn confirm(State(state): State<AppState>, auth: AppResult<Auth>, body: Bytes) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let token = six_char_token(body::object(&body)?)?;
  let user_id = auth?.user()?.id;
  let key = key(&state)?;

  let pending = two_factor::get(&state.db, user_id).await?.filter(|tf| !tf.is_enabled);
  let Some(pending) = pending else {
    return Err(AppError::coded(
      StatusCode::BAD_REQUEST,
      "two-factor-error-no-pending-setup",
      "No pending 2FA setup found",
    ));
  };

  two_factor::check_rate_limit(&state.db, user_id).await?;
  two_factor::verify_code(&state.db, key, user_id, &pending.secret, token).await?;

  let (backup_codes, hashes) = two_factor::generate_backup_codes().await;
  two_factor::enable(&state.db, user_id, &hashes, token).await?;
  two_factor::reset_rate_limit(&state.db, user_id).await?;

  Ok(Json(json!({ "backupCodes": backup_codes })))
}

/// `POST /api/2fa/setup/cancel`
pub async fn cancel(State(state): State<AppState>, auth: Auth) -> AppResult<Json<Value>> {
  two_factor::delete_pending(&state.db, auth.user()?.id).await?;

  Ok(Json(json!({ "ok": true })))
}

/// `POST /api/2fa/disable`: needs the password and a current code, and is refused
/// while 2FA is required for the user.
pub async fn disable(State(state): State<AppState>, auth: AppResult<Auth>, body: Bytes) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let password = body::string(object, "password")?;
  let token = six_char_token(object)?;
  let user_id = auth?.user()?.id;
  let key = key(&state)?;

  if two_factor::requirement(&state.db, user_id).await?.is_some() {
    return Err(AppError::coded(
      StatusCode::FORBIDDEN,
      "two-factor-error-disable-not-allowed",
      "2FA is required and cannot be disabled",
    ));
  }

  let correct = match store::user(&state.db, user_id).await? {
    Some(user) => password::verify(password, &user.password).await,
    None => false,
  };
  if !correct {
    return Err(AppError::coded(
      StatusCode::BAD_REQUEST,
      "two-factor-error-incorrect-password",
      "Incorrect password",
    ));
  }

  let enabled = two_factor::get(&state.db, user_id).await?.filter(|tf| tf.is_enabled);
  let Some(enabled) = enabled else {
    return Err(AppError::coded(
      StatusCode::BAD_REQUEST,
      "two-factor-error-not-enabled",
      "2FA is not enabled",
    ));
  };

  two_factor::check_rate_limit(&state.db, user_id).await?;
  two_factor::verify_code(&state.db, key, user_id, &enabled.secret, token).await?;
  two_factor::disable(&state.db, user_id, token).await?;
  two_factor::reset_rate_limit(&state.db, user_id).await?;

  Ok(Json(json!({ "ok": true })))
}

/// `POST /api/2fa/verify`: completes a 2FA login with a code or a backup code,
/// exchanging the partial token from `/api/auth/login` for a session.
pub async fn verify(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> AppResult<Json<Value>> {
  let Some(token) = auth::bearer_token(&headers) else {
    return Err(AppError::unauthorized_code("two-factor-error-missing-token"));
  };

  let claims: Option<Value> = jwt::verify_secure(&token, &state.config.secret);
  let user_id = claims
    .filter(|claims| claims["type"] == PARTIAL_AUTH_TOKEN_TYPE)
    .and_then(|claims| claims["userId"].as_str().and_then(|id| id.parse::<Uuid>().ok()));
  let Some(user_id) = user_id else {
    return Err(AppError::unauthorized_code("two-factor-error-invalid-partial-token"));
  };

  let key = key(&state)?;

  // Exactly one of `token` (six characters) or `backupCode`, and nothing else.
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let credential = match (object.len(), object.get("token"), object.get("backupCode")) {
    (1, Some(Value::String(code)), None) if body::js_len(code) == 6 => Credential::Code(code),
    (1, None, Some(Value::String(code))) if !code.is_empty() => Credential::BackupCode(code),
    _ => return Err(AppError::bad_request()),
  };

  let Some(user) = store::user(&state.db, user_id).await? else {
    return Err(AppError::Unauthorized);
  };

  let enabled = two_factor::get(&state.db, user_id).await?.filter(|tf| tf.is_enabled);
  let Some(enabled) = enabled else {
    return Err(AppError::coded(
      StatusCode::BAD_REQUEST,
      "two-factor-error-not-enabled",
      "2FA not enabled for this user",
    ));
  };

  two_factor::check_rate_limit(&state.db, user_id).await?;

  match credential {
    Credential::BackupCode(code) => two_factor::verify_backup_code(&state.db, user_id, code).await?,
    Credential::Code(code) => {
      two_factor::verify_code(&state.db, key, user_id, &enabled.secret, code).await?;
      two_factor::mark_used(&mut *state.db.acquire().await?, user_id, code).await?;
    }
  }
  two_factor::reset_rate_limit(&state.db, user_id).await?;

  let teams = store::user_teams(&state.db, user_id).await?;

  Ok(Json(json!({
      "token": auth::session_token(&user, &state.config.secret),
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

enum Credential<'a> {
  Code(&'a str),
  BackupCode(&'a str),
}

/// Admin routes answer non-admins with 401, as upstream does.
fn require_admin(auth: &Auth) -> AppResult<()> {
  match &auth.user {
    Some(user) if user.is_admin => Ok(()),
    _ => Err(AppError::Unauthorized),
  }
}

/// The `required` flag of the admin policy routes, with upstream's check order:
/// body, then auth, then admin, then configuration.
async fn policy_body(state: &AppState, auth: AppResult<Auth>, body: &Bytes) -> AppResult<bool> {
  let body = body::parse(body);
  let required = body::boolean(body::object(&body)?, "required")?;
  require_admin(&auth?)?;

  if required {
    key(state)?;
  }

  Ok(required)
}

/// `POST /api/admin/2fa/global`: require 2FA from everyone.
pub async fn set_global(State(state): State<AppState>, auth: AppResult<Auth>, body: Bytes) -> AppResult<Json<Value>> {
  let required = policy_body(&state, auth, &body).await?;
  two_factor::set_global_required(&state.db, required).await?;

  Ok(Json(json!({ "ok": true, "required": required })))
}

/// `GET /api/admin/users/{userId}/2fa`
pub async fn user_status(
  State(state): State<AppState>,
  auth: Auth,
  Path(user_id): Path<String>,
) -> AppResult<Json<Value>> {
  require_admin(&auth)?;
  let user_id = user_id.parse().map_err(|_| AppError::NotFound)?;
  let enabled = two_factor::get(&state.db, user_id)
    .await?
    .is_some_and(|tf| tf.is_enabled);

  Ok(Json(json!({ "isEnabled": enabled })))
}

/// `POST /api/admin/users/{userId}/2fa`: require 2FA from one user.
pub async fn set_user(
  State(state): State<AppState>,
  Path(user_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let required = policy_body(&state, auth, &body).await?;
  let user_id: Uuid = user_id.parse().map_err(|_| AppError::NotFound)?;

  let updated = sqlx::query(r#"update "user" set two_factor_required = $2, updated_at = now() where user_id = $1"#)
    .bind(user_id)
    .bind(required)
    .execute(&state.db)
    .await?;
  if updated.rows_affected() == 0 {
    return Err(AppError::NotFound);
  }

  Ok(Json(
    json!({ "ok": true, "userId": user_id, "twoFactorRequired": required }),
  ))
}

/// `DELETE /api/admin/users/{userId}/2fa`: clears a user's 2FA (lost device).
pub async fn reset_user(
  State(state): State<AppState>,
  auth: Auth,
  Path(user_id): Path<String>,
) -> AppResult<Json<Value>> {
  require_admin(&auth)?;
  let id: Uuid = user_id.parse().map_err(|_| AppError::NotFound)?;
  let counts = two_factor::reset(&state.db, id).await?;

  Ok(Json(json!({
      "ok": true,
      "userId": user_id,
      "reset": {
          "twoFactorAuth": counts.two_factor_auth,
          "backupCodes": counts.backup_codes,
          "otpUsed": counts.otp_used,
          "rateLimit": counts.rate_limit,
      },
  })))
}

/// `POST /api/admin/teams/{teamId}/2fa`: require 2FA from a team's members.
pub async fn set_team(
  State(state): State<AppState>,
  Path(team_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let required = policy_body(&state, auth, &body).await?;
  let team_id: Uuid = team_id.parse().map_err(|_| AppError::NotFound)?;

  let updated = sqlx::query("update team set two_factor_required = $2, updated_at = now() where team_id = $1")
    .bind(team_id)
    .bind(required)
    .execute(&state.db)
    .await?;
  if updated.rows_affected() == 0 {
    return Err(AppError::NotFound);
  }

  Ok(Json(
    json!({ "ok": true, "teamId": team_id, "twoFactorRequired": required }),
  ))
}
