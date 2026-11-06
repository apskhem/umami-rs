//! Users (upstream's `user.ts`).

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use super::team::{ROLE_OWNER, delete_teams};
use super::website::delete_websites;
use super::{Filters, IDS, delete_where, into_map, now, restrict, set_null, sort_column};
use crate::auth::store;
use crate::json as js;

/// Upstream's `USER_SORT_FIELDS`.
pub const SORT_FIELDS: &[&str] = &["username", "role", "createdAt"];

/// A user as upstream's `getUser` selects it.
pub fn to_json(user: &store::User) -> Value {
    json!({
        "id": user.user_id,
        "username": user.username,
        "role": user.role,
        "createdAt": user.created_at.map(js::date),
        "twoFactorRequired": user.two_factor_required,
    })
}

/// Whether the username is taken, including by deleted users.
pub async fn username_taken(db: &PgPool, username: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(r#"select exists(select 1 from "user" where username = $1)"#)
        .bind(username.to_lowercase())
        .fetch_one(db)
        .await
}

#[derive(sqlx::FromRow)]
struct UserRow {
    user_id: Uuid,
    username: String,
    role: String,
    logo_url: Option<String>,
    display_name: Option<String>,
    two_factor_required: bool,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    deleted_at: Option<DateTime<Utc>>,
    website_count: i64,
}

/// Upstream's `getUsers` for the admin list: non-deleted users without their
/// passwords, newest first, with their website counts.
pub async fn list(db: &PgPool, filters: Filters) -> sqlx::Result<Value> {
    let filters = filters.sanitize(SORT_FIELDS, Some("createdAt"), Some(true));
    let search = filters.search_pattern();

    let push_where = |builder: &mut QueryBuilder<sqlx::Postgres>| {
        builder.push(" where u.deleted_at is null");
        if let Some(search) = &search {
            builder
                .push(" and u.username ilike ")
                .push_bind(search.clone());
        }
    };

    let mut query = QueryBuilder::new(
        "select user_id, username, role, logo_url, display_name, two_factor_required, \
         created_at, updated_at, deleted_at, \
         (select count(*) from website w where w.user_id = u.user_id and w.deleted_at is null) \
           as website_count \
         from \"user\" u",
    );
    push_where(&mut query);
    filters.push_order(&mut query, sort_column, None);
    filters.push_limit(&mut query);
    let users: Vec<UserRow> = query.build_query_as().fetch_all(db).await?;

    let mut count = QueryBuilder::new(r#"select count(*) from "user" u"#);
    push_where(&mut count);
    let count: i64 = count.build_query_scalar().fetch_one(db).await?;

    let data = users
        .into_iter()
        .map(|user| {
            Value::Object(into_map(json!({
                "id": user.user_id,
                "username": user.username,
                "role": user.role,
                "logoUrl": user.logo_url,
                "displayName": user.display_name,
                "twoFactorRequired": user.two_factor_required,
                "createdAt": user.created_at.map(js::date),
                "updatedAt": user.updated_at.map(js::date),
                "deletedAt": user.deleted_at.map(js::date),
                "_count": { "websites": user.website_count },
            })))
        })
        .collect();

    Ok(filters.page(data, count))
}

/// Upstream's `createUser`; returns `{ id, username, role }`.
pub async fn create(
    db: &PgPool,
    user_id: Uuid,
    username: &str,
    password_hash: &str,
    role: &str,
) -> sqlx::Result<Value> {
    let (id, username, role): (Uuid, String, String) = sqlx::query_as(
        r#"insert into "user" (user_id, username, password, role, created_at, updated_at)
           values ($1, $2, $3, $4, $5, $5) returning user_id, username, role"#,
    )
    .bind(user_id)
    .bind(username)
    .bind(password_hash)
    .bind(role)
    .bind(now())
    .fetch_one(db)
    .await?;

    Ok(json!({ "id": id, "username": username, "role": role }))
}

/// Changes to a user; `None` fields are kept.
#[derive(Default)]
pub struct UserUpdate {
    pub password_hash: Option<String>,
    pub role: Option<String>,
    pub username: Option<String>,
}

/// Upstream's `updateUser`; returns `{ id, username, role, createdAt }`.
pub async fn update(db: &PgPool, user_id: Uuid, changes: &UserUpdate) -> sqlx::Result<Value> {
    let (id, username, role, created_at): (Uuid, String, String, Option<DateTime<Utc>>) =
        sqlx::query_as(
            r#"update "user" set password = coalesce($2, password), role = coalesce($3, role),
               username = coalesce($4, username), updated_at = $5
               where user_id = $1 returning user_id, username, role, created_at"#,
        )
        .bind(user_id)
        .bind(&changes.password_hash)
        .bind(&changes.role)
        .bind(&changes.username)
        .bind(now())
        .fetch_one(db)
        .await?;

    Ok(json!({
        "id": id,
        "username": username,
        "role": role,
        "createdAt": created_at.map(js::date),
    }))
}

/// Upstream's `deleteUser` (self-hosted mode): the user, their websites and
/// those websites' data, the teams they own, and their links, pixels and boards.
///
/// With Prisma's emulated relations this fails while the websites still have
/// revenue, segments, annotations, replays or heatmap data, which upstream does
/// not delete first; websites the user created for other teams lose
/// `created_by`; and the teams' remaining websites, links, pixels and boards
/// lose their `team_id`. `false` if the user does not exist.
pub async fn delete(db: &PgPool, user_id: Uuid) -> anyhow::Result<bool> {
    let user = [user_id];
    let websites: Vec<Uuid> =
        sqlx::query_scalar("select website_id from website where user_id = $1")
            .bind(user_id)
            .fetch_all(db)
            .await?;
    let teams: Vec<Uuid> = sqlx::query_scalar(
        "select team_id from team t where exists (select 1 from team_user tu \
         where tu.team_id = t.team_id and tu.user_id = $1 and tu.role = $2)",
    )
    .bind(user_id)
    .bind(ROLE_OWNER)
    .fetch_all(db)
    .await?;

    const OWNED: &str = "(user_id = $1 or team_id = any($2))";
    let entities: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select link_id from link where {OWNED} \
         union all select pixel_id from pixel where {OWNED} \
         union all select board_id from board where {OWNED}"
    )))
    .bind(user_id)
    .bind(&teams)
    .fetch_all(db)
    .await?;

    let mut tx = db.begin().await?;

    delete_where(&mut tx, "event_data", "website_id", &websites).await?;
    delete_where(&mut tx, "session_data", "website_id", &websites).await?;

    let events = "select event_id from website_event where website_id = any($1)";
    restrict(&mut tx, "event_data", "website_event_id", events, &websites).await?;
    delete_where(&mut tx, "website_event", "website_id", &websites).await?;

    let sessions = "select session_id from session where website_id = any($1)";
    for child in ["website_event", "session_data", "revenue"] {
        restrict(&mut tx, child, "session_id", sessions, &websites).await?;
    }
    delete_where(&mut tx, "session", "website_id", &websites).await?;

    sqlx::query("delete from team_user where team_id = any($1) or user_id = $2")
        .bind(&teams)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    delete_teams(&mut tx, &teams).await?;

    sqlx::query("delete from report where website_id = any($1) or user_id = $2")
        .bind(&websites)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    delete_where(&mut tx, "share", "entity_id", &entities).await?;

    for table in ["link", "pixel", "board"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "delete from {table} where {OWNED}"
        )))
        .bind(user_id)
        .bind(&teams)
        .execute(&mut *tx)
        .await?;
    }
    delete_websites(&mut tx, &websites).await?;

    // `user.delete` and its emulated relations.
    for child in ["team_user", "report"] {
        restrict(&mut tx, child, "user_id", IDS, &user).await?;
    }
    for (table, column) in [
        ("website", "user_id"),
        ("website", "created_by"),
        ("link", "user_id"),
        ("pixel", "user_id"),
        ("board", "user_id"),
    ] {
        set_null(&mut tx, table, column, &user).await?;
    }
    for table in [
        "api_key",
        "two_factor_auth",
        "two_factor_backup_code",
        "two_factor_otp_used",
        "two_factor_rate_limit",
    ] {
        delete_where(&mut tx, table, "user_id", &user).await?;
    }

    let deleted = sqlx::query(r#"delete from "user" where user_id = $1"#)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if deleted.rows_affected() == 0 {
        return Ok(false);
    }

    tx.commit().await?;

    Ok(true)
}
