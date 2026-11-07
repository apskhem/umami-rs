//! `/api/links` and `/api/pixels` with their `{id}`, `shares` and `charts`
//! routes, and `/api/teams/{teamId}/links` and `/pixels`, ported from upstream's
//! `src/app/api/links` and `src/app/api/pixels`. The two sets of routes differ
//! only in that links have a target URL; each handler serves both, picked by
//! its [`Model`] type parameter.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use chrono::{
  DateTime,
  Utc,
};
use serde_json::{
  Value,
  json,
};
use uuid::Uuid;

use super::params::{
  PAGING_SEARCH_SORTING,
  Params,
};
use super::{
  body,
  ok,
  shares,
};
use crate::analytics::{
  charts,
  date,
};
use crate::auth::Auth;
use crate::collect::event_type;
use crate::error::{
  AppError,
  AppResult,
};
use crate::permissions::{
  self as can,
  WEBSITE_DELETE,
  WEBSITE_UPDATE,
  db_id,
};
use crate::queries::link::{
  self,
  Kind,
  LinkUpdate,
  NewLink,
  Scope,
};
use crate::queries::{
  share,
  team,
};
use crate::state::AppState;
use crate::{
  js,
  zod,
};

/// Which routes a handler serves: [`Links`] or [`Pixels`].
pub trait Model {
  const KIND: Kind;
}

/// `/api/links`.
pub struct Links;

impl Model for Links {
  const KIND: Kind = Kind::Link;
}

/// `/api/pixels`.
pub struct Pixels;

impl Model for Pixels {
  const KIND: Kind = Kind::Pixel;
}

/// A name: `z.string().max(100)`.
fn name(value: &str) -> AppResult<&str> {
  body::max_len(value, 100)
}

/// A link's target: `z.string().max(500)`.
fn url(value: &str) -> AppResult<&str> {
  body::max_len(value, 500)
}

/// A slug: `z.string().min(8).max(100)`.
fn slug(value: &str) -> AppResult<&str> {
  if body::js_len(value) < 8 {
    return Err(AppError::bad_request());
  }
  body::max_len(value, 100)
}

/// A 400 for a taken ID or slug, where upstream fails with a 500 (except for a
/// slug taken by an update).
fn taken(error: sqlx::Error, kind: Kind) -> AppError {
  let slug = matches!(&error, sqlx::Error::Database(e) if e.constraint().is_some_and(|c| c.ends_with("_slug_key")));
  AppError::taken(error, if slug { link::SLUG_TAKEN } else { kind.id_taken() })
}

fn to_json(found: &link::Link) -> Json<Value> {
  Json(Value::Object(found.to_json()))
}

/// `GET /api/links`: the caller's links.
pub async fn list<M: Model>(
  State(state): State<AppState>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let filters = Params::parse(query.as_deref()).filters(PAGING_SEARCH_SORTING)?;
  let user_id = auth?.user()?.id;

  Ok(Json(
    link::list(&state.db, M::KIND, Scope::User(user_id), filters).await?,
  ))
}

