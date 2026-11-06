//! Access rules, ported from upstream's `src/permissions` and the role table in
//! `src/lib/constants.ts`.
//!
//! IDs from request paths arrive as strings and are compared as upstream compares
//! them. Where upstream would pass a malformed ID to the database and fail with a
//! 500, these return [`AppError::NotFound`] instead.

use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::Auth;
use crate::error::{
  AppError,
  AppResult,
};
use crate::queries::{
  team,
  website,
};

pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_USER: &str = "user";
pub const ROLE_VIEW_ONLY: &str = "view-only";

pub const WEBSITE_CREATE: &str = "website:create";
pub const WEBSITE_UPDATE: &str = "website:update";
pub const WEBSITE_DELETE: &str = "website:delete";
pub const WEBSITE_TRANSFER_TO_TEAM: &str = "website:transfer-to-team";
pub const WEBSITE_TRANSFER_TO_USER: &str = "website:transfer-to-user";
pub const TEAM_CREATE: &str = "team:create";
pub const TEAM_UPDATE: &str = "team:update";
pub const TEAM_DELETE: &str = "team:delete";

/// Upstream's `ROLE_PERMISSIONS`. Admins (`all`) are handled before any lookup.
fn role_permissions(role: &str) -> &'static [&'static str] {
  match role {
    ROLE_ADMIN => &["all"],
    ROLE_USER => &[WEBSITE_CREATE, WEBSITE_UPDATE, WEBSITE_DELETE, TEAM_CREATE],
    team::ROLE_OWNER => {
      &[
        TEAM_UPDATE,
        TEAM_DELETE,
        WEBSITE_CREATE,
        WEBSITE_UPDATE,
        WEBSITE_DELETE,
        WEBSITE_TRANSFER_TO_TEAM,
        WEBSITE_TRANSFER_TO_USER,
      ]
    }
    team::ROLE_MANAGER => {
      &[
        TEAM_UPDATE,
        WEBSITE_CREATE,
        WEBSITE_UPDATE,
        WEBSITE_DELETE,
        WEBSITE_TRANSFER_TO_TEAM,
      ]
    }
    team::ROLE_MEMBER => &[WEBSITE_CREATE, WEBSITE_UPDATE, WEBSITE_DELETE],
    _ => &[],
  }
}

/// Upstream's `hasPermission`.
pub fn has_permission(role: &str, permission: &str) -> bool {
  role_permissions(role).contains(&permission)
}

/// Upstream's `TEAM_ROLE_RANK`; unknown roles rank lowest (-1).
pub fn team_role_rank(role: Option<&str>) -> i32 {
  match role {
    Some("team-view-only") => 0,
    Some(team::ROLE_MEMBER) => 1,
    Some(team::ROLE_MANAGER) => 2,
    Some(team::ROLE_OWNER) => 3,
    _ => -1,
  }
}

/// An ID as the database would read it, or the error standing in for
/// upstream's database failure on a malformed one.
pub fn db_id(id: &str) -> AppResult<Uuid> {
  id.parse().map_err(|_| AppError::NotFound)
}

fn is_admin(auth: &Auth) -> bool {
  auth.user.as_ref().is_some_and(|user| user.is_admin)
}

fn is_user(auth: &Auth, id: &str) -> bool {
  auth.user.as_ref().is_some_and(|user| user.id.to_string() == id)
}

/// Whether the user's membership of `team_id` grants `permission`.
async fn team_permits(db: &PgPool, team_id: Uuid, user_id: Uuid, permission: &str) -> AppResult<bool> {
  let member = team::team_user(db, team_id, user_id).await?;

  Ok(member.is_some_and(|member| has_permission(&member.role, permission)))
}

// Users

pub fn can_create_user(auth: &Auth) -> bool {
  is_admin(auth)
}

pub fn can_view_users(auth: &Auth) -> bool {
  is_admin(auth)
}

/// Upstream's `canViewUser` and `canUpdateUser`: admins, or the user themself.
pub fn can_view_user(auth: &Auth, user_id: &str) -> bool {
  is_admin(auth) || is_user(auth, user_id)
}

pub fn can_delete_user(auth: &Auth) -> bool {
  is_admin(auth)
}

// Teams

pub async fn can_view_team(db: &PgPool, auth: &Auth, team_id: &str) -> AppResult<bool> {
  let Some(user) = &auth.user else {
    return Ok(false);
  };
  if user.is_admin {
    return Ok(true);
  }

  Ok(team::team_user(db, db_id(team_id)?, user.id).await?.is_some())
}

pub fn can_create_team(auth: &Auth) -> bool {
  auth
    .user
    .as_ref()
    .is_some_and(|user| user.is_admin || has_permission(&user.role, TEAM_CREATE))
}

