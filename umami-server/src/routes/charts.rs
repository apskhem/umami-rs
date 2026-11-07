//! The query of the list sparkline routes (`/api/websites/charts`,
//! `/api/links/charts`, `/api/pixels/charts`), which share upstream's schema.

use chrono::{
  DateTime,
  Utc,
};
use serde_json::{
  Value,
  json,
};
use uuid::Uuid;

use super::body;
use super::params::Params;
use crate::analytics::{
  charts,
  date,
};
use crate::error::{
  AppError,
  AppResult,
};
use crate::state::AppState;
use crate::{
  js,
  zod,
};

/// A validated chart request: the requested IDs (as given, and parsed), the
/// explicit range if any, and the time zone.
pub struct ChartQuery {
  pub ids: Vec<(String, Uuid)>,
  range: Option<(DateTime<Utc>, DateTime<Utc>)>,
  timezone: String,
}

impl ChartQuery {
  /// Validates the query as upstream's schema does (a 400 on failure): `ids`,
  /// optional integer `startAt` and `endAt`, and an optional `timezone`.
  pub async fn parse(state: &AppState, query: Option<&str>) -> AppResult<Self> {
    let params = Params::parse(query);
    let ids = ids(params.get("ids"))?;
    let (start_at, end_at) = (timestamp(&params, "startAt")?, timestamp(&params, "endAt")?);
    let timezone = match params.get("timezone") {
      Some(value) => {
        date::time_zone(&state.db, value)
          .await?
          .ok_or_else(AppError::bad_request)?
      }
      None => "UTC".to_owned(),
    };

    Ok(Self {
      ids,
      // Upstream uses the range only if both ends are given.
      range: start_at.zip(end_at),
      timezone,
    })
  }

  /// The `{ data }` response for the `visible` IDs: their charts over the
  /// requested range, or by default the last seven days and today.
  pub async fn respond(&self, state: &AppState, visible: &[Uuid], event_type: Option<i32>) -> AppResult<Value> {
    let range = match self.range {
      Some(range) => range,
      None => charts::default_range(&state.db, &self.timezone).await?,
    };
    let data = charts::list_charts(&state.db, visible, range, &self.timezone, event_type).await?;

    Ok(json!({ "data": data }))
  }
}

/// The `ids` parameter: 1 to 20 comma-separated UUIDs.
fn ids(value: Option<&str>) -> AppResult<Vec<(String, Uuid)>> {
  let ids = value
    .ok_or_else(AppError::bad_request)?
    .split(',')
    .map(body::trim)
    .filter(|id| !id.is_empty())
    .map(|id| {
      zod::uuid(id)
        .map(|uuid| (id.to_owned(), uuid))
        .ok_or_else(AppError::bad_request)
    })
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

#[cfg(test)]
mod tests {
  use super::*;

  const ID: &str = "0d8e3a5c-1f2b-4c3d-9e4f-5a6b7c8d9e0f";

  #[test]
  fn parses_ids_like_upstream() {
    let parsed = ids(Some(&format!(" {ID} ,,{ID}"))).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].0, ID);

    let many = vec![ID; 21].join(",");
    for invalid in [None, Some(""), Some(" , "), Some("x"), Some(many.as_str())] {
      assert!(ids(invalid).is_err(), "{invalid:?}");
    }
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
