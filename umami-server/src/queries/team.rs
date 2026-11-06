//! Teams and their members (upstream's `team.ts` and `teamUser.ts`).

use chrono::{
  DateTime,
  Utc,
};
use serde_json::{
  Map,
  Value,
  json,
};
use sqlx::{
  PgConnection,
  PgPool,
  QueryBuilder,
};
use uuid::Uuid;

use super::{
  Filters,
  delete_where,
  into_map,
  now,
  sort_column,
  website,
};
use crate::json as js;

pub const ROLE_OWNER: &str = "team-owner";
pub const ROLE_MANAGER: &str = "team-manager";
pub const ROLE_MEMBER: &str = "team-member";

/// Upstream's `TEAM_SORT_FIELDS`.
pub const SORT_FIELDS: &[&str] = &["name", "createdAt"];

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Team {
  pub team_id: Uuid,
  pub name: String,
  pub access_code: Option<String>,
  pub logo_url: Option<String>,
  pub two_factor_required: bool,
  pub created_at: Option<DateTime<Utc>>,
  pub updated_at: Option<DateTime<Utc>>,
  pub deleted_at: Option<DateTime<Utc>>,
}

const TEAM_COLUMNS: &str = "team_id, name, access_code, logo_url, two_factor_required, \
                            created_at, updated_at, deleted_at";

impl Team {
  /// The Prisma model as JSON, fields in schema order.
  pub fn to_json(&self) -> Map<String, Value> {
    let value = json!({
        "id": self.team_id,
        "name": self.name,
        "accessCode": self.access_code,
        "logoUrl": self.logo_url,
        "twoFactorRequired": self.two_factor_required,
        "createdAt": self.created_at.map(js::date),
        "updatedAt": self.updated_at.map(js::date),
        "deletedAt": self.deleted_at.map(js::date),
    });

    into_map(value)
  }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TeamUser {
  pub team_user_id: Uuid,
  pub team_id: Uuid,
  pub user_id: Uuid,
  pub role: String,
  pub created_at: Option<DateTime<Utc>>,
  pub updated_at: Option<DateTime<Utc>>,
}

const TEAM_USER_COLUMNS: &str = "team_user_id, team_id, user_id, role, created_at, updated_at";

impl TeamUser {
  pub fn to_json(&self) -> Map<String, Value> {
    let value = json!({
        "id": self.team_user_id,
        "teamId": self.team_id,
        "userId": self.user_id,
        "role": self.role,
        "createdAt": self.created_at.map(js::date),
        "updatedAt": self.updated_at.map(js::date),
    });

    into_map(value)
  }
}

/// Upstream's `getTeamUser`: the membership, if the team is not deleted.
pub async fn team_user(db: &PgPool, team_id: Uuid, user_id: Uuid) -> sqlx::Result<Option<TeamUser>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {TEAM_USER_COLUMNS} from team_user tu \
         where team_id = $1 and user_id = $2 \
         and exists (select 1 from team t where t.team_id = tu.team_id and t.deleted_at is null) \
         limit 1"
  )))
  .bind(team_id)
  .bind(user_id)
  .fetch_optional(db)
  .await
}

/// Upstream's `getTeam`, deleted or not.
pub async fn get(db: &PgPool, team_id: Uuid) -> sqlx::Result<Option<Team>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {TEAM_COLUMNS} from team where team_id = $1"
  )))
  .bind(team_id)
  .fetch_optional(db)
  .await
}

/// Whether the team exists and is not deleted.
pub async fn exists(db: &PgPool, team_id: Uuid) -> sqlx::Result<bool> {
  sqlx::query_scalar("select exists(select 1 from team where team_id = $1 and deleted_at is null)")
    .bind(team_id)
    .fetch_one(db)
    .await
}

