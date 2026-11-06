//! Request authentication: port of upstream's `checkAuth` (`src/lib/auth.ts`).
//!
//! A request is authenticated by one of:
//! - a session token (`Authorization: Bearer <secure token>`), issued at login and
//!   bound to the password it was issued for;
//! - an API key (`Authorization: Bearer umami_...`), except on credential and
//!   admin routes;
//! - a share token (`x-umami-share-token`), only together with `x-umami-share-context`.

pub mod password;
pub mod store;
pub mod totp;
pub mod two_factor;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::{crypto, json as js, jwt};

pub const SHARE_TOKEN_HEADER: &str = "x-umami-share-token";
pub const SHARE_CONTEXT_HEADER: &str = "x-umami-share-context";
const SHARE_TOKEN_TYPE: &str = "share";
pub const PARTIAL_AUTH_TOKEN_TYPE: &str = "partial-auth";

const API_KEY_PREFIX: &str = "umami_";
/// How often an API key's `last_used_at` is refreshed.
const API_KEY_LAST_USED_INTERVAL: Duration = Duration::minutes(5);
/// Routes that manage credentials or admin state, which API keys cannot reach.
const API_KEY_BLOCKED_PATHS: &[&str] = &[
    "/api/me/password",
    "/api/me/api-keys",
    "/api/2fa",
    "/api/auth",
    "/api/users",
    "/api/admin",
];

/// The authenticated user, as upstream's `getUser` returns it (without the
/// password) plus `isAdmin`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthUser {
    pub id: Uuid,
    pub username: String,
    pub role: String,
    #[serde(serialize_with = "js::serialize_date")]
    pub created_at: Option<DateTime<Utc>>,
    pub two_factor_required: bool,
    pub is_admin: bool,
}

