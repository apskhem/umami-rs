//! Helpers shared by the parity tests: identical records seeded into both
//! databases, share tokens, and masks for values each server generates.

use serde_json::{
  Value,
  json,
};
use umami_server::{
  crypto,
  jwt,
};
use uuid::Uuid;

use crate::{
  APP_SECRET,
  Stack,
  TestUser,
  mask,
};

/// The creation time of seeded records, the same in both databases.
pub const SEEDED_AT: &str = "2026-01-02T03:04:05.678Z";

/// A unique, lowercase tag for names, slugs and search terms.
pub fn tag() -> String {
  Uuid::new_v4().simple().to_string()[..10].to_owned()
}

/// The secret both servers sign tokens with (upstream's `secret()`).
pub fn secret() -> String {
  crypto::hash(&[APP_SECRET])
}

/// A share token with `claims`, as `/api/share/{slug}` issues them.
pub fn share_token(mut claims: Value) -> String {
  claims["type"] = json!("share");
  claims["iat"] = json!(1);
  jwt::sign(&claims, &secret())
}

/// Hides the timestamps each server sets when it writes a record.
pub fn timestamps(value: &mut Value) {
  mask(value, &["createdAt", "updatedAt", "resetAt"]);
}

/// Creates the same team, with the same members, in both databases.
pub async fn seed_team(stack: &Stack, members: &[(&TestUser, &str)]) -> Uuid {
  let id = Uuid::new_v4();
  stack
    .execute(
      &format!("insert into team (team_id, name, created_at) values ($1[1], 'Parity team', '{SEEDED_AT}')"),
      &[id],
    )
    .await;
  for (user, role) in members {
    stack
      .execute(
        &format!("insert into team_user (team_user_id, team_id, user_id, role) values ($1[1], $1[2], $1[3], '{role}')"),
        &[Uuid::new_v4(), id, user.id],
      )
      .await;
  }
  id
}

/// What [`seed_entity`] creates.
#[derive(Debug, Clone, Copy)]
pub enum Entity {
  Website,
  Link,
  Pixel,
  /// A `mixed` board without components.
  Board,
}

/// Creates the same website, link, pixel or board, named `name` and owned by
/// `user` or else `team`, in both databases.
pub async fn seed_entity(stack: &Stack, kind: Entity, name: &str, user: Option<Uuid>, team: Option<Uuid>) -> Uuid {
  let id = Uuid::new_v4();
  let owner = |id: Option<Uuid>| id.map_or_else(|| "null".to_owned(), |id| format!("'{id}'"));
  let (table, columns, values) = match kind {
    Entity::Website => ("website", "domain", "'example.com'"),
    Entity::Link => ("link", "url, slug", "'https://example.com', $1[1]::text"),
    Entity::Pixel => ("pixel", "slug", "$1[1]::text"),
    Entity::Board => ("board", "type, description, parameters", "'mixed', '', '{}'"),
  };
  stack
    .execute(
      &format!(
        "insert into {table} ({table}_id, user_id, team_id, name, created_at, {columns}) \
         values ($1[1], {}, {}, '{name}', '{SEEDED_AT}', {values})",
        owner(user),
        owner(team)
      ),
      &[id],
    )
    .await;
  id
}

/// Creates the same website, named `name` and owned by `user` or else `team`,
/// in both databases.
pub async fn seed_website(stack: &Stack, name: &str, user: Option<Uuid>, team: Option<Uuid>) -> Uuid {
  seed_entity(stack, Entity::Website, name, user, team).await
}

/// Creates the same share link of `entity` in both databases; returns its ID
/// and slug.
pub async fn seed_share(stack: &Stack, entity: Uuid, share_type: i32, parameters: &str) -> (Uuid, String) {
  let id = Uuid::new_v4();
  let slug = format!("parity{}", tag());
  stack
    .execute(
      &format!(
        "insert into share (share_id, entity_id, name, share_type, slug, parameters, created_at) \
         values ($1[1], $1[2], 'Shared', {share_type}, '{slug}', '{parameters}', '{SEEDED_AT}')"
      ),
      &[id, entity],
    )
    .await;
  (id, slug)
}
