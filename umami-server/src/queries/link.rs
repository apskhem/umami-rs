//! Links and pixels (upstream's `link.ts` and `pixel.ts`): short links that
//! redirect to a URL, and tracking images. The two models are the same but for
//! a link's target `url`.

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
  website,
};
use crate::js;
use crate::permissions::{
  EntityType,
  Owner,
};

/// Which of the two models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
  Link,
  Pixel,
}

impl Kind {
  pub fn entity_type(self) -> EntityType {
    match self {
      Self::Link => EntityType::Link,
      Self::Pixel => EntityType::Pixel,
    }
  }

  fn table(self) -> &'static str {
    self.entity_type().table()
  }

  /// Upstream's `LINK_SORT_FIELDS` and `PIXEL_SORT_FIELDS`.
  fn sort_fields(self) -> &'static [&'static str] {
    match self {
      Self::Link => &["name", "slug", "url", "createdAt"],
      Self::Pixel => &["name", "slug", "createdAt"],
    }
  }

  /// The columns of [`Link`]; pixels have no `url`.
  fn columns(self) -> &'static str {
    match self {
      Self::Link => "link_id as id, name, url, slug, user_id, team_id, created_at, updated_at, deleted_at",
      Self::Pixel => {
        "pixel_id as id, name, null::varchar as url, slug, user_id, team_id, created_at, updated_at, \
         deleted_at"
      }
    }
  }

  /// The 400 for a taken ID (upstream fails with a 500).
  pub fn id_taken(self) -> &'static str {
    match self {
      Self::Link => "That link ID is already taken.",
      Self::Pixel => "That pixel ID is already taken.",
    }
  }
}

/// Upstream's 400 for a taken slug.
pub const SLUG_TAKEN: &str = "That slug is already taken.";

/// A link, or a pixel (which has no `url`).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
  pub id: Uuid,
  pub name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub url: Option<String>,
  pub slug: String,
  pub user_id: Option<Uuid>,
  pub team_id: Option<Uuid>,
  #[serde(serialize_with = "js::serialize_date")]
  pub created_at: Option<DateTime<Utc>>,
  #[serde(serialize_with = "js::serialize_date")]
  pub updated_at: Option<DateTime<Utc>>,
  #[serde(serialize_with = "js::serialize_date")]
  pub deleted_at: Option<DateTime<Utc>>,
}

impl Link {
  pub fn owner(&self) -> Owner {
    Owner {
      user_id: self.user_id,
      team_id: self.team_id,
    }
  }

  /// The Prisma model as JSON, fields in schema order.
  pub fn to_json(&self) -> Map<String, Value> {
    model_json(self)
  }
}

/// Upstream's `getLink` and `getPixel`, deleted or not.
pub async fn find(db: &PgPool, kind: Kind, id: Uuid) -> sqlx::Result<Option<Link>> {
  let (columns, table) = (kind.columns(), kind.table());
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {columns} from {table} where {table}_id = $1"
  )))
  .bind(id)
  .fetch_optional(db)
  .await
}

/// The link or pixel that `/q/{slug}` or `/p/{slug}` serves (upstream's
/// `findLink`/`findPixel` by slug), unless deleted.
pub async fn by_slug(db: &PgPool, kind: Kind, slug: &str) -> sqlx::Result<Option<Link>> {
  let (columns, table) = (kind.columns(), kind.table());
  sqlx::query_as(sqlx::AssertSqlSafe(format!(
    "select {columns} from {table} where slug = $1 and deleted_at is null"
  )))
  .bind(slug)
  .fetch_optional(db)
  .await
}

/// Whose links or pixels a list covers.
#[derive(Debug, Clone, Copy)]
pub enum Scope {
  /// The user's own (upstream's `getUserLinks`, `getUserPixels`).
  User(Uuid),
  /// The team's (upstream's `getTeamLinks`, `getTeamPixels`).
  Team(Uuid),
}

