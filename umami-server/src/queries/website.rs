//! Websites and their share links (upstream's `website.ts` and `share.ts`).

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

use super::team::{
  ROLE_MANAGER,
  ROLE_OWNER,
  Team,
  TeamUser,
  usernames,
};
use super::{
  Filters,
  into_map,
  now,
  sort_column,
};
use crate::{
  json as js,
  zod,
};

/// Upstream's `WEBSITE_SORT_FIELDS`.
pub const SORT_FIELDS: &[&str] = &["name", "domain", "createdAt"];

/// Upstream's `ENTITY_TYPE.website`, the `share_type` of website shares.
const SHARE_TYPE_WEBSITE: i32 = 1;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Website {
  pub website_id: Uuid,
  pub name: String,
  pub domain: Option<String>,
  pub reset_at: Option<DateTime<Utc>>,
  pub user_id: Option<Uuid>,
  pub team_id: Option<Uuid>,
  pub created_by: Option<Uuid>,
  pub created_at: Option<DateTime<Utc>>,
  pub updated_at: Option<DateTime<Utc>>,
  pub deleted_at: Option<DateTime<Utc>>,
  pub recorder_enabled: bool,
  pub replay_config: Option<Value>,
}

const WEBSITE_COLUMNS: &str = "website_id, name, domain, reset_at, user_id, team_id, created_by, \
                               created_at, updated_at, deleted_at, recorder_enabled, replay_config";

impl Website {
  /// The Prisma model as JSON, fields in schema order.
  pub fn to_json(&self) -> Map<String, Value> {
    into_map(json!({
        "id": self.website_id,
        "name": self.name,
        "domain": self.domain,
        "resetAt": self.reset_at.map(js::date),
        "userId": self.user_id,
        "teamId": self.team_id,
        "createdBy": self.created_by,
        "createdAt": self.created_at.map(js::date),
        "updatedAt": self.updated_at.map(js::date),
        "deletedAt": self.deleted_at.map(js::date),
        "recorderEnabled": self.recorder_enabled,
        "replayConfig": self.replay_config,
    }))
  }
}

/// Upstream's `findWebsite`, deleted or not.
pub async fn find(db: &PgPool, website_id: Uuid) -> sqlx::Result<Option<Website>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {WEBSITE_COLUMNS} from website where website_id = $1"
  )))
  .bind(website_id)
  .fetch_optional(db)
  .await
}

/// Upstream's `getWebsite` without the share ID: `None` for IDs that are not
/// UUIDs, rather than a database error.
pub async fn get(db: &PgPool, website_id: &str) -> sqlx::Result<Option<Website>> {
  match zod::uuid(website_id) {
    Some(id) => find(db, id).await,
    None => Ok(None),
  }
}

/// The website as JSON with `shareId` attached (upstream's `getWebsite`).
pub async fn with_share_id(db: &PgPool, website: &Website) -> sqlx::Result<Value> {
  let mut object = website.to_json();
  let share_id = latest_share(db, website.website_id).await?;
  object.insert("shareId".to_owned(), json!(share_id));

  Ok(Value::Object(object))
}

/// The slug of the entity's newest share (upstream's `getShareByEntityId`).
pub async fn latest_share(db: &PgPool, entity_id: Uuid) -> sqlx::Result<Option<String>> {
  sqlx::query_scalar("select slug from share where entity_id = $1 order by created_at desc limit 1")
    .bind(entity_id)
    .fetch_optional(db)
    .await
}

/// Which websites a list covers, and what each list includes.
pub enum Scope {
  /// The user's own websites, with `user` (upstream's `getUserWebsites`).
  Owned(Uuid),
  /// The user's own websites and those of teams they own or manage
  /// (upstream's `getAllUserWebsitesIncludingTeamAccess`).
  WithTeamAccess(Uuid),
  /// A team's websites, with `createUser` (upstream's `getTeamWebsites`).
  Team(Uuid),
  /// Every website, with `user` and `team` (the admin list).
  All,
}

