//! `/api/websites/{websiteId}/segments` and `/api/websites/{websiteId}/segments/{segmentId}`:
//! saved segments and cohorts, ported from upstream's
//! `src/app/api/websites/[websiteId]/segments`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use serde_json::{
  Map,
  Value,
  json,
};

use super::body;
use super::params::Params;
use crate::auth::Auth;
use crate::crypto;
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
use crate::queries::segment::{
  self,
  Segment,
};
use crate::queries::{
  Filters,
  website,
};
use crate::state::AppState;

/// Upstream's `segmentTypeParam`.
const TYPES: &[&str] = &["segment", "cohort"];

/// Upstream's `operatorParam`.
const OPERATORS: &[&str] = &[
  "eq", "neq", "s", "ns", "c", "dnc", "re", "nre", "t", "f", "gt", "lt", "gte", "lte", "bf", "af",
];

/// Upstream's `FIELD_LENGTH.dataKey`.
const DATA_KEY_LENGTH: usize = 500;

/// A segment's `type`, `name` and `parameters`, validated.
struct Input<'a> {
  kind: &'a str,
  name: &'a str,
  parameters: Value,
}

/// Upstream's create and update schema: `type`, `name` (at most 200
/// characters) and `parameters` (`segmentParamSchema`). Cohorts cannot have
/// session property filters.
fn input(object: &Map<String, Value>) -> AppResult<Input<'_>> {
  let kind = body::one_of(body::string(object, "type")?, TYPES)?;
  let name = body::max_len(body::string(object, "name")?, 200)?;
  let parameters = parameters(object.get("parameters"))?;

  let has_session_filters = parameters
    .get("sessionPropertyFilters")
    .and_then(Value::as_array)
    .is_some_and(|filters| !filters.is_empty());
  if kind == "cohort" && has_session_filters {
    return Err(AppError::BadRequest(
      "Session property filters are only supported for segments.".to_owned(),
    ));
  }

  Ok(Input {
    kind,
    name,
    parameters: Value::Object(parameters),
  })
}

/// Upstream's `segmentParamSchema`. Like zod, the result keeps only the known
/// fields.
fn parameters(value: Option<&Value>) -> AppResult<Map<String, Value>> {
  let object = value.and_then(Value::as_object).ok_or_else(AppError::bad_request)?;
  let mut parsed = Map::new();

  if let Some(filters) = optional_array(object, "filters")? {
    let filters = filters
      .iter()
      .map(|filter| {
        let filter = filter.as_object().ok_or_else(AppError::bad_request)?;
        Ok(json!({
            "name": body::string(filter, "name")?,
            "operator": operator(filter)?,
            "value": body::string(filter, "value")?,
        }))
      })
      .collect::<AppResult<Vec<_>>>()?;
    parsed.insert("filters".to_owned(), json!(filters));
  }

  if let Some(filters) = optional_array(object, "sessionPropertyFilters")? {
    let filters = filters
      .iter()
      .map(|filter| {
        let filter = filter.as_object().ok_or_else(AppError::bad_request)?;
        let property = body::max_len(body::string(filter, "propertyName")?, DATA_KEY_LENGTH)?;
        if property.is_empty() {
          return Err(AppError::bad_request());
        }
        // `DATA_TYPE`: string, number, boolean, date, array.
        let data_type = filter
          .get("dataType")
          .and_then(Value::as_f64)
          .filter(|t| matches!(*t, 1.0 | 2.0 | 3.0 | 4.0 | 5.0))
          .ok_or_else(AppError::bad_request)?;
        Ok(json!({
            "propertyName": property,
            "dataType": data_type as i64,
            "operator": operator(filter)?,
            "value": body::string(filter, "value")?,
        }))
      })
      .collect::<AppResult<Vec<_>>>()?;
    parsed.insert("sessionPropertyFilters".to_owned(), json!(filters));
  }

  if let Some(match_) = body::optional_string(object, "match")? {
    parsed.insert("match".to_owned(), json!(body::one_of(match_, &["all", "any"])?));
  }

  if let Some(date_range) = body::optional_string(object, "dateRange")? {
    parsed.insert("dateRange".to_owned(), json!(date_range));
  }

  match object.get("action") {
    None => {}
    Some(Value::Object(action)) => {
      parsed.insert(
        "action".to_owned(),
        json!({
            "type": body::string(action, "type")?,
            "value": body::string(action, "value")?,
        }),
      );
    }
    Some(_) => return Err(AppError::bad_request()),
  }

  Ok(parsed)
}

/// An optional array field (`z.array(...).optional()`).
fn optional_array<'a>(object: &'a Map<String, Value>, key: &str) -> AppResult<Option<&'a Vec<Value>>> {
  match object.get(key) {
    None => Ok(None),
    Some(Value::Array(items)) => Ok(Some(items)),
    Some(_) => Err(AppError::bad_request()),
  }
}

fn operator(filter: &Map<String, Value>) -> AppResult<&str> {
  body::one_of(body::string(filter, "operator")?, OPERATORS)
}

/// The website's segment `segment_id`, or a 404.
async fn find(state: &AppState, website_id: &str, segment_id: &str) -> AppResult<Segment> {
  segment::get(&state.db, db_id(website_id)?, db_id(segment_id)?)
    .await?
    .ok_or(AppError::NotFound)
}

