//! Boards: dashboards of websites, pixels and links (upstream's `board.ts`,
//! with the board helpers of `lib/boards.ts`).

use chrono::{
  DateTime,
  Utc,
};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::js;
use crate::permissions::{
  EntityType,
  Owner,
};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Board {
  pub board_id: Uuid,
  #[sqlx(rename = "type")]
  pub kind: String,
  pub name: String,
  pub description: String,
  pub parameters: Value,
  pub user_id: Option<Uuid>,
  pub team_id: Option<Uuid>,
  pub created_at: Option<DateTime<Utc>>,
  pub updated_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "board_id, type, name, description, parameters, user_id, team_id, created_at, updated_at";

/// The websites, pixels and links a board shows, each in first-seen order
/// without duplicates.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EntityIds {
  pub websites: Vec<String>,
  pub pixels: Vec<String>,
  pub links: Vec<String>,
}

impl EntityIds {
  fn list(&mut self, kind: EntityType) -> Option<&mut Vec<String>> {
    match kind {
      EntityType::Website => Some(&mut self.websites),
      EntityType::Pixel => Some(&mut self.pixels),
      EntityType::Link => Some(&mut self.links),
      EntityType::Board => None,
    }
  }

  /// Adds `id` if it is a non-empty string (upstream checks truthiness, then
  /// collects into a `Set`).
  fn add(&mut self, kind: EntityType, id: &Value) {
    let Some(id) = id.as_str().filter(|id| !id.is_empty()) else {
      return;
    };
    if let Some(list) = self.list(kind)
      && !list.iter().any(|seen| seen == id)
    {
      list.push(id.to_owned());
    }
  }
}

/// A board entity type by its name in board parameters (`BOARD_ENTITY_TYPES`).
fn entity_type(name: &str) -> Option<EntityType> {
  match name {
    "website" => Some(EntityType::Website),
    "pixel" => Some(EntityType::Pixel),
    "link" => Some(EntityType::Link),
    _ => None,
  }
}

impl Board {
  pub fn owner(&self) -> Owner {
    Owner {
      user_id: self.user_id,
      team_id: self.team_id,
    }
  }

  /// Upstream's `getBoardType`: a known type (`open` meaning `mixed`), or one
  /// inferred from the parameters' entity (`getLegacyBoardType`).
  fn board_type(&self) -> &str {
    match self.kind.as_str() {
      "open" => "mixed",
      known @ ("dashboard" | "mixed" | "website" | "pixel" | "link") => known,
      _ if js::truthy(&self.parameters["pixelId"]) => "pixel",
      _ if js::truthy(&self.parameters["linkId"]) => "link",
      _ if js::truthy(&self.parameters["websiteId"]) => "website",
      _ => "mixed",
    }
  }

  /// Upstream's `getBoardEntityIds`: the board's own entity, then any entity
  /// its parameters name, then those of its components.
  pub fn entity_ids(&self) -> EntityIds {
    let parameters = &self.parameters;
    let mut ids = EntityIds::default();

    // `getBoardEntity`
    let board_type = self.board_type();
    if let Some(kind) = entity_type(board_type) {
      ids.add(kind, &parameters[format!("{board_type}Id")]);
    }

    ids.add(EntityType::Website, &parameters["websiteId"]);
    ids.add(EntityType::Pixel, &parameters["pixelId"]);
    ids.add(EntityType::Link, &parameters["linkId"]);

    let rows = parameters["rows"].as_array().into_iter().flatten();
    for column in rows.flat_map(|row| row["columns"].as_array().into_iter().flatten()) {
      // `getComponentEntity`
      let component = &column["component"];
      if js::truthy(&component["entityType"]) && js::truthy(&component["entityId"]) {
        if let Some(kind) = component["entityType"].as_str().and_then(entity_type) {
          ids.add(kind, &component["entityId"]);
        }
      } else if js::truthy(&component["websiteId"]) {
        ids.add(EntityType::Website, &component["websiteId"]);
      }
    }

    ids
  }
}

/// Upstream's `getBoard`.
pub async fn get(db: &PgPool, board_id: Uuid) -> sqlx::Result<Option<Board>> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {COLUMNS} from board where board_id = $1"
  )))
  .bind(board_id)
  .fetch_optional(db)
  .await
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn board(kind: &str, parameters: Value) -> Board {
    Board {
      board_id: Uuid::nil(),
      kind: kind.to_owned(),
      name: String::new(),
      description: String::new(),
      parameters,
      user_id: None,
      team_id: None,
      created_at: None,
      updated_at: None,
    }
  }

  fn ids(websites: &[&str], pixels: &[&str], links: &[&str]) -> EntityIds {
    let owned = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect();
    EntityIds {
      websites: owned(websites),
      pixels: owned(pixels),
      links: owned(links),
    }
  }

  #[test]
  fn collects_entities_like_upstream() {
    let parameters = json!({
        "websiteId": "w1",
        "rows": [
            { "columns": [
                { "component": { "entityType": "pixel", "entityId": "p1" } },
                { "component": { "websiteId": "w2" } },
                { "component": { "entityType": "link", "entityId": "", "websiteId": "w1" } },
                { "component": { "entityType": "other", "entityId": "x" } },
            ] },
            { "columns": [{ "component": { "entityType": "link", "entityId": "l1" } }] },
            { "columns": null },
        ],
    });

    assert_eq!(
      board("mixed", parameters).entity_ids(),
      ids(&["w1", "w2"], &["p1"], &["l1"])
    );
    // An unknown type falls back to the parameters' entity.
    assert_eq!(
      board("x", json!({ "linkId": "l1" })).entity_ids(),
      ids(&[], &[], &["l1"])
    );
    assert_eq!(board("dashboard", json!(null)).entity_ids(), EntityIds::default());
  }
}