/// Upstream's `canUpdateTeam` (`team:update`) and `canDeleteTeam` (`team:delete`).
pub async fn can_manage_team(db: &PgPool, auth: &Auth, team_id: &str, permission: &str) -> AppResult<bool> {
  let Some(user) = &auth.user else {
    return Ok(false);
  };
  if user.is_admin {
    return Ok(true);
  }

  team_permits(db, db_id(team_id)?, user.id, permission).await
}

pub async fn can_delete_team_user(db: &PgPool, auth: &Auth, team_id: &str, remove_user_id: &str) -> AppResult<bool> {
  let Some(user) = &auth.user else {
    return Ok(false);
  };
  if user.is_admin || is_user(auth, remove_user_id) {
    return Ok(true);
  }

  team_permits(db, db_id(team_id)?, user.id, TEAM_UPDATE).await
}

pub async fn can_create_team_website(db: &PgPool, auth: &Auth, team_id: &str) -> AppResult<bool> {
  can_manage_team(db, auth, team_id, WEBSITE_CREATE).await
}

pub fn can_view_all_teams(auth: &Auth) -> bool {
  is_admin(auth)
}

// Websites

/// The owner of a website, link, pixel or board (upstream's `getEntity`).
struct Entity {
  user_id: Option<Uuid>,
  team_id: Option<Uuid>,
}

async fn entity(db: &PgPool, entity_id: &str) -> AppResult<Option<Entity>> {
  let id = db_id(entity_id)?;
  // `getWebsite` skips IDs zod rejects; the other lookups query them anyway.
  let check_websites = crate::zod::uuid(entity_id).is_some();

  let owner: Option<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
    "select user_id, team_id from ( \
           select 1 as n, user_id, team_id from website where $2 and website_id = $1 \
           union all select 2, user_id, team_id from link where link_id = $1 \
           union all select 3, user_id, team_id from pixel where pixel_id = $1 \
           union all select 4, user_id, team_id from board where board_id = $1 \
         ) e order by n limit 1",
  )
  .bind(id)
  .bind(check_websites)
  .fetch_optional(db)
  .await?;

  Ok(owner.map(|(user_id, team_id)| Entity { user_id, team_id }))
}

/// Whether the share token covers the website (or link or pixel).
pub fn share_token_includes(auth: &Auth, website_id: &str) -> bool {
  let Some(token) = &auth.share_token else {
    return false;
  };

  let single = ["websiteId", "pixelId", "linkId"]
    .iter()
    .any(|key| token[key].as_str() == Some(website_id));
  let listed = ["websiteIds", "pixelIds", "linkIds"].iter().any(|key| {
    token[key]
      .as_array()
      .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(website_id)))
  });

  single || listed
}

pub async fn can_view_website(db: &PgPool, auth: &Auth, website_id: &str) -> AppResult<bool> {
  if is_admin(auth) || share_token_includes(auth, website_id) {
    return Ok(true);
  }

  let Some(user) = &auth.user else {
    return Ok(false);
  };
  let Some(entity) = entity(db, website_id).await? else {
    return Ok(false);
  };

  if let Some(owner) = entity.user_id {
    return Ok(owner == user.id);
  }
  if let Some(team_id) = entity.team_id {
    return Ok(team::team_user(db, team_id, user.id).await?.is_some());
  }

  Ok(false)
}

/// Upstream's `canViewSharedWebsite`: signed-in users by their own access,
/// share links by the websites they cover.
pub async fn can_view_shared_website(db: &PgPool, auth: &Auth, website_id: &str) -> AppResult<bool> {
  if auth.user.is_some() {
    return can_view_website(db, auth, website_id).await;
  }

  Ok(share_token_includes(auth, website_id))
}

/// Upstream's `SHARE_SECTIONS`: parts of a website a share link may enable.
const SHARE_SECTIONS: &[&str] = &[
  "overview",
  "events",
  "sessions",
  "realtime",
  "performance",
  "compare",
  "breakdown",
  "goals",
  "funnels",
  "journeys",
  "retention",
  "utm",
  "revenue",
  "attribution",
];

/// Upstream's `canViewWebsiteSection`: signed-in users by their own access;
/// share links if they cover the website and enable one of `sections` (links
/// that name no sections enable all of them).
pub async fn can_view_website_section(
  db: &PgPool,
  auth: &Auth,
  website_id: &str,
  sections: &[&str],
) -> AppResult<bool> {
  if auth.user.is_some() {
    return can_view_website(db, auth, website_id).await;
  }
  let Some(token) = &auth.share_token else {
    return Ok(false);
  };
  if !share_token_includes(auth, website_id) {
    return Ok(false);
  }

  let parameters = &token["parameters"];
  let names_sections = SHARE_SECTIONS.iter().any(|section| parameters[section].is_boolean());

  Ok(
    !names_sections
      || sections
        .iter()
        .any(|section| parameters[section] == serde_json::Value::Bool(true)),
  )
}

