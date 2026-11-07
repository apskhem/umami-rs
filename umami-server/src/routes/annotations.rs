//! `/api/websites/{websiteId}/annotations` and `/{annotationId}`, ported from
//! upstream's `src/app/api/websites/[websiteId]/annotations`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use chrono::{
  DateTime,
  SubsecRound,
  Utc,
};
use serde_json::{
  Map,
  Value,
};

use super::params::{
  PAGING_SEARCH,
  Params,
};
use super::{
  body,
  ok,
};
use crate::analytics::date;
use crate::auth::Auth;
use crate::error::{
  AppError,
  AppResult,
};
use crate::js;
use crate::permissions::{
  self as can,
  WEBSITE_UPDATE,
  db_id,
};
use crate::queries::annotation::{
  self,
  Annotation,
  Note,
};
use crate::queries::website;
use crate::state::AppState;

/// zod's `z.coerce.date()` of a JSON value (`new Date(value)`), at
/// JavaScript's millisecond precision.
fn coerce_date(value: Option<&Value>) -> Option<DateTime<Utc>> {
  let date = match value? {
    Value::String(text) => date::parse_date(text)?,
    Value::Number(n) => date::from_js_millis(n.as_f64()?)?,
    // `new Date(null)` and `new Date(true)`: 0 and 1 ms after the epoch.
    Value::Null => date::from_js_millis(0.0)?,
    Value::Bool(b) => date::from_js_millis(f64::from(u8::from(*b)))?,
    Value::Array(_) | Value::Object(_) => return None,
  };
  Some(date.trunc_subsecs(3))
}

/// Upstream's `annotationSchema`: a date, `allDay` (by default `true`) and a
/// note of 1 to 500 characters.
fn note(object: &Map<String, Value>) -> AppResult<Note<'_>> {
  let date = coerce_date(object.get("date")).ok_or_else(AppError::bad_request)?;
  let all_day = match object.get("allDay") {
    None => true,
    Some(Value::Bool(all_day)) => *all_day,
    Some(_) => return Err(AppError::bad_request()),
  };
  let note = body::max_len(body::string(object, "note")?, 500)?;
  if note.is_empty() {
    return Err(AppError::bad_request());
  }

  Ok(Note { date, all_day, note })
}

/// An optional `z.coerce.number()` parameter as a JavaScript date.
fn timestamp(params: &Params, key: &str) -> AppResult<Option<f64>> {
  params
    .get(key)
    .map(|value| {
      let number = js::string_to_number(value);
      if number.is_nan() {
        Err(AppError::bad_request())
      } else {
        Ok(number)
      }
    })
    .transpose()
}

/// The website's annotation `annotation_id`; a 404 if there is none (or an ID
/// is malformed, where upstream fails with a 500).
async fn find(state: &AppState, website_id: &str, annotation_id: &str) -> AppResult<Annotation> {
  annotation::get(&state.db, db_id(website_id)?, db_id(annotation_id)?)
    .await?
    .ok_or(AppError::NotFound)
}

/// `GET .../annotations`: the website's annotations, latest first, between
/// `startAt` and `endAt` if both are given.
pub async fn list(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  let (start_at, end_at) = (timestamp(&params, "startAt")?, timestamp(&params, "endAt")?);
  let filters = params.filters(PAGING_SEARCH)?;

  let auth = auth?;
  can::require(can::can_view_shared_website_filters(&state.db, &auth, &website_id).await?)?;

  // Upstream fails with a 500 on timestamps JavaScript cannot represent.
  let range = match start_at.zip(end_at) {
    Some((start, end)) => {
      let date = |ms: f64| date::from_js_millis(ms).ok_or_else(AppError::bad_request);
      Some((date(start)?, date(end)?))
    }
    None => None,
  };

  Ok(Json(
    annotation::list(&state.db, db_id(&website_id)?, range, &filters).await?,
  ))
}

/// `POST .../annotations`: adds an annotation.
pub async fn create(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let note = note(body::object(&body)?)?;

  let auth = auth?;
  can::require(can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await?)?;
  // Admins pass for any ID; upstream then annotates a website that does not exist.
  let Some(found) = website::get(&state.db, &website_id).await? else {
    return Err(AppError::NotFound);
  };

  let created = annotation::create(&state.db, state.new_id(), found.website_id, auth.user()?.id, &note).await?;

  Ok(Json(created.to_json()))
}

/// `GET .../annotations/{annotationId}`
pub async fn get(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, annotation_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  can::require(can::can_view_shared_website_filters(&state.db, &auth, &website_id).await?)?;

  Ok(Json(find(&state, &website_id, &annotation_id).await?.to_json()))
}

/// `POST .../annotations/{annotationId}`: changes the annotation's date,
/// `allDay` (by default `true`) and note.
pub async fn update(
  State(state): State<AppState>,
  Path((website_id, annotation_id)): Path<(String, String)>,
  auth: AppResult<Auth>,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let body = body::parse(&body);
  let note = note(body::object(&body)?)?;

  let auth = auth?;
  can::require(can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await?)?;
  let found = find(&state, &website_id, &annotation_id).await?;

  let updated = annotation::update(&state.db, found.annotation_id, &note)
    .await?
    .ok_or(AppError::NotFound)?;

  Ok(Json(updated.to_json()))
}

/// `DELETE .../annotations/{annotationId}`: removes the annotation (for those
/// who may update the website, as upstream).
pub async fn delete(
  State(state): State<AppState>,
  auth: Auth,
  Path((website_id, annotation_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
  can::require(can::can_change_website(&state.db, &auth, &website_id, WEBSITE_UPDATE).await?)?;
  let found = find(&state, &website_id, &annotation_id).await?;

  if !annotation::delete(&state.db, found.annotation_id).await? {
    return Err(AppError::NotFound);
  }

  Ok(ok())
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn coerces_dates_like_zod() {
    let date = |value: Value| coerce_date(Some(&value)).map(|d| d.timestamp_millis());

    assert_eq!(date(json!("2026-03-04")), Some(1_772_582_400_000));
    assert_eq!(date(json!("2026-03-04T05:06:07.123456Z")), Some(1_772_600_767_123));
    assert_eq!(date(json!(1_700_000_000_000_u64)), Some(1_700_000_000_000));
    assert_eq!(date(json!(null)), Some(0));
    assert_eq!(date(json!(true)), Some(1));
    for invalid in [json!("soon"), json!([]), json!({}), json!(9e15)] {
      assert_eq!(date(invalid.clone()), None, "{invalid}");
    }
    assert_eq!(coerce_date(None), None);
  }
}