/// `GET /api/teams/{teamId}/links`: the team's links.
pub async fn team_list<M: Model>(
  State(state): State<AppState>,
  Path(team_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let filters = Params::parse(query.as_deref()).filters(PAGING_SEARCH_SORTING)?;
  let auth = auth?;
  can::require(can::can_view_team(&state.db, &auth, &team_id).await?)?;

  let scope = Scope::Team(db_id(&team_id)?);

  Ok(Json(link::list(&state.db, M::KIND, scope, filters).await?))
}

/// `POST /api/links`: creates a link for the caller or for a team.
pub async fn create<M: Model>(
  State(state): State<AppState>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let kind = M::KIND;
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let name = name(body::string(object, "name")?)?;
  let url = match kind {
    Kind::Link => Some(url(body::string(object, "url")?)?),
    Kind::Pixel => None,
  };
  let slug = slug(body::string(object, "slug")?)?;
  let team_id = body::nullable_uuid(object, "teamId")?;
  let id = body::nullable_uuid(object, "id")?;

  let auth = auth?;
  let team_allowed = match team_id {
    Some(team_id) => can::can_create_team_website(&state.db, &auth, &team_id.to_string()).await?,
    None => true,
  };
  if !team_allowed || !can::can_create_website(&auth) {
    return Err(AppError::Unauthorized);
  }
  let user_id = auth.user()?.id;
  // Only admins can name a team they are not in; upstream accepts any ID.
  if let Some(team_id) = team_id
    && !team::exists(&state.db, team_id).await?
  {
    return Err(AppError::team_not_found());
  }

  let created = link::create(
    &state.db,
    kind,
    &NewLink {
      id: id.unwrap_or_else(|| state.new_id()),
      name,
      url,
      slug,
      user_id: team_id.is_none().then_some(user_id),
      team_id,
    },
  )
  .await
  .map_err(|e| taken(e, kind))?;

  Ok(to_json(&created))
}

/// `GET /api/links/{linkId}`: the link, or `null` if there is none.
pub async fn get<M: Model>(
  State(state): State<AppState>,
  auth: Auth,
  Path(id): Path<String>,
) -> AppResult<Json<Value>> {
  let kind = M::KIND;
  can::require(can::can_view_link_or_pixel(&state.db, &auth, kind.entity_type(), &id).await?)?;

  match link::find(&state.db, kind, db_id(&id)?).await? {
    Some(found) => Ok(to_json(&found)),
    None => Ok(Json(Value::Null)),
  }
}

/// `POST /api/links/{linkId}`: renames the link or changes its slug or URL.
pub async fn update<M: Model>(
  State(state): State<AppState>,
  Path(id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let kind = M::KIND;
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let changes = LinkUpdate {
    name: body::optional_string(object, "name")?.map(name).transpose()?,
    url: match kind {
      Kind::Link => body::optional_string(object, "url")?.map(url).transpose()?,
      Kind::Pixel => None,
    },
    slug: body::optional_string(object, "slug")?.map(slug).transpose()?,
  };

  let auth = auth?;
  can::require(can::can_change_link_or_pixel(&state.db, &auth, kind.entity_type(), &id, WEBSITE_UPDATE).await?)?;

  // Admins pass for any ID, where upstream then fails with a 500.
  let updated = link::update(&state.db, kind, db_id(&id)?, &changes)
    .await
    .map_err(|e| taken(e, kind))?
    .ok_or(AppError::NotFound)?;

  Ok(to_json(&updated))
}

/// `DELETE /api/links/{linkId}`: deletes the link, its share links and its
/// collected events.
pub async fn delete<M: Model>(
  State(state): State<AppState>,
  auth: Auth,
  Path(id): Path<String>,
) -> AppResult<Json<Value>> {
  let kind = M::KIND;
  can::require(can::can_change_link_or_pixel(&state.db, &auth, kind.entity_type(), &id, WEBSITE_DELETE).await?)?;

  if !link::delete(&state.db, kind, db_id(&id)?).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
}

/// `GET /api/links/{linkId}/shares`: the link's share links, newest first.
pub async fn share_list<M: Model>(
  State(state): State<AppState>,
  Path(id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let filters = shares::list_filters(query.as_deref())?;

  let auth = auth?;
  can::require(can::can_view_link_or_pixel(&state.db, &auth, M::KIND.entity_type(), &id).await?)?;

  Ok(Json(share::list(&state.db, db_id(&id)?, &filters).await?))
}

/// `POST /api/links/{linkId}/shares`: a new share link for the link, with a
/// random slug.
pub async fn share_create<M: Model>(
  State(state): State<AppState>,
  Path(id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let kind = M::KIND;
  let body = body::parse(&body);
  let (name, parameters) = shares::new_share(body::object(&body)?)?;

  let auth = auth?;
  can::require(can::can_change_link_or_pixel(&state.db, &auth, kind.entity_type(), &id, WEBSITE_UPDATE).await?)?;
  // Admins pass for any ID; upstream then shares a link that does not exist.
  let Some(found) = link::find(&state.db, kind, db_id(&id)?).await? else {
    return Err(AppError::NotFound);
  };

  shares::create_random(&state, found.id, kind.entity_type(), name, parameters).await
}

/// The `ids` parameter: 1 to 20 comma-separated UUIDs.
fn chart_ids(value: Option<&str>) -> AppResult<Vec<(&str, Uuid)>> {
  let ids = value
    .ok_or_else(AppError::bad_request)?
    .split(',')
    .map(body::trim)
    .filter(|id| !id.is_empty())
    .map(|id| zod::uuid(id).map(|uuid| (id, uuid)).ok_or_else(AppError::bad_request))
    .collect::<AppResult<Vec<_>>>()?;

  if ids.is_empty() || ids.len() > 20 {
    return Err(AppError::bad_request());
  }
  Ok(ids)
}

/// An optional `z.coerce.number().int()` timestamp.
fn timestamp(params: &Params, key: &str) -> AppResult<Option<DateTime<Utc>>> {
  params
    .get(key)
    .map(|value| {
      let number = js::string_to_number(value);
      if number.fract() != 0.0 {
        return Err(AppError::bad_request());
      }
      date::from_js_millis(number).ok_or_else(AppError::bad_request)
    })
    .transpose()
}

/// `GET /api/links/charts`: the sparklines of the links list, for the given
/// links the caller can view (others are left out).
pub async fn charts<M: Model>(
  State(state): State<AppState>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let kind = M::KIND;
  let params = Params::parse(query.as_deref());
  let ids = chart_ids(params.get("ids"))?;
  let (start_at, end_at) = (timestamp(&params, "startAt")?, timestamp(&params, "endAt")?);
  let timezone = match params.get("timezone") {
    Some(value) => {
      date::time_zone(&state.db, value)
        .await?
        .ok_or_else(AppError::bad_request)?
    }
    None => "UTC".to_owned(),
  };

  let auth = auth?;
  let mut visible = Vec::new();
  for (id, uuid) in ids {
    if can::can_view_link_or_pixel(&state.db, &auth, kind.entity_type(), id).await? {
      visible.push(uuid);
    }
  }

  let range = match (start_at, end_at) {
    (Some(start), Some(end)) => (start, end),
    _ => charts::default_range(&state.db, &timezone).await?,
  };
  let event_type = match kind {
    Kind::Link => event_type::LINK_EVENT,
    Kind::Pixel => event_type::PIXEL_EVENT,
  };
  let data = charts::list_charts(&state.db, &visible, range, &timezone, Some(event_type)).await?;

  Ok(Json(json!({ "data": data })))
}

#[cfg(test)]
mod tests {
  use super::*;

  const ID: &str = "0d8e3a5c-1f2b-4c3d-9e4f-5a6b7c8d9e0f";

  #[test]
  fn parses_chart_ids_like_upstream() {
    let value = format!(" {ID} ,,{ID}");
    let ids = chart_ids(Some(&value)).unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0].0, ID);

    let many = vec![ID; 21].join(",");
    for invalid in [None, Some(""), Some(" , "), Some("x"), Some(many.as_str())] {
      assert!(chart_ids(invalid).is_err(), "{invalid:?}");
    }
  }

  #[test]
  fn validates_slugs() {
    assert!(slug("12345678").is_ok());
    assert!(slug("1234567").is_err());
    assert!(slug(&"s".repeat(101)).is_err());
    // zod counts UTF-16 code units.
    assert!(slug("😀😀😀😀").is_ok());
  }

  #[test]
  fn timestamps_are_integers() {
    let params = Params::parse(Some("a=1700000000000&b=1.5&c=x&d="));

    assert!(timestamp(&params, "a").unwrap().is_some());
    assert!(timestamp(&params, "b").is_err());
    assert!(timestamp(&params, "c").is_err());
    // `Number("")` is 0.
    assert_eq!(timestamp(&params, "d").unwrap(), DateTime::from_timestamp_millis(0));
    assert_eq!(timestamp(&params, "missing").unwrap(), None);
  }
}
