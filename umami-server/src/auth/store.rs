//! Queries behind authentication, mirroring upstream's `src/queries/prisma`
//! (`getUser`, `getUserByUsername`, `getAllUserTeams`, `getApiKeyByHash`).

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// A non-deleted user, with the columns upstream's `getUser` selects.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub user_id: Uuid,
    pub username: String,
    pub password: String,
    pub role: String,
    pub created_at: Option<DateTime<Utc>>,
    pub two_factor_required: bool,
}

const USER_COLUMNS: &str =
    r#"select user_id, username, password, role, created_at, two_factor_required from "user""#;

pub async fn user(db: &PgPool, user_id: Uuid) -> sqlx::Result<Option<User>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{USER_COLUMNS} where user_id = $1 and deleted_at is null"
    )))
    .bind(user_id)
    .fetch_optional(db)
    .await
}

/// Looks up a user by username, which upstream stores lowercased.
pub async fn user_by_username(db: &PgPool, username: &str) -> sqlx::Result<Option<User>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{USER_COLUMNS} where username = $1 and deleted_at is null"
    )))
    .bind(username.to_lowercase())
    .fetch_optional(db)
    .await
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamSummary {
    #[sqlx(rename = "team_id")]
    pub id: Uuid,
    pub name: String,
    pub logo_url: Option<String>,
}

/// The non-deleted teams a user belongs to (upstream's `getAllUserTeams`).
pub async fn user_teams(db: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<TeamSummary>> {
    sqlx::query_as(
        "select team_id, name, logo_url from team t \
         where deleted_at is null \
         and exists (select 1 from team_user tu where tu.team_id = t.team_id and tu.user_id = $1)",
    )
    .bind(user_id)
    .fetch_all(db)
    .await
}

/// Whether the user has completed 2FA setup.
pub async fn two_factor_enabled(db: &PgPool, user_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "select exists(select 1 from two_factor_auth where user_id = $1 and is_enabled)",
    )
    .bind(user_id)
    .fetch_one(db)
    .await
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKey {
    pub api_key_id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub last_used_at: Option<DateTime<Utc>>,
}

pub async fn api_key_by_hash(db: &PgPool, key_hash: &str) -> sqlx::Result<Option<ApiKey>> {
    sqlx::query_as(
        "select api_key_id, user_id, name, last_used_at from api_key where key_hash = $1",
    )
    .bind(key_hash)
    .fetch_optional(db)
    .await
}

pub async fn touch_api_key(db: &PgPool, api_key_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("update api_key set last_used_at = now() where api_key_id = $1")
        .bind(api_key_id)
        .execute(db)
        .await?;

    Ok(())
}