/// A team with `include: { members: true }`.
pub async fn get_with_members(db: &PgPool, team_id: Uuid) -> sqlx::Result<Option<Value>> {
  let Some(team) = get(db, team_id).await? else {
    return Ok(None);
  };
  let members: Vec<TeamUser> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {TEAM_USER_COLUMNS} from team_user where team_id = $1 \
         order by created_at, team_user_id"
  )))
  .bind(team_id)
  .fetch_all(db)
  .await?;

  let mut object = team.to_json();
  object.insert(
    "members".to_owned(),
    members.iter().map(|m| Value::Object(m.to_json())).collect(),
  );

  Ok(Some(Value::Object(object)))
}

/// A non-deleted team by its access code, for joining.
pub async fn by_access_code(db: &PgPool, access_code: &str) -> sqlx::Result<Option<Team>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {TEAM_COLUMNS} from team where access_code = $1 and deleted_at is null"
  )))
  .bind(access_code)
  .fetch_optional(db)
  .await
}

/// Which teams a list covers.
pub enum Scope {
  /// Non-deleted teams the user belongs to (upstream's `getUserTeams`).
  Member(Uuid),
  /// Every team, deleted or not (the admin list).
  All,
}

#[derive(sqlx::FromRow)]
struct TeamRow {
  #[sqlx(flatten)]
  team: Team,
  website_count: i64,
  member_count: i64,
}

/// Upstream's `getTeams` with members (and their usernames) and counts included.
pub async fn list(db: &PgPool, scope: Scope, filters: Filters) -> sqlx::Result<Value> {
  let filters = filters.sanitize(SORT_FIELDS, None, None);
  let search = filters.search_pattern();

  let push_where = |builder: &mut QueryBuilder<sqlx::Postgres>| {
    builder.push(" where true");
    if let Scope::Member(user_id) = scope {
      builder
        .push(
          " and t.deleted_at is null and exists \
                     (select 1 from team_user tu where tu.team_id = t.team_id and tu.user_id = ",
        )
        .push_bind(user_id)
        .push(")");
    }
    if let Some(search) = &search {
      builder.push(" and t.name ilike ").push_bind(search.clone());
    }
  };

  let mut query = QueryBuilder::new(format!(
    "select {TEAM_COLUMNS}, \
         (select count(*) from website w where w.team_id = t.team_id and w.deleted_at is null) \
           as website_count, \
         (select count(*) from team_user tu where tu.team_id = t.team_id and exists \
           (select 1 from \"user\" u where u.user_id = tu.user_id and u.deleted_at is null)) \
           as member_count \
         from team t"
  ));
  push_where(&mut query);
  // Upstream leaves a member's teams unordered; these list by name.
  let default_order = match scope {
    Scope::Member(_) => "t.name asc",
    Scope::All => "t.created_at desc",
  };
  filters.push_order(&mut query, sort_column, default_order, "t.team_id");
  filters.push_limit(&mut query);
  let rows: Vec<TeamRow> = query.build_query_as().fetch_all(db).await?;

  let mut count = QueryBuilder::new("select count(*) from team t");
  push_where(&mut count);
  let count: i64 = count.build_query_scalar().fetch_one(db).await?;

  let ids: Vec<Uuid> = rows.iter().map(|row| row.team.team_id).collect();
  let members = members_with_users(db, &ids).await?;

  let data = rows
    .into_iter()
    .map(|row| {
      let mut object = row.team.to_json();
      let team_members: Vec<Value> = members
        .iter()
        .filter(|(member, _)| member.team_id == row.team.team_id)
        .map(|(member, user)| {
          let mut object = member.to_json();
          object.insert("user".to_owned(), user.clone());
          Value::Object(object)
        })
        .collect();
      object.insert("members".to_owned(), json!(team_members));
      object.insert(
        "_count".to_owned(),
        json!({ "websites": row.website_count, "members": row.member_count }),
      );
      Value::Object(object)
    })
    .collect();

  Ok(filters.page(data, count))
}

