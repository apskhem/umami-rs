//! `/api/websites` and the website management routes under
//! `/api/websites/{websiteId}`, ported from upstream's `src/app/api/websites`.

use std::sync::LazyLock;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use fancy_regex::Regex;
use serde_json::{
  Map,
  Value,
  json,
};

use super::charts::ChartQuery;
use super::params::{
  PAGING_SEARCH_SORTING,
  Params,
};
use super::{
  body,
  ok,
};
use crate::auth::Auth;
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
use crate::queries::share::{
  self,
  NewShare,
};
use crate::queries::website::{
  self,
  NewWebsite,
  Website,
  WebsiteUpdate,
};
use crate::queries::{
  team,
  user,
};
use crate::state::AppState;

/// Upstream's `DOMAIN_REGEX` (`\d` spelled `[0-9]`, as JavaScript reads it).
static DOMAIN: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(
        r"^(localhost(:[1-9][0-9]{0,4})?|((?=[a-z0-9_-]{1,63}\.)(xn--)?[a-z0-9_-]+(-[a-z0-9_-]+)*\.)+(xn--)?[a-z0-9_-]{2,63})$",
    )
    .expect("valid domain regex")
});

/// A website name: `z.string().trim().min(1).max(100)`.
fn name(value: &str) -> AppResult<&str> {
  let name = body::max_len(body::trim(value), 100)?;
  if name.is_empty() {
    return Err(AppError::bad_request());
  }
  Ok(name)
}

/// A domain: `z.string().trim().regex(DOMAIN_REGEX).max(500)`. Over-long values
/// are rejected before matching, which bounds the regex's backtracking (a match
/// that hits the backtracking limit counts as a mismatch).
fn domain(value: &str) -> AppResult<&str> {
  let domain = body::max_len(body::trim(value), 500)?;
  if !DOMAIN.is_match(domain).unwrap_or(false) {
    return Err(AppError::bad_request());
  }
  Ok(domain)
}

/// A share slug: `z.string().max(50).nullable().optional()`.
fn share_id(object: &Map<String, Value>) -> AppResult<Option<Option<&str>>> {
  let share_id = body::nullable_string(object, "shareId")?;
  if let Some(Some(slug)) = share_id {
    body::max_len(slug, 50)?;
  }
  Ok(share_id)
}

fn with_share(website: &Website, share_id: Option<String>) -> Value {
  let mut object = website.to_json();
  object.insert("shareId".to_owned(), json!(share_id));
  Value::Object(object)
}

/// `GET /api/websites`: the caller's websites, or with `includeTeams` also those
/// of teams they own or manage.
pub async fn list(
  State(state): State<AppState>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let filters = params.filters(PAGING_SEARCH_SORTING)?;
  let user_id = auth?.user()?.id;

  let scope = website::Scope::for_user(user_id, params.flag("includeTeams"));

  Ok(Json(website::list(&state.db, scope, filters).await?))
}

/// `GET /api/websites/charts`: the sparklines of the websites list, for the
/// given websites the caller can view (others are left out).
pub async fn charts(
  State(state): State<AppState>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let chart = ChartQuery::parse(&state, query.as_deref()).await?;

  let auth = auth?;
  let ids: Vec<(&str, uuid::Uuid)> = chart.ids.iter().map(|(id, uuid)| (id.as_str(), *uuid)).collect();
  let visible = can::viewable_websites(&state.db, &auth, &ids).await?;

  Ok(Json(chart.respond(&state, &visible, None).await?))
}

/// `POST /api/websites`: creates a website for the caller or for a team.
pub async fn create(State(state): State<AppState>, auth: AppResult<Auth>, body: Bytes) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let name = name(body::string(object, "name")?)?;
  let domain = domain(body::string(object, "domain")?)?;
  let share_id = share_id(object)?.flatten();
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

  // Upstream creates the website before the share link, so a taken share ID
  // fails with a 500 and leaves the website behind; here neither is created.
  let mut tx = state.db.begin().await?;
  let created = website::create(
    &mut tx,
    &NewWebsite {
      id: id.unwrap_or_else(|| state.new_id()),
      name,
      domain,
      user_id: team_id.is_none().then_some(user_id),
      team_id,
      created_by: user_id,
    },
  )
  .await
  .map_err(|e| AppError::taken(e, "That website ID is already taken."))?;

  let share = match share_id.filter(|slug| !slug.is_empty()) {
    Some(slug) => {
      Some(
        share::create(&mut *tx, &NewShare::for_website(state.new_id(), &created, slug))
          .await
          .map_err(|e| AppError::taken(e, share::SLUG_TAKEN))?
          .slug,
      )
    }
    None => None,
  };
  tx.commit().await?;

  Ok(Json(with_share(&created, share)))
}