impl AuthUser {
    fn new(user: store::User) -> Self {
        Self {
            is_admin: user.role == "admin",
            id: user.user_id,
            username: user.username,
            role: user.role,
            created_at: user.created_at,
            two_factor_required: user.two_factor_required,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AuthKind {
    Session,
    Share,
    ApiKey { id: Uuid, name: String },
}

/// An authenticated request.
#[derive(Debug, Clone)]
pub struct Auth {
    pub token: Option<String>,
    pub user: Option<AuthUser>,
    pub share_token: Option<Value>,
    pub kind: AuthKind,
}

impl Auth {
    /// The authenticated user; share-only access has none.
    pub fn user(&self) -> AppResult<&AuthUser> {
        self.user.as_ref().ok_or(AppError::Unauthorized)
    }

    /// The auth object as upstream's `GET /api/me` returns it.
    pub fn to_json(&self) -> Value {
        let mut object = json!({ "token": self.token });

        match &self.kind {
            AuthKind::ApiKey { id, name } => {
                object["user"] = json!(self.user);
                object["authType"] = json!("api-key");
                object["apiKey"] = json!({ "id": id, "name": name });
            }
            kind => {
                object["shareToken"] = json!(self.share_token);
                object["user"] = json!(self.user);
                object["authType"] = json!(if matches!(kind, AuthKind::Session) {
                    "session"
                } else {
                    "share"
                });
            }
        }

        if self.token.is_none() {
            object.as_object_mut().unwrap().remove("token");
        }

        object
    }
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        authenticate(state, &parts.headers, parts.uri.path())
            .await?
            .ok_or(AppError::Unauthorized)
    }
}

/// Upstream's `getBearerToken`: the second space-separated part of `Authorization`.
pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    value.split(' ').nth(1).map(str::to_owned)
}

#[derive(Deserialize)]
struct SessionClaims {
    #[serde(rename = "userId")]
    user_id: Option<String>,
    pwd: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// Upstream's `checkAuth`: `None` when the request is not authenticated.
pub async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
) -> AppResult<Option<Auth>> {
    let secret = &state.config.secret;
    let token = bearer_token(headers);

    if let Some(key) = token.as_deref().filter(|t| t.starts_with(API_KEY_PREFIX)) {
        return api_key_auth(state, key, path).await;
    }

    let claims: Option<SessionClaims> =
        token.as_deref().and_then(|t| jwt::verify_secure(t, secret));

    // The partial token from the password step of a 2FA login only proves the
    // password; it is valid solely for completing the challenge.
    if claims.as_ref().and_then(|c| c.kind.as_deref()) == Some(PARTIAL_AUTH_TOKEN_TYPE) {
        return Ok(None);
    }

    let share_token = share_token(headers, secret);
    let mut user = None;

    if let Some(claims) = claims
        && let Some(user_id) = claims.user_id.as_deref().and_then(|id| id.parse().ok())
        && let Some(found) = store::user(&state.db, user_id).await?
    {
        // Tokens are bound to the password hash they were issued for; legacy tokens
        // without a fingerprint stay valid.
        let current = claims.pwd.as_deref().filter(|pwd| !pwd.is_empty());
        if current.is_none_or(|pwd| crypto::hash(&[&found.password]) == pwd) {
            user = Some(AuthUser::new(found));
        }
    }

    if user.is_none() && (share_token.is_none() || !headers.contains_key(SHARE_CONTEXT_HEADER)) {
        return Ok(None);
    }

    Ok(Some(Auth {
        kind: if user.is_some() {
            AuthKind::Session
        } else {
            AuthKind::Share
        },
        token,
        user,
        share_token,
    }))
}

async fn api_key_auth(state: &AppState, key: &str, path: &str) -> AppResult<Option<Auth>> {
    let blocked = API_KEY_BLOCKED_PATHS
        .iter()
        .any(|blocked| path == *blocked || path.starts_with(&format!("{blocked}/")));
    if blocked {
        return Ok(None);
    }

    let Some(api_key) = store::api_key_by_hash(&state.db, &crypto::hash(&[key])).await? else {
        return Ok(None);
    };
    let Some(user) = store::user(&state.db, api_key.user_id).await? else {
        return Ok(None);
    };

    let stale = api_key
        .last_used_at
        .is_none_or(|used| Utc::now() - used > API_KEY_LAST_USED_INTERVAL);
    if stale {
        let db = state.db.clone();
        tokio::spawn(async move {
            if let Err(error) = store::touch_api_key(&db, api_key.api_key_id).await {
                tracing::warn!(%error, "failed to update API key last use");
            }
        });
    }

    Ok(Some(Auth {
        token: Some(key.to_owned()),
        user: Some(AuthUser::new(user)),
        share_token: None,
        kind: AuthKind::ApiKey {
            id: api_key.api_key_id,
            name: api_key.name,
        },
    }))
}

/// Upstream's `parseShareToken`: a JWT that must have been minted as a share token
/// (not, say, a cache token signed with the same secret).
fn share_token(headers: &HeaderMap, secret: &str) -> Option<Value> {
    let token = headers.get(SHARE_TOKEN_HEADER)?.to_str().ok()?;
    jwt::verify::<Value>(token, secret).filter(|claims| claims["type"] == SHARE_TOKEN_TYPE)
}

/// A session token for `user`, as upstream's login issues it (without Redis).
pub fn session_token(user: &store::User, secret: &str) -> String {
    jwt::sign_secure(
        &json!({
            "userId": user.user_id,
            "role": user.role,
            // Binds the token to the current password: changing it signs the user out.
            "pwd": crypto::hash(&[&user.password]),
            "iat": Utc::now().timestamp(),
        }),
        secret,
    )
}

/// A session token for `user` that expires after `seconds`.
pub fn expiring_session_token(user: &store::User, secret: &str, seconds: i64) -> String {
    let now = Utc::now().timestamp();

    jwt::sign_secure(
        &json!({
            "userId": user.user_id,
            "role": user.role,
            "pwd": crypto::hash(&[&user.password]),
            "iat": now,
            "exp": now + seconds,
        }),
        secret,
    )
}

/// The short-lived token for completing a 2FA login.
pub fn partial_token(user: &store::User, secret: &str) -> String {
    let now = Utc::now().timestamp();

    jwt::sign_secure(
        &json!({
            "userId": user.user_id,
            "type": PARTIAL_AUTH_TOKEN_TYPE,
            "iat": now,
            "exp": now + 5 * 60,
        }),
        secret,
    )
}