/// The members of `team_ids`, each with `user: { id, username }`.
async fn members_with_users(db: &PgPool, team_ids: &[Uuid]) -> sqlx::Result<Vec<(TeamUser, Value)>> {
  let members: Vec<TeamUser> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {TEAM_USER_COLUMNS} from team_user where team_id = any($1) \
         order by created_at, team_user_id"
  )))
  .bind(team_ids)
  .fetch_all(db)
  .await?;

  let user_ids: Vec<Uuid> = members.iter().map(|m| m.user_id).collect();
  let users = usernames(db, &user_ids).await?;

  Ok(
    members
      .into_iter()
      .map(|member| {
        let user = users
          .iter()
          .find(|(id, _)| *id == member.user_id)
          .map_or(Value::Null, |(id, username)| json!({ "id": id, "username": username }));
        (member, user)
      })
      .collect(),
  )
}

/// `(id, username)` of the given users, deleted or not.
pub(super) async fn usernames(db: &PgPool, user_ids: &[Uuid]) -> sqlx::Result<Vec<(Uuid, String)>> {
  sqlx::query_as(r#"select user_id, username from "user" where user_id = any($1)"#)
    .bind(user_ids)
    .fetch_all(db)
    .await
}

/// Upstream's `getTeamUsers`: a team's members whose users are not deleted,
/// oldest first, with `user: { id, username }`.
pub async fn members_page(db: &PgPool, team_id: Uuid, filters: Filters) -> sqlx::Result<Value> {
  let search = filters.search_pattern();
  let push_where = |builder: &mut QueryBuilder<sqlx::Postgres>| {
    builder.push(" where tu.team_id = ").push_bind(team_id).push(
      " and exists (select 1 from \"user\" u where u.user_id = tu.user_id \
                 and u.deleted_at is null",
    );
    if let Some(search) = &search {
      builder.push(" and u.username ilike ").push_bind(search.clone());
    }
    builder.push(")");
  };

  let mut query = QueryBuilder::new(format!("select {TEAM_USER_COLUMNS} from team_user tu"));
  push_where(&mut query);
  filters.push_order(&mut query, |_| None, "tu.created_at asc", "tu.team_user_id");
  filters.push_limit(&mut query);
  let members: Vec<TeamUser> = query.build_query_as().fetch_all(db).await?;

  let mut count = QueryBuilder::new("select count(*) from team_user tu");
  push_where(&mut count);
  let count: i64 = count.build_query_scalar().fetch_one(db).await?;

  let user_ids: Vec<Uuid> = members.iter().map(|m| m.user_id).collect();
  let users = usernames(db, &user_ids).await?;
  let data = members
    .into_iter()
    .map(|member| {
      let user = users
        .iter()
        .find(|(id, _)| *id == member.user_id)
        .map_or(Value::Null, |(id, username)| json!({ "id": id, "username": username }));
      let mut object = member.to_json();
      object.insert("user".to_owned(), user);
      Value::Object(object)
    })
    .collect();

  Ok(filters.page(data, count))
}

/// Upstream's `createTeam`: the team and its owner's membership, in one transaction.
pub async fn create(
  db: &PgPool,
  team_id: Uuid,
  name: &str,
  access_code: &str,
  owner: (Uuid, Uuid),
) -> sqlx::Result<(Team, TeamUser)> {
  let (team_user_id, owner_id) = owner;
  let at = now();
  let mut tx = db.begin().await?;

  let team: Team = sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "insert into team (team_id, name, access_code, created_at, updated_at) \
         values ($1, $2, $3, $4, $4) returning {TEAM_COLUMNS}"
  )))
  .bind(team_id)
  .bind(name)
  .bind(access_code)
  .bind(at)
  .fetch_one(&mut *tx)
  .await?;

  let member = insert_member(&mut tx, team_user_id, owner_id, team_id, ROLE_OWNER).await?;
  tx.commit().await?;

  Ok((team, member))
}

/// Upstream's `updateTeam`; `None` if the team does not exist.
pub async fn update(
  db: &PgPool,
  team_id: Uuid,
  name: Option<&str>,
  access_code: Option<&str>,
) -> sqlx::Result<Option<Team>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "update team set name = coalesce($2, name), access_code = case when $3 then $4 else access_code end, \
         updated_at = $5 where team_id = $1 returning {TEAM_COLUMNS}"
  )))
  .bind(team_id)
  .bind(name)
  .bind(access_code.is_some())
  .bind(access_code)
  .bind(now())
  .fetch_optional(db)
  .await
}