/// `GET /api/websites/{websiteId}`: the website, or `null` if there is none.
pub async fn get(State(state): State<AppState>, auth: Auth, Path(website_id): Path<String>) -> AppResult<Json<Value>> {
  can::require(can::can_view_shared_website(&state.db, &auth, &website_id).await?)?;

  match website::get(&state.db, &website_id).await? {
    Some(found) => Ok(Json(website::with_share_id(&state.db, &found).await?)),
    None => Ok(Json(Value::Null)),
  }
}

/// `POST /api/websites/{websiteId}`: renames the website, changes its domain,
/// share link or recorder settings.
pub async fn update(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let name = body::optional_string(object, "name")?.map(name).transpose()?;
  let domain = body::optional_string(object, "domain")?.map(domain).transpose()?;
  let share_id = share_id(object)?;
  let replay_input = replay_config_input(object)?;

  let auth = auth?;
  can::require(can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await?)?;

  let Some(current) = website::get(&state.db, &website_id).await? else {
    return Err(AppError::BadRequest("Website not found.".to_owned()));
  };

  // A `null` config clears the settings; otherwise the given ones are merged in.
  let replay_config = replay_input.map(|input| {
    let mut merged = match input {
      Some(_) => website::recorder_config(current.replay_config.as_ref().unwrap_or(&Value::Null)),
      None => Map::new(),
    };
    merged.extend(input.unwrap_or_default());
    let next = website::recorder_config(&Value::Object(merged));
    let enabled = website::recorder_enabled(&next);
    (Value::Object(next), enabled)
  });

  let changes = WebsiteUpdate {
    name,
    domain,
    replay_config,
  };
  // Upstream saves the other changes even when the share ID is taken; here
  // the update applies entirely or not at all.
  let mut tx = state.db.begin().await?;
  let updated = website::update(&mut tx, current.website_id, &changes)
    .await?
    .ok_or_else(|| AppError::Internal(anyhow::anyhow!("website disappeared during update")))?;

  if share_id == Some(None) {
    share::delete_for_entities(&mut *tx, &[updated.website_id]).await?;
  }
  let created_share = match share_id.flatten().filter(|slug| !slug.is_empty()) {
    Some(slug) => {
      Some(
        share::create(&mut *tx, &NewShare::for_website(state.new_id(), &updated, slug))
          .await
          .map_err(|e| AppError::taken(e, share::SLUG_TAKEN))?
          .slug,
      )
    }
    None => None,
  };
  tx.commit().await?;

  let share = match created_share {
    Some(slug) => Some(slug),
    None => share::latest_slug(&state.db, updated.website_id).await?,
  };

  Ok(Json(with_share(&updated, share)))
}

/// The `replayConfig` field (upstream's `replayConfigInputSchema`, nullable and
/// optional): absent is `None`, `null` is `Some(None)`. Unknown keys are dropped.
fn replay_config_input(object: &Map<String, Value>) -> AppResult<Option<Option<Map<String, Value>>>> {
  let input = match object.get("replayConfig") {
    None => return Ok(None),
    Some(Value::Null) => return Ok(Some(None)),
    Some(Value::Object(input)) => input,
    Some(_) => return Err(AppError::bad_request()),
  };

  let mut parsed = Map::new();
  for (key, value) in input {
    let valid = match key.as_str() {
      "replayEnabled" | "heatmapEnabled" => value.is_boolean(),
      "sampleRate" | "heatmapSampleRate" => value.as_f64().is_some_and(|rate| (0.0..=1.0).contains(&rate)),
      "maskLevel" => matches!(value.as_str(), Some("strict" | "moderate")),
      "maxDuration" => {
        value
          .as_f64()
          .is_some_and(|d| d > 0.0 && d.fract() == 0.0 && d <= 9_007_199_254_740_991.0)
      }
      "blockSelector" => value.is_string(),
      _ => continue,
    };
    if !valid {
      return Err(AppError::bad_request());
    }
    parsed.insert(key.clone(), value.clone());
  }

  Ok(Some(Some(parsed)))
}

/// `DELETE /api/websites/{websiteId}`: deletes the website and its data.
pub async fn delete(
  State(state): State<AppState>,
  auth: Auth,
  Path(website_id): Path<String>,
) -> AppResult<Json<Value>> {
  can::require(can::can_change_website(&state.db, &auth, &website_id, WEBSITE_DELETE).await?)?;

  if !website::delete(&state.db, db_id(&website_id)?).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
}

