//! Boards: dashboards of websites, pixels and links (upstream's `board.ts`,
//! with the board helpers of `lib/boards.ts`).

use std::collections::HashMap;

use chrono::{
  DateTime,
  Utc,
};
use serde::Serialize;
use serde_json::{
  Map,
  Value,
};
use sqlx::{
  PgPool,
  QueryBuilder,
};
use uuid::Uuid;

use super::{
  Filters,
  model_json,
  now,
  share,
  sort_column,
};
use crate::js;
use crate::permissions::{
  EntityType,
  Owner,
};

/// Upstream's `BOARD_SORT_FIELDS`.
const SORT_FIELDS: &[&str] = &["name", "description", "type", "createdAt"];

/// Upstream's `BOARD_TYPES.dashboard`: the type of a user's own dashboard,
/// whose ID is the user's.
pub const DASHBOARD: &str = "dashboard";

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Board {
  #[serde(rename = "id")]
  pub board_id: Uuid,
  #[sqlx(rename = "type")]
  #[serde(rename = "type")]
  pub kind: String,
  pub name: String,
  pub description: String,
  pub parameters: Value,
  pub user_id: Option<Uuid>,
  pub team_id: Option<Uuid>,
  #[serde(serialize_with = "js::serialize_date")]
  pub created_at: Option<DateTime<Utc>>,
  #[serde(serialize_with = "js::serialize_date")]
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

/// Upstream's `normalizeBoardType`: a known type, with `open` meaning `mixed`.
pub fn normalize_type(kind: &str) -> Option<&'static str> {
  Some(match kind {
    "open" | "mixed" => "mixed",
    DASHBOARD => DASHBOARD,
    "website" => "website",
    "pixel" => "pixel",
    "link" => "link",
    _ => return None,
  })
}

/// Upstream's `getBoardType`: a known type, or one inferred from the
/// parameters' entity (`getLegacyBoardType`).
fn board_type(kind: &str, parameters: &Value) -> &'static str {
  match normalize_type(kind) {
    Some(known) => known,
    None if js::truthy(&parameters["pixelId"]) => "pixel",
    None if js::truthy(&parameters["linkId"]) => "link",
    None if js::truthy(&parameters["websiteId"]) => "website",
    None => "mixed",
  }
}

/// Upstream's `getBoardEntity`: the entity a website, pixel or link board is
/// about, and its ID.
fn board_entity<'a>(kind: &str, parameters: &'a Value) -> Option<(EntityType, &'a Value)> {
  let board_type = board_type(kind, parameters);
  entity_type(board_type).map(|kind| (kind, &parameters[format!("{board_type}Id")]))
}

/// Upstream's `getComponentEntity`: the entity a component names, if any (an
/// unknown entity type is none, with no fallback to `websiteId`).
fn component_entity(component: &Value) -> Option<(Option<EntityType>, &Value)> {
  if js::truthy(&component["entityType"]) && js::truthy(&component["entityId"]) {
    let kind = component["entityType"].as_str().and_then(entity_type);
    return Some((kind, &component["entityId"]));
  }
  if js::truthy(&component["websiteId"]) {
    return Some((Some(EntityType::Website), &component["websiteId"]));
  }
  None
}

/// The components of a board's layout (`parameters.rows[].columns[].component`).
fn components(parameters: &Value) -> impl Iterator<Item = &Value> {
  let rows = parameters["rows"].as_array().into_iter().flatten();
  rows
    .flat_map(|row| row["columns"].as_array().into_iter().flatten())
    .map(|column| &column["component"])
}

/// Whether a board's layout has the shape upstream iterates over: `rows` and
/// each row's `columns`, if present, are lists of objects. Upstream fails
/// with a 500 on other shapes.
pub fn valid_layout(parameters: &Value) -> bool {
  let list_of_objects = |value: &Value| {
    match value {
      Value::Null => true,
      Value::Array(items) => items.iter().all(Value::is_object),
      _ => false,
    }
  };

  list_of_objects(&parameters["rows"])
    && parameters["rows"]
      .as_array()
      .into_iter()
      .flatten()
      .all(|row| list_of_objects(&row["columns"]))
}

/// Upstream's `getBoardEntityIds`: the board's own entity, then any entity its
/// parameters name, then those of its components.
pub fn entity_ids(kind: &str, parameters: &Value) -> EntityIds {
  let mut ids = EntityIds::default();

  if let Some((kind, id)) = board_entity(kind, parameters) {
    ids.add(kind, id);
  }

  ids.add(EntityType::Website, &parameters["websiteId"]);
  ids.add(EntityType::Pixel, &parameters["pixelId"]);
  ids.add(EntityType::Link, &parameters["linkId"]);

  for component in components(parameters) {
    if let Some((Some(kind), id)) = component_entity(component) {
      ids.add(kind, id);
    }
  }

  ids
}

impl Board {
  pub fn owner(&self) -> Owner {
    Owner {
      user_id: self.user_id,
      team_id: self.team_id,
    }
  }

  pub fn entity_ids(&self) -> EntityIds {
    entity_ids(&self.kind, &self.parameters)
  }