/// Upstream's `getWebsites`: non-deleted websites, paged, with `shareId`.
pub async fn list(db: &PgPool, scope: Scope, filters: Filters) -> sqlx::Result<Value> {
  let filters = match scope {
    Scope::Owned(_) | Scope::WithTeamAccess(_) => filters.sanitize(SORT_FIELDS, Some("name"), None),
    Scope::Team(_) | Scope::All => filters.sanitize(SORT_FIELDS, None, None),
  };
  let search = filters.search_pattern();

  let push_where = |builder: &mut QueryBuilder<sqlx::Postgres>| {
    builder.push(" where w.deleted_at is null");
    match scope {
      Scope::Owned(user_id) => {
        builder.push(" and w.user_id = ").push_bind(user_id);
      }
      Scope::WithTeamAccess(user_id) => {
        builder
          .push(" and (w.user_id = ")
          .push_bind(user_id)
          .push(
            " or exists (select 1 from team t where t.team_id = w.team_id \
                         and t.deleted_at is null and exists (select 1 from team_user tu \
                         where tu.team_id = t.team_id and tu.role in (",
          )
          .push_bind(ROLE_OWNER)
          .push(", ")
          .push_bind(ROLE_MANAGER)
          .push(") and tu.user_id = ")
          .push_bind(user_id)
          .push(")))");
      }
      Scope::Team(team_id) => {
        builder.push(" and w.team_id = ").push_bind(team_id);
      }
      Scope::All => {}
    }
    if let Some(search) = &search {
      builder
        .push(" and (w.name ilike ")
        .push_bind(search.clone())
        .push(" or w.domain ilike ")
        .push_bind(search.clone())
        .push(")");
    }
  };

  let mut query = QueryBuilder::new(format!("select {WEBSITE_COLUMNS} from website w"));
  push_where(&mut query);
  // Upstream leaves a team's websites unordered; every list here defaults
  // to name order, except the admin list (newest first).
  let default_order = match scope {
    Scope::All => "w.created_at desc",
    _ => "w.name asc",
  };
  filters.push_order(&mut query, sort_column, default_order, "w.website_id");
  filters.push_limit(&mut query);
  let websites: Vec<Website> = query.build_query_as().fetch_all(db).await?;

  let mut count = QueryBuilder::new("select count(*) from website w");
  push_where(&mut count);
  let count: i64 = count.build_query_scalar().fetch_one(db).await?;

  let mut data: Vec<Map<String, Value>> = websites.iter().map(Website::to_json).collect();

  match scope {
    Scope::Owned(_) => include_user(db, &websites, &mut data, "user", |w| w.user_id).await?,
    Scope::Team(_) => {
      include_user(db, &websites, &mut data, "createUser", |w| w.created_by).await?;
    }
    Scope::All => include_owners(db, &websites, &mut data).await?,
    Scope::WithTeamAccess(_) => {}
  }

  let ids: Vec<Uuid> = websites.iter().map(|w| w.website_id).collect();
  let shares: Vec<(Uuid, String)> = sqlx::query_as(
    "select distinct on (entity_id) entity_id, slug from share \
         where entity_id = any($1) order by entity_id, created_at desc",
  )
  .bind(&ids)
  .fetch_all(db)
  .await?;

  let data = data
    .into_iter()
    .zip(&websites)
    .map(|(mut object, website)| {
      let share_id = shares
        .iter()
        .find(|(id, _)| *id == website.website_id)
        .map(|(_, slug)| slug);
      object.insert("shareId".to_owned(), json!(share_id));
      Value::Object(object)
    })
    .collect();

  Ok(filters.page(data, count))
}

/// Adds `{ id, username }` of the user that `user_of` names, deleted or not.
async fn include_user(
  db: &PgPool,
  websites: &[Website],
  data: &mut [Map<String, Value>],
  field: &str,
  user_of: fn(&Website) -> Option<Uuid>,
) -> sqlx::Result<()> {
  let ids: Vec<Uuid> = websites.iter().filter_map(user_of).collect();
  let users = usernames(db, &ids).await?;

  for (object, website) in data.iter_mut().zip(websites) {
    let user = user_of(website)
      .and_then(|id| users.iter().find(|(user_id, _)| *user_id == id))
      .map_or(Value::Null, |(id, username)| json!({ "id": id, "username": username }));
    object.insert(field.to_owned(), user);
  }

  Ok(())
}

