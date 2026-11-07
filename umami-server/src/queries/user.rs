//! Users (upstream's `user.ts`).

use chrono::{
  DateTime,
  Utc,
};
use serde::Serialize;
use serde_json::{
  Value,
  json,
};
use sqlx::{
  PgPool,
  QueryBuilder,
};
use uuid::Uuid;

use super::team::{
  ROLE_OWNER,
  delete_owned_entities,
  delete_teams,
};
use super::{
  Filters,
  delete_where,
  into_map,
  model_json,
  now,
  sort_column,
  website,
};
use crate::js;

/// Upstream's `USER_SORT_FIELDS`.
pub const SORT_FIELDS: &[&str] = &["username", "role", "createdAt"];

/// A non-deleted user, with the columns upstream's `getUser` selects.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
  #[serde(rename = "id")]
  pub user_id: Uuid,
  pub username: String,
  #[serde(skip)]
  pub password: String,
  pub role: String,
  #[serde(serialize_with = "js::serialize_date")]
  pub created_at: Option<DateTime<Utc>>,
  pub two_factor_required: bool,
}

impl User {
  /// The user as upstream's `getUser` returns it (without the password).
  pub fn to_json(&self) -> Value {
    Value::Object(model_json(self))
  }
}

const USER_COLUMNS: &str = r#"select user_id, username, password, role, created_at, two_factor_required from "user""#;

/// Upstream's `getUser`: a user that is not deleted.
pub async fn get(db: &PgPool, user_id: Uuid) -> sqlx::Result<Option<User>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "{USER_COLUMNS} where user_id = $1 and deleted_at is null"
  )))
  .bind(user_id)
  .fetch_optional(db)
  .await
}

/// Upstream's `getUserByUsername`: usernames are stored lowercased.
pub async fn by_username(db: &PgPool, username: &str) -> sqlx::Result<Option<User>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "{USER_COLUMNS} where username = $1 and deleted_at is null"
  )))
  .bind(username.to_lowercase())
  .fetch_optional(db)
  .await
}

/// The user holding a username, including deleted users.
pub async fn username_owner(db: &PgPool, username: &str) -> sqlx::Result<Option<Uuid>> {
  sqlx::query_scalar(r#"select user_id from "user" where username = $1"#)
    .bind(username.to_lowercase())
    .fetch_optional(db)
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
      builder.push(" and u.username ilike ").push_bind(search.clone());
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
  filters.push_order(&mut query, sort_column, "u.created_at desc", "u.user_id");
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
  let (id, username, role, created_at): (Uuid, String, String, Option<DateTime<Utc>>) = sqlx::query_as(
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

/// Upstream's `deleteUser` (self-hosted mode): the user, their websites, links,
/// pixels and boards, and the teams they own with everything in them. Websites
/// they created for other teams are kept, without `created_by`. `false` if the
/// user does not exist.
///
/// Upstream deletes less of each website's data first, so it fails (with a 500)
/// for users whose websites have revenue, segments, annotations, replays or
/// heatmap data; it also leaves the websites' share links behind, and keeps the
/// owned teams' websites, links, pixels and boards without a team. umami-rs
/// deletes all of it.
pub async fn delete(db: &PgPool, user_id: Uuid) -> sqlx::Result<bool> {
  let user = [user_id];
  let mut tx = db.begin().await?;

  let teams: Vec<Uuid> = sqlx::query_scalar("select team_id from team_user where user_id = $1 and role = $2")
    .bind(user_id)
    .bind(ROLE_OWNER)
    .fetch_all(&mut *tx)
    .await?;
  delete_teams(&mut tx, &teams).await?;

  let websites: Vec<Uuid> = sqlx::query_scalar("select website_id from website where user_id = $1")
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?;
  website::purge(&mut tx, &websites).await?;
  delete_owned_entities(&mut tx, "user_id = any($1)", &user).await?;

  for table in ["team_user", "report"] {
    delete_where(&mut tx, table, "user_id", &user).await?;
  }
  // Prisma's emulated `SetNull`, which leaves `updated_at` alone.
  sqlx::query("update website set created_by = null where created_by = $1")
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
  // Upstream's cascading relations.
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