/// Upstream's `deleteTeam` (self-hosted mode). `false` if the team does not exist.
pub async fn delete(db: &PgPool, team_id: Uuid) -> sqlx::Result<bool> {
  let mut tx = db.begin().await?;
  if delete_teams(&mut tx, &[team_id]).await? == 0 {
    return Ok(false);
  }
  tx.commit().await?;

  Ok(true)
}

/// Deletes teams with their memberships, websites, links, pixels and boards
/// (and those entities' share links). Returns how many teams were deleted.
///
/// Upstream's UI warns that deleting a team deletes its websites, but upstream
/// keeps them, without a team or owner; umami-rs deletes them.
pub(super) async fn delete_teams(tx: &mut PgConnection, ids: &[Uuid]) -> sqlx::Result<u64> {
  let websites: Vec<Uuid> = sqlx::query_scalar("select website_id from website where team_id = any($1)")
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
  website::purge(tx, &websites).await?;

  delete_owned_entities(tx, "team_id = any($1)", ids).await?;
  delete_where(tx, "team_user", "team_id", ids).await?;

  let deleted = sqlx::query("delete from team where team_id = any($1)")
    .bind(ids)
    .execute(&mut *tx)
    .await?;

  Ok(deleted.rows_affected())
}

/// Deletes the links, pixels and boards matching `condition` (on `$1`), with
/// their share links.
pub(super) async fn delete_owned_entities(tx: &mut PgConnection, condition: &str, ids: &[Uuid]) -> sqlx::Result<()> {
  sqlx::query(sqlx::AssertSqlSafe(format!(
    "delete from share where entity_id in ( \
           select link_id from link where {condition} \
           union all select pixel_id from pixel where {condition} \
           union all select board_id from board where {condition})"
  )))
  .bind(ids)
  .execute(&mut *tx)
  .await?;

  for table in ["link", "pixel", "board"] {
    sqlx::query(sqlx::AssertSqlSafe(format!("delete from {table} where {condition}")))
      .bind(ids)
      .execute(&mut *tx)
      .await?;
  }

  Ok(())
}

async fn insert_member(
  tx: &mut PgConnection,
  team_user_id: Uuid,
  user_id: Uuid,
  team_id: Uuid,
  role: &str,
) -> sqlx::Result<TeamUser> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "insert into team_user (team_user_id, team_id, user_id, role, created_at, updated_at) \
         values ($1, $2, $3, $4, $5, $5) returning {TEAM_USER_COLUMNS}"
  )))
  .bind(team_user_id)
  .bind(team_id)
  .bind(user_id)
  .bind(role)
  .bind(now())
  .fetch_one(tx)
  .await
}

/// Upstream's `createTeamUser`.
pub async fn add_member(
  db: &PgPool,
  team_user_id: Uuid,
  user_id: Uuid,
  team_id: Uuid,
  role: &str,
) -> sqlx::Result<TeamUser> {
  let mut conn = db.acquire().await?;
  insert_member(&mut conn, team_user_id, user_id, team_id, role).await
}

/// Upstream's `updateTeamUser` (the role is the only field it changes).
pub async fn set_member_role(db: &PgPool, team_user_id: Uuid, role: &str) -> sqlx::Result<TeamUser> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "update team_user set role = $2, updated_at = $3 where team_user_id = $1 \
         returning {TEAM_USER_COLUMNS}"
  )))
  .bind(team_user_id)
  .bind(role)
  .bind(now())
  .fetch_one(db)
  .await
}

/// Upstream's `deleteTeamUser`.
pub async fn remove_member(db: &PgPool, team_id: Uuid, user_id: Uuid) -> sqlx::Result<()> {
  sqlx::query("delete from team_user where team_id = $1 and user_id = $2")
    .bind(team_id)
    .bind(user_id)
    .execute(db)
    .await?;

  Ok(())
}