pub fn can_view_all_websites(auth: &Auth) -> bool {
  is_admin(auth)
}

pub fn can_create_website(auth: &Auth) -> bool {
  auth
    .user
    .as_ref()
    .is_some_and(|user| user.is_admin || has_permission(&user.role, WEBSITE_CREATE))
}

/// Upstream's `canUpdateWebsite` (`website:update`) and `canDeleteWebsite`
/// (`website:delete`): admins, the owner, or team members whose role permits it.
pub async fn can_change_website(db: &PgPool, auth: &Auth, website_id: &str, permission: &str) -> AppResult<bool> {
  let Some(user) = &auth.user else {
    return Ok(false);
  };
  if user.is_admin {
    return Ok(true);
  }
  let Some(website) = website::get(db, website_id).await? else {
    return Ok(false);
  };

  if let Some(owner) = website.user_id {
    return Ok(owner == user.id);
  }
  if let Some(team_id) = website.team_id {
    return team_permits(db, team_id, user.id, permission).await;
  }

  Ok(false)
}

/// Upstream's `canTransferWebsiteToUser`: team owners may take a team website
/// for themselves.
pub async fn can_transfer_website_to_user(
  db: &PgPool,
  auth: &Auth,
  website_id: &str,
  user_id: Uuid,
) -> AppResult<bool> {
  let Some(user) = &auth.user else {
    return Ok(false);
  };
  if user.is_admin {
    return Ok(true);
  }
  let Some(website) = website::get(db, website_id).await? else {
    return Ok(false);
  };

  match website.team_id {
    Some(team_id) if user.id == user_id => team_permits(db, team_id, user_id, WEBSITE_TRANSFER_TO_USER).await,
    _ => Ok(false),
  }
}

/// Upstream's `canTransferWebsiteToTeam`: owners may move their website to a
/// team they own or manage.
pub async fn can_transfer_website_to_team(
  db: &PgPool,
  auth: &Auth,
  website_id: &str,
  team_id: Uuid,
) -> AppResult<bool> {
  let Some(user) = &auth.user else {
    return Ok(false);
  };
  if user.is_admin {
    return Ok(true);
  }
  let Some(website) = website::get(db, website_id).await? else {
    return Ok(false);
  };

  if website.user_id == Some(user.id) {
    return team_permits(db, team_id, user.id, WEBSITE_TRANSFER_TO_TEAM).await;
  }

  Ok(false)
}

#[cfg(test)]
mod tests {
  use serde_json::{
    Value,
    json,
  };

  use super::*;

  fn token(value: Value) -> Auth {
    Auth {
      token: None,
      user: None,
      share_token: Some(value),
      kind: crate::auth::AuthKind::Share,
    }
  }

  #[test]
  fn role_table_matches_upstream() {
    assert!(has_permission(ROLE_USER, WEBSITE_CREATE));
    assert!(has_permission(ROLE_USER, TEAM_CREATE));
    assert!(!has_permission(ROLE_USER, TEAM_UPDATE));
    assert!(!has_permission(ROLE_VIEW_ONLY, WEBSITE_CREATE));
    assert!(has_permission(team::ROLE_OWNER, TEAM_DELETE));
    assert!(has_permission(team::ROLE_OWNER, WEBSITE_TRANSFER_TO_USER));
    assert!(!has_permission(team::ROLE_MANAGER, TEAM_DELETE));
    assert!(!has_permission(team::ROLE_MANAGER, WEBSITE_TRANSFER_TO_USER));
    assert!(has_permission(team::ROLE_MANAGER, WEBSITE_TRANSFER_TO_TEAM));
    assert!(!has_permission(team::ROLE_MEMBER, TEAM_UPDATE));
    assert!(!has_permission("team-view-only", WEBSITE_UPDATE));
    // Admins pass by `isAdmin`, never through the table.
    assert!(!has_permission(ROLE_ADMIN, WEBSITE_CREATE));
  }

  #[test]
  fn ranks_team_roles() {
    assert!(team_role_rank(Some(team::ROLE_OWNER)) > team_role_rank(Some(team::ROLE_MANAGER)));
    assert!(team_role_rank(Some("team-view-only")) > team_role_rank(None));
    assert_eq!(team_role_rank(Some("admin")), -1);
  }

  #[test]
  fn share_tokens_cover_listed_entities() {
    let id = "0d8e3a5c-1f2b-4c3d-9e4f-5a6b7c8d9e0f";

    assert!(share_token_includes(&token(json!({ "websiteId": id })), id));
    assert!(share_token_includes(&token(json!({ "linkIds": ["x", id] })), id));
    assert!(!share_token_includes(&token(json!({ "websiteId": "other" })), id));
    assert!(!share_token_includes(&token(json!({ "websiteIds": id })), id));
  }
}