/// The admin list's includes: the owning user and team (each only if not
/// deleted), the team with its owners as `members`.
async fn include_owners(db: &PgPool, websites: &[Website], data: &mut [Map<String, Value>]) -> sqlx::Result<()> {
  let user_ids: Vec<Uuid> = websites.iter().filter_map(|w| w.user_id).collect();
  let users: Vec<(Uuid, String)> =
    sqlx::query_as(r#"select user_id, username from "user" where user_id = any($1) and deleted_at is null"#)
      .bind(&user_ids)
      .fetch_all(db)
      .await?;

  let team_ids: Vec<Uuid> = websites.iter().filter_map(|w| w.team_id).collect();
  let teams: Vec<Team> = sqlx::query_as(
    "select team_id, name, access_code, logo_url, two_factor_required, created_at, \
         updated_at, deleted_at from team where team_id = any($1) and deleted_at is null",
  )
  .bind(&team_ids)
  .fetch_all(db)
  .await?;
  let owners: Vec<TeamUser> = sqlx::query_as(
    "select team_user_id, team_id, user_id, role, created_at, updated_at from team_user \
         where team_id = any($1) and role = $2 order by created_at, team_user_id",
  )
  .bind(&team_ids)
  .bind(ROLE_OWNER)
  .fetch_all(db)
  .await?;

  for (object, website) in data.iter_mut().zip(websites) {
    let user = website
      .user_id
      .and_then(|id| users.iter().find(|(user_id, _)| *user_id == id))
      .map_or(Value::Null, |(id, username)| json!({ "id": id, "username": username }));
    let team = website
      .team_id
      .and_then(|id| teams.iter().find(|team| team.team_id == id))
      .map_or(Value::Null, |team| {
        let mut object = team.to_json();
        let members: Vec<Value> = owners
          .iter()
          .filter(|owner| owner.team_id == team.team_id)
          .map(|owner| Value::Object(owner.to_json()))
          .collect();
        object.insert("members".to_owned(), json!(members));
        Value::Object(object)
      });

    object.insert("user".to_owned(), user);
    object.insert("team".to_owned(), team);
  }

  Ok(())
}

/// A new website (upstream's `createWebsite`).
pub struct NewWebsite<'a> {
  pub id: Uuid,
  pub name: &'a str,
  pub domain: &'a str,
  pub user_id: Option<Uuid>,
  pub team_id: Option<Uuid>,
  pub created_by: Uuid,
}

pub async fn create(tx: &mut PgConnection, website: &NewWebsite<'_>) -> sqlx::Result<Website> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "insert into website (website_id, name, domain, user_id, team_id, created_by, \
         created_at, updated_at) values ($1, $2, $3, $4, $5, $6, $7, $7) \
         returning {WEBSITE_COLUMNS}"
  )))
  .bind(website.id)
  .bind(website.name)
  .bind(website.domain)
  .bind(website.user_id)
  .bind(website.team_id)
  .bind(website.created_by)
  .bind(now())
  .fetch_one(tx)
  .await
}

/// Changes to a website (upstream's `updateWebsite`); `None` fields are kept.
#[derive(Default)]
pub struct WebsiteUpdate<'a> {
  pub name: Option<&'a str>,
  pub domain: Option<&'a str>,
  /// The recorder config and whether it enables recording.
  pub replay_config: Option<(Value, bool)>,
}

/// Applies `changes`; `None` if the website does not exist.
pub async fn update(
  tx: &mut PgConnection,
  website_id: Uuid,
  changes: &WebsiteUpdate<'_>,
) -> sqlx::Result<Option<Website>> {
  let (replay_config, recorder_enabled) = changes.replay_config.clone().unzip();

  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "update website set name = coalesce($2, name), domain = coalesce($3, domain), \
         replay_config = case when $4 then $5 else replay_config end, \
         recorder_enabled = coalesce($6, recorder_enabled), updated_at = $7 \
         where website_id = $1 returning {WEBSITE_COLUMNS}"
  )))
  .bind(website_id)
  .bind(changes.name)
  .bind(changes.domain)
  .bind(replay_config.is_some())
  .bind(replay_config)
  .bind(recorder_enabled)
  .bind(now())
  .fetch_optional(tx)
  .await
}

/// Moves a website to a user or a team (upstream's transfer route); `None` if
/// the website does not exist.
pub async fn transfer(
  db: &PgPool,
  website_id: Uuid,
  user_id: Option<Uuid>,
  team_id: Option<Uuid>,
) -> sqlx::Result<Option<Website>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "update website set user_id = $2, team_id = $3, updated_at = $4 \
         where website_id = $1 returning {WEBSITE_COLUMNS}"
  )))
  .bind(website_id)
  .bind(user_id)
  .bind(team_id)
  .bind(now())
  .fetch_optional(db)
  .await
}