/// `POST /api/websites/{websiteId}/reset`: deletes the website's collected data.
pub async fn reset(
  State(state): State<AppState>,
  auth: Auth,
  Path(website_id): Path<String>,
) -> AppResult<Json<Value>> {
  can::require(can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await?)?;

  if !website::reset(&state.db, db_id(&website_id)?).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
}

/// `POST /api/websites/{websiteId}/transfer`: moves the website to a user or a team.
pub async fn transfer(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let object = body::object(&body)?;
  let user_id = body::optional_string(object, "userId")?.map(body::uuid).transpose()?;
  let team_id = body::optional_string(object, "teamId")?.map(body::uuid).transpose()?;

  let auth = auth?;
  let (allowed, owner) = if let Some(user_id) = user_id {
    let allowed = can::can_transfer_website_to_user(&state.db, &auth, &website_id, user_id).await?;
    (allowed, (Some(user_id), None))
  } else if let Some(team_id) = team_id {
    let allowed = can::can_transfer_website_to_team(&state.db, &auth, &website_id, team_id).await?;
    (allowed, (None, Some(team_id)))
  } else {
    return Err(AppError::bad_request());
  };
  if !allowed {
    return Err(AppError::Unauthorized);
  }

  // Admins may name any user or team; upstream accepts IDs that do not exist.
  let (user_id, team_id) = owner;
  if let Some(user_id) = user_id
    && user::get(&state.db, user_id).await?.is_none()
  {
    return Err(AppError::user_not_found());
  }
  if let Some(team_id) = team_id
    && !team::exists(&state.db, team_id).await?
  {
    return Err(AppError::team_not_found());
  }

  let moved = website::transfer(&state.db, db_id(&website_id)?, user_id, team_id)
    .await?
    .ok_or(AppError::NotFound)?;

  Ok(Json(Value::Object(moved.to_json())))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn matches_upstream_domains() {
    for valid in [
      "example.com",
      "sub.example.co.uk",
      "localhost",
      "localhost:3000",
      "xn--bcher-kva.example",
      "my_site.example",
    ] {
      assert!(domain(valid).is_ok(), "{valid}");
    }
    assert_eq!(domain("  example.com ").unwrap(), "example.com");

    for invalid in [
      "",
      "Example.com",
      "example",
      "localhost:0",
      "localhost:123456",
      "https://example.com",
      "example.com/path",
      "a.b",
    ] {
      assert!(domain(invalid).is_err(), "{invalid}");
    }
  }

  #[test]
  fn rejects_pathological_domains_quickly() {
    let evil = format!("{}!", "a-".repeat(240));
    let started = std::time::Instant::now();

    assert!(domain(&evil).is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
  }

  #[test]
  fn sanitizes_recorder_config_like_upstream() {
    let config = website::recorder_config(&json!({
        "replayEnabled": true,
        "heatmapEnabled": false,
        "sampleRate": 0.5,
        "maskLevel": "loose",
        "maxDuration": 2.5,
        "blockSelector": ".secret",
        "other": 1,
    }));

    assert_eq!(
      Value::Object(config.clone()),
      json!({
          "replayEnabled": true,
          "sampleRate": 0.5,
          "maxDuration": 3,
          "blockSelector": ".secret",
      })
    );
    assert!(website::recorder_enabled(&config));
    assert!(!website::recorder_enabled(&website::recorder_config(
      &json!({ "heatmapEnabled": false })
    )));
    assert!(website::recorder_config(&json!([1])).is_empty());
  }

  #[test]
  fn validates_replay_input() {
    let input = |value: Value| {
      let body = json!({ "replayConfig": value });
      replay_config_input(body.as_object().unwrap())
    };

    assert_eq!(
      input(json!({ "sampleRate": 1, "unknown": "x" })).unwrap(),
      Some(Some(json!({ "sampleRate": 1 }).as_object().unwrap().clone()))
    );
    assert_eq!(input(Value::Null).unwrap(), Some(None));
    for invalid in [
      json!({ "sampleRate": 2 }),
      json!({ "maxDuration": 0 }),
      json!({ "maxDuration": 1.5 }),
      json!({ "maskLevel": "none" }),
      json!({ "replayEnabled": "yes" }),
      json!({ "blockSelector": null }),
      json!("on"),
    ] {
      assert!(input(invalid.clone()).is_err(), "{invalid}");
    }
  }
}