/// `GET /api/websites/{websiteId}/segments?type=segment|cohort`: the website's
/// segments or cohorts, searchable by name.
pub async fn list(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let kind = body::one_of(params.get("type").ok_or_else(AppError::bad_request)?, TYPES)?;
  // The schema has no paging parameters, so every request gets the first page.
  let filters = Filters {
    search: params.get("search").map(str::to_owned),
    ..Filters::default()
  };

  let auth = auth?;
  if !can::can_view_shared_website_filters(&state.db, &auth, &website_id).await? {
    return Err(AppError::Unauthorized);
  }

  Ok(Json(
    segment::list(&state.db, db_id(&website_id)?, kind, &filters).await?,
  ))
}

/// `POST /api/websites/{websiteId}/segments`: saves a segment or cohort.
pub async fn create(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let input = input(body::object(&body)?)?;

  let auth = auth?;
  if !can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await? {
    return Err(AppError::Unauthorized);
  }
  // Admins pass for any ID; upstream then saves a segment for no website.
  let Some(found) = website::get(&state.db, &website_id).await? else {
    return Err(AppError::NotFound);
  };

  let id = crypto::random_uuid(state.config.collect.use_uuidv7);
  let created = segment::create(
    &state.db,
    id,
    found.website_id,
    input.kind,
    input.name,
    &input.parameters,
  )
  .await?;

  Ok(Json(created.to_json()))
}

/// `GET /api/websites/{websiteId}/segments/{segmentId}`: the segment or cohort.
pub async fn get(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, segment_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  if !can::can_view_shared_website_filters(&state.db, &auth, &website_id).await? {
    return Err(AppError::Unauthorized);
  }

  Ok(Json(find(&state, &website_id, &segment_id).await?.to_json()))
}

/// `POST /api/websites/{websiteId}/segments/{segmentId}`: replaces the type,
/// name and parameters of a segment or cohort.
pub async fn update(
  State(state): State<AppState>,
  Path((website_id, segment_id)): Path<(String, String)>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let input = input(body::object(&body)?)?;

  let auth = auth?;
  if !can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await? {
    return Err(AppError::Unauthorized);
  }
  let found = find(&state, &website_id, &segment_id).await?;

  let updated = segment::update(&state.db, found.segment_id, input.kind, input.name, &input.parameters)
    .await?
    .ok_or(AppError::NotFound)?;

  Ok(Json(updated.to_json()))
}

/// `DELETE /api/websites/{websiteId}/segments/{segmentId}`: deletes the segment
/// or cohort.
pub async fn delete(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, segment_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  if !can::can_change_website(&state.db, &auth, &website_id, WEBSITE_DELETE).await? {
    return Err(AppError::Unauthorized);
  }
  let found = find(&state, &website_id, &segment_id).await?;

  if !segment::delete(&state.db, found.segment_id).await? {
    return Err(AppError::NotFound);
  }

  Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(value: Value) -> AppResult<Value> {
    parameters(Some(&value)).map(Value::Object)
  }

  #[test]
  fn keeps_only_known_parameters() {
    let parsed = parse(json!({
        "filters": [{ "name": "browser", "operator": "eq", "value": "chrome", "extra": 1 }],
        "sessionPropertyFilters": [
            { "propertyName": "plan", "dataType": 1.0, "operator": "c", "value": "pro" },
        ],
        "match": "any",
        "dateRange": "30day",
        "action": { "type": "path", "value": "/", "other": true },
        "unknown": "dropped",
    }))
    .unwrap();

    assert_eq!(
      parsed,
      json!({
          "filters": [{ "name": "browser", "operator": "eq", "value": "chrome" }],
          "sessionPropertyFilters": [
              { "propertyName": "plan", "dataType": 1, "operator": "c", "value": "pro" },
          ],
          "match": "any",
          "dateRange": "30day",
          "action": { "type": "path", "value": "/" },
      })
    );
    assert_eq!(parse(json!({})).unwrap(), json!({}));
  }

  #[test]
  fn rejects_invalid_parameters() {
    for invalid in [
      json!(null),
      json!([]),
      json!({ "filters": {} }),
      json!({ "filters": [{ "name": "browser", "operator": "like", "value": "x" }] }),
      json!({ "filters": [{ "name": "browser", "operator": "eq", "value": 1 }] }),
      json!({ "sessionPropertyFilters": [{ "propertyName": "", "dataType": 1, "operator": "eq", "value": "" }] }),
      json!({ "sessionPropertyFilters": [{ "propertyName": "p", "dataType": 6, "operator": "eq", "value": "" }] }),
      json!({ "sessionPropertyFilters": [{ "propertyName": "p", "dataType": "1", "operator": "eq", "value": "" }] }),
      json!({ "match": "none" }),
      json!({ "dateRange": null }),
      json!({ "action": { "type": "path" } }),
      json!({ "action": null }),
    ] {
      assert!(parse(invalid.clone()).is_err(), "{invalid}");
    }
  }

  #[test]
  fn cohorts_cannot_filter_session_properties() {
    let body = |kind: &str, filters: Value| {
      json!({
          "type": kind,
          "name": "n",
          "parameters": { "sessionPropertyFilters": filters },
      })
    };
    let filter = json!([{ "propertyName": "p", "dataType": 1, "operator": "eq", "value": "v" }]);

    assert!(input(body("segment", filter.clone()).as_object().unwrap()).is_ok());
    assert!(input(body("cohort", json!([])).as_object().unwrap()).is_ok());
    let error = input(body("cohort", filter).as_object().unwrap()).err().unwrap();
    assert_eq!(
      error.to_string(),
      "Session property filters are only supported for segments."
    );
  }
}