  /// The Prisma model as JSON, fields in schema order.
  pub fn to_json(&self) -> Map<String, Value> {
    model_json(self)
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

/// Whose boards a list covers.
#[derive(Debug, Clone, Copy)]
pub enum Scope {
  /// The user's (upstream's `getUserBoards`).
  User(Uuid),
  /// The team's (upstream's `getTeamBoards`).
  Team(Uuid),
}

/// Upstream's `getBoards`: boards other than users' dashboards, searched by
/// name and description, paged.
pub async fn list(db: &PgPool, scope: Scope, filters: Filters) -> sqlx::Result<Value> {
  let filters = filters.sanitize(SORT_FIELDS, None, None);
  let search = filters.search_pattern();

  let push_where = |builder: &mut QueryBuilder<sqlx::Postgres>| {
    builder.push(" where type <> ").push_bind(DASHBOARD);
    match scope {
      Scope::User(user_id) => builder.push(" and user_id = ").push_bind(user_id),
      Scope::Team(team_id) => builder.push(" and team_id = ").push_bind(team_id),
    };
    if let Some(search) = &search {
      builder
        .push(" and (name ilike ")
        .push_bind(search.clone())
        .push(" or description ilike ")
        .push_bind(search.clone())
        .push(")");
    }
  };

  let mut query = QueryBuilder::new(format!("select {COLUMNS} from board"));
  push_where(&mut query);
  // Upstream leaves the order to the database unless asked; this sorts by name.
  filters.push_order(&mut query, sort_column, "name asc", "board_id");
  filters.push_limit(&mut query);
  let boards: Vec<Board> = query.build_query_as().fetch_all(db).await?;

  let mut count = QueryBuilder::new("select count(*) from board");
  push_where(&mut count);
  let count: i64 = count.build_query_scalar().fetch_one(db).await?;

  let data = boards.iter().map(|board| Value::Object(board.to_json())).collect();

  Ok(filters.page(data, count))
}

/// A new board (upstream's `createBoard`).
pub struct NewBoard<'a> {
  pub id: Uuid,
  pub kind: &'a str,
  pub name: &'a str,
  pub description: &'a str,
  pub parameters: &'a Value,
  pub user_id: Option<Uuid>,
  pub team_id: Option<Uuid>,
}

pub async fn create(db: &PgPool, board: &NewBoard<'_>) -> sqlx::Result<Board> {
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "insert into board (board_id, type, name, description, parameters, user_id, team_id, \
     created_at, updated_at) values ($1, $2, $3, $4, $5, $6, $7, $8, $8) returning {COLUMNS}"
  )))
  .bind(board.id)
  .bind(board.kind)
  .bind(board.name)
  .bind(board.description)
  .bind(board.parameters)
  .bind(board.user_id)
  .bind(board.team_id)
  .bind(now())
  .fetch_one(db)
  .await
}

/// Changes to a board (upstream's `updateBoard`); `None` fields are kept.
#[derive(Default)]
pub struct BoardUpdate<'a> {
  pub kind: Option<&'a str>,
  pub name: Option<&'a str>,
  pub description: Option<&'a str>,
  pub parameters: Option<&'a Value>,
}

/// Applies `changes`; `None` if the board does not exist. Like Prisma, an
/// update without changes leaves `updated_at` alone.
pub async fn update(db: &PgPool, board_id: Uuid, changes: &BoardUpdate<'_>) -> sqlx::Result<Option<Board>> {
  let BoardUpdate {
    kind,
    name,
    description,
    parameters,
  } = changes;
  if kind.is_none() && name.is_none() && description.is_none() && parameters.is_none() {
    return get(db, board_id).await;
  }

  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "update board set type = coalesce($2, type), name = coalesce($3, name), \
     description = coalesce($4, description), parameters = coalesce($5, parameters), \
     updated_at = $6 where board_id = $1 returning {COLUMNS}"
  )))
  .bind(board_id)
  .bind(kind)
  .bind(name)
  .bind(description)
  .bind(parameters)
  .bind(now())
  .fetch_optional(db)
  .await
}

/// Upstream's `deleteBoard`, which also deletes the board's share links
/// (upstream keeps them). `false` if the board does not exist.
pub async fn delete(db: &PgPool, board_id: Uuid) -> sqlx::Result<bool> {
  let mut tx = db.begin().await?;
  let deleted = sqlx::query("delete from board where board_id = $1")
    .bind(board_id)
    .execute(&mut *tx)
    .await?;
  if deleted.rows_affected() == 0 {
    return Ok(false);
  }
  share::delete_for_entities(&mut *tx, &[board_id]).await?;
  tx.commit().await?;

  Ok(true)
}

/// Upstream's `BOARD_COMPONENT_REPORT_TYPES`: the report type a component
/// shows, if it shows one.
fn component_report_type(component: &Value) -> Option<&'static str> {
  match component["type"].as_str()? {
    "Funnel" => Some("funnel"),
    "Goal" => Some("goal"),
    _ => None,
  }
}