/// A website share with upstream's defaults (`createShare` in the website routes).
/// Fails on a duplicate slug.
pub async fn create_share(
  tx: &mut PgConnection,
  share_id: Uuid,
  website: &Website,
  slug: &str,
) -> sqlx::Result<String> {
  let at = now();

  sqlx::query_scalar(
    "insert into share (share_id, entity_id, name, share_type, slug, parameters, \
         created_at, updated_at) values ($1, $2, $3, $4, $5, $6, $7, $7) returning slug",
  )
  .bind(share_id)
  .bind(website.website_id)
  .bind(&website.name)
  .bind(SHARE_TYPE_WEBSITE)
  .bind(slug)
  .bind(json!({ "overview": true, "events": true }))
  .bind(at)
  .fetch_one(tx)
  .await
}

/// Upstream's `deleteSharesByEntityId`.
pub async fn delete_shares(tx: &mut PgConnection, entity_id: Uuid) -> sqlx::Result<()> {
  sqlx::query("delete from share where entity_id = $1")
    .bind(entity_id)
    .execute(tx)
    .await?;

  Ok(())
}

/// Upstream's `deleteWebsiteDependentData`: the collected data of `ids`.
async fn delete_collected_data(tx: &mut PgConnection, ids: &[Uuid]) -> sqlx::Result<()> {
  for table in [
    "session_replay_saved",
    "session_replay",
    "heatmap_event",
    "revenue",
    "event_data",
  ] {
    delete_by_website(tx, table, ids).await?;
  }

  // Event data whose own website_id drifted from its event's.
  sqlx::query(
    "delete from event_data using website_event \
         where event_data.website_event_id = website_event.event_id \
         and website_event.website_id = any($1)",
  )
  .bind(ids)
  .execute(&mut *tx)
  .await?;

  for table in ["session_data", "session_link", "website_event", "session"] {
    delete_by_website(tx, table, ids).await?;
  }

  Ok(())
}

async fn delete_by_website(tx: &mut PgConnection, table: &str, ids: &[Uuid]) -> sqlx::Result<()> {
  sqlx::query(sqlx::AssertSqlSafe(format!(
    "delete from {table} where website_id = any($1)"
  )))
  .bind(ids)
  .execute(&mut *tx)
  .await?;

  Ok(())
}

/// Upstream's `resetWebsite`: deletes the collected data and records when.
/// `false` if the website does not exist.
pub async fn reset(db: &PgPool, website_id: Uuid) -> sqlx::Result<bool> {
  let mut tx = db.begin().await?;
  delete_collected_data(&mut tx, &[website_id]).await?;

  let at = now();
  let updated = sqlx::query("update website set reset_at = $2, updated_at = $2 where website_id = $1")
    .bind(website_id)
    .bind(at)
    .execute(&mut *tx)
    .await?;
  if updated.rows_affected() == 0 {
    return Ok(false);
  }
  tx.commit().await?;

  Ok(true)
}

/// Upstream's `deleteWebsite` (self-hosted mode). `false` if the website does
/// not exist.
pub async fn delete(db: &PgPool, website_id: Uuid) -> sqlx::Result<bool> {
  let mut tx = db.begin().await?;
  if purge(&mut tx, &[website_id]).await? == 0 {
    return Ok(false);
  }
  tx.commit().await?;

  Ok(true)
}

/// Deletes websites with everything that belongs to them: collected data,
/// reports, segments, annotations and share links. Returns how many websites
/// were deleted.
///
/// Upstream deletes all of this for a single website, but when deleting a
/// user it skips most of it (and so fails on revenue and the rest) and leaves
/// share links behind; deleting a team keeps the team's websites without a team.
/// umami-rs purges websites the same way in every case.
pub(super) async fn purge(tx: &mut PgConnection, ids: &[Uuid]) -> sqlx::Result<u64> {
  delete_collected_data(tx, ids).await?;
  for table in ["report", "segment", "annotation"] {
    delete_by_website(tx, table, ids).await?;
  }
  sqlx::query("delete from share where entity_id = any($1)")
    .bind(ids)
    .execute(&mut *tx)
    .await?;

  let deleted = sqlx::query("delete from website where website_id = any($1)")
    .bind(ids)
    .execute(&mut *tx)
    .await?;

  Ok(deleted.rows_affected())
}