/// Upstream's `getLinks` and `getPixels`: non-deleted, searched by name, slug
/// (and a link's URL), paged.
pub async fn list(db: &PgPool, kind: Kind, scope: Scope, filters: Filters) -> sqlx::Result<Value> {
  let filters = filters.sanitize(kind.sort_fields(), None, None);
  let search = filters.search_pattern();
  let table = kind.table();

  let push_where = |builder: &mut QueryBuilder<sqlx::Postgres>| {
    builder.push(" where deleted_at is null");
    match scope {
      Scope::User(user_id) => builder.push(" and user_id = ").push_bind(user_id),
      Scope::Team(team_id) => builder.push(" and team_id = ").push_bind(team_id),
    };
    if let Some(search) = &search {
      builder
        .push(" and (name ilike ")
        .push_bind(search.clone())
        .push(" or slug ilike ")
        .push_bind(search.clone());
      if kind == Kind::Link {
        builder.push(" or url ilike ").push_bind(search.clone());
      }
      builder.push(")");
    }
  };

  let mut query = QueryBuilder::new(format!("select {} from {table}", kind.columns()));
  push_where(&mut query);
  // Upstream leaves the order to the database unless asked; this sorts by name.
  filters.push_order(&mut query, sort_column, "name asc", &format!("{table}_id"));
  filters.push_limit(&mut query);
  let items: Vec<Link> = query.build_query_as().fetch_all(db).await?;

  let mut count = QueryBuilder::new(format!("select count(*) from {table}"));
  push_where(&mut count);
  let count: i64 = count.build_query_scalar().fetch_one(db).await?;

  let data = items.iter().map(|item| Value::Object(item.to_json())).collect();

  Ok(filters.page(data, count))
}

/// A new link or pixel (upstream's `createLink`, `createPixel`).
pub struct NewLink<'a> {
  pub id: Uuid,
  pub name: &'a str,
  /// The target, for links.
  pub url: Option<&'a str>,
  pub slug: &'a str,
  pub user_id: Option<Uuid>,
  pub team_id: Option<Uuid>,
}

/// Creates a link or pixel. Fails on a taken ID or slug.
pub async fn create(db: &PgPool, kind: Kind, new: &NewLink<'_>) -> sqlx::Result<Link> {
  let (columns, table) = (kind.columns(), kind.table());
  let mut query = QueryBuilder::new(format!(
    "insert into {table} ({table}_id, name, slug, user_id, team_id, "
  ));
  if kind == Kind::Link {
    query.push("url, ");
  }
  query
    .push("created_at, updated_at) values (")
    .push_bind(new.id)
    .push(", ")
    .push_bind(new.name)
    .push(", ")
    .push_bind(new.slug)
    .push(", ")
    .push_bind(new.user_id)
    .push(", ")
    .push_bind(new.team_id)
    .push(", ");
  if kind == Kind::Link {
    query.push_bind(new.url.unwrap_or_default()).push(", ");
  }
  let at = now();
  query
    .push_bind(at)
    .push(", ")
    .push_bind(at)
    .push(format!(") returning {columns}"));

  query.build_query_as().fetch_one(db).await
}

/// Changes to a link or pixel (upstream's `updateLink`, `updatePixel`); `None`
/// fields are kept, as is a pixel's (absent) URL.
#[derive(Default)]
pub struct LinkUpdate<'a> {
  pub name: Option<&'a str>,
  pub url: Option<&'a str>,
  pub slug: Option<&'a str>,
}

/// Applies `changes`; `None` if there is no such link or pixel. Fails on a
/// taken slug.
pub async fn update(db: &PgPool, kind: Kind, id: Uuid, changes: &LinkUpdate<'_>) -> sqlx::Result<Option<Link>> {
  // Like Prisma, an update without changes leaves `updated_at` alone.
  let url = changes.url.filter(|_| kind == Kind::Link);
  if changes.name.is_none() && changes.slug.is_none() && url.is_none() {
    return find(db, kind, id).await;
  }

  let (columns, table) = (kind.columns(), kind.table());
  let mut query = QueryBuilder::new(format!("update {table} set name = coalesce("));
  query
    .push_bind(changes.name)
    .push(", name), slug = coalesce(")
    .push_bind(changes.slug)
    .push(", slug), ");
  if kind == Kind::Link {
    query.push("url = coalesce(").push_bind(url).push(", url), ");
  }
  query
    .push("updated_at = ")
    .push_bind(now())
    .push(format!(" where {table}_id = "))
    .push_bind(id)
    .push(format!(" returning {columns}"));

  query.build_query_as().fetch_optional(db).await
}

/// Upstream's `deleteLink` and `deletePixel`, which also delete the share links
/// and the collected events (upstream keeps both). `false` if there is no such
/// link or pixel.
pub async fn delete(db: &PgPool, kind: Kind, id: Uuid) -> sqlx::Result<bool> {
  let table = kind.table();
  let mut tx = db.begin().await?;

  let deleted = sqlx::query(sqlx::AssertSqlSafe(format!(
    "delete from {table} where {table}_id = $1"
  )))
  .bind(id)
  .execute(&mut *tx)
  .await?;
  if deleted.rows_affected() == 0 {
    return Ok(false);
  }
  share::delete_for_entities(&mut *tx, &[id]).await?;
  website::delete_collected_data(&mut tx, &[id]).await?;
  tx.commit().await?;

  Ok(true)
}