/// Saved reports by ID, as type and website (`None` if missing), looked up
/// once each.
type ReportCache = HashMap<String, Option<(String, Uuid)>>;

/// Upstream's `isValidBoardReportReference`: a funnel or goal component's
/// `reportId`, if set, names a report of that type on the component's
/// website. Upstream fails with a 500 on an ID that is not a UUID; here such a
/// reference is invalid.
async fn valid_report_reference(
  db: &PgPool,
  (kind, parameters): (&str, &Value),
  component: &Value,
  cache: &mut ReportCache,
) -> sqlx::Result<bool> {
  let Some(expected) = component_report_type(component) else {
    return Ok(true);
  };
  let report_id = match &component["props"]["reportId"] {
    Value::Null => return Ok(true),
    Value::String(id) if id.is_empty() => return Ok(true),
    Value::String(id) => id,
    _ => return Ok(false),
  };

  // `getResolvedComponentEntity`
  let entity = match board_entity(kind, parameters) {
    Some((kind, id)) if js::truthy(id) => Some((Some(kind), id)),
    _ => component_entity(component),
  };
  let Some((Some(EntityType::Website), Value::String(website_id))) = entity else {
    return Ok(false);
  };

  if !cache.contains_key(report_id) {
    let report = match report_id.parse::<Uuid>() {
      Ok(id) => {
        sqlx::query_as("select type, website_id from report where report_id = $1")
          .bind(id)
          .fetch_optional(db)
          .await?
      }
      Err(_) => None,
    };
    cache.insert(report_id.clone(), report);
  }

  Ok(
    cache[report_id]
      .as_ref()
      .is_some_and(|(kind, website)| kind == expected && website.to_string() == *website_id),
  )
}

/// Upstream's `hasValidBoardReports`: whether every funnel and goal component
/// references a valid report (see [`valid_report_reference`]).
pub async fn valid_reports(db: &PgPool, kind: &str, parameters: &Value) -> sqlx::Result<bool> {
  let mut cache = ReportCache::new();

  for component in components(parameters).filter(|c| js::truthy(c)) {
    if !valid_report_reference(db, (kind, parameters), component, &mut cache).await? {
      return Ok(false);
    }
  }

  Ok(true)
}

/// Upstream's `stripInvalidBoardReports`: `parameters` without the invalid
/// report references (and without `props` left empty by removing one).
pub async fn strip_invalid_reports(db: &PgPool, kind: &str, parameters: &Value) -> sqlx::Result<Value> {
  let mut cache = ReportCache::new();
  let mut invalid = Vec::new();

  for (r, row) in parameters["rows"].as_array().into_iter().flatten().enumerate() {
    for (c, column) in row["columns"].as_array().into_iter().flatten().enumerate() {
      let component = &column["component"];
      if js::truthy(component) && !valid_report_reference(db, (kind, parameters), component, &mut cache).await? {
        invalid.push((r, c));
      }
    }
  }

  let mut stripped = parameters.clone();
  for (r, c) in invalid {
    let component = &mut stripped["rows"][r]["columns"][c]["component"];
    let Some(component) = component.as_object_mut() else {
      continue;
    };
    let empty = match component.get_mut("props").and_then(Value::as_object_mut) {
      Some(props) => {
        props.shift_remove("reportId");
        props.is_empty()
      }
      None => false,
    };
    if empty {
      component.shift_remove("props");
    }
  }

  Ok(stripped)
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

  #[test]
  fn normalizes_types_like_upstream() {
    assert_eq!(normalize_type("open"), Some("mixed"));
    assert_eq!(normalize_type("dashboard"), Some("dashboard"));
    assert_eq!(normalize_type("Website"), None);
    // Unknown types are inferred from the parameters.
    assert_eq!(
      board_type("legacy", &json!({ "pixelId": "p", "websiteId": "w" })),
      "pixel"
    );
    assert_eq!(board_type("legacy", &json!({})), "mixed");
  }

  #[test]
  fn checks_layout_shapes() {
    for valid in [
      json!({}),
      json!({ "rows": null }),
      json!({ "rows": [{}, { "columns": [{ "component": null }, {}] }] }),
    ] {
      assert!(valid_layout(&valid), "{valid}");
    }
    for invalid in [
      json!({ "rows": {} }),
      json!({ "rows": [null] }),
      json!({ "rows": [{ "columns": "x" }] }),
      json!({ "rows": [{ "columns": [1] }] }),
    ] {
      assert!(!valid_layout(&invalid), "{invalid}");
    }
  }

  #[test]
  fn resolves_component_entities_like_upstream() {
    // An unknown entity type names nothing, without falling back to `websiteId`.
    let unknown = json!({ "entityType": "other", "entityId": "x", "websiteId": "w" });
    assert!(matches!(component_entity(&unknown), Some((None, _))));
    let website = json!({ "entityType": "", "websiteId": "w" });
    assert_eq!(
      component_entity(&website).map(|(kind, id)| (kind, id.clone())),
      Some((Some(EntityType::Website), json!("w")))
    );
    assert!(component_entity(&json!({})).is_none());
  }
}
