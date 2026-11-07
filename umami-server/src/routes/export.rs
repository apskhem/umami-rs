//! `GET /api/websites/{websiteId}/export`: the website's top events, pages,
//! referrers, browsers, OSes, devices and countries for the period, as CSV
//! files in a base64 zip. Ported from upstream's
//! `src/app/api/websites/[websiteId]/export/route.ts`.

use std::io::{
  Cursor,
  Write as _,
};

use axum::Json;
use axum::extract::{
  Path,
  RawQuery,
  State,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{
  Value,
  json,
};
use zip::write::SimpleFileOptions;
use zip::{
  CompressionMethod,
  ZipWriter,
};

use super::params::Params;
use super::replays::authorize;
use crate::analytics::filters::Options;
use crate::analytics::queries::{
  self,
  DEFAULT_LIMIT,
  Page,
};
use crate::auth::Auth;
use crate::error::AppResult;
use crate::js;
use crate::state::AppState;

/// `GET .../export`: `{ zip }`, the CSV files base64-encoded.
pub async fn export(
  State(state): State<AppState>,
  Path(website_id): Path<String>,
  RawQuery(query): RawQuery,
  auth: AppResult<Auth>,
) -> AppResult<Json<Value>> {
  let params = Params::parse(query.as_deref());
  // Upstream's schema has only the date range and paging, so it strips filters.
  let options = Options {
    paging: true,
    ..Options::NONE
  };
  let (website_id, filters) = authorize(&state, auth, &website_id, &params, options).await?;
  let (db, page) = (
    &state.db,
    Page {
      limit: DEFAULT_LIMIT,
      offset: 0,
    },
  );

  let (events, pages, referrers, browsers, os, devices, countries) = tokio::try_join!(
    queries::event_metrics(db, website_id, "event", page, &filters),
    queries::pageview_metrics(db, website_id, "path", page, &filters),
    queries::pageview_metrics(db, website_id, "referrer", page, &filters),
    queries::session_metrics(db, website_id, "browser", page, &filters),
    queries::session_metrics(db, website_id, "os", page, &filters),
    queries::session_metrics(db, website_id, "device", page, &filters),
    queries::session_metrics(db, website_id, "country", page, &filters),
  )?;

  // Stored uncompressed, as JSZip's default.
  let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
  let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
  for (name, rows) in [
    ("events.csv", events),
    ("pages.csv", pages),
    ("referrers.csv", referrers),
    ("browsers.csv", browsers),
    ("os.csv", os),
    ("devices.csv", devices),
    ("countries.csv", countries),
  ] {
    zip.start_file(name, options).map_err(anyhow::Error::from)?;
    zip.write_all(csv(&rows).as_bytes()).map_err(anyhow::Error::from)?;
  }
  let bytes = zip.finish().map_err(anyhow::Error::from)?.into_inner();

  Ok(Json(json!({ "zip": BASE64.encode(bytes) })))
}

/// Upstream's CSV of query rows: `Papa.unparse(rows, { header: true,
/// skipEmptyLines: true })`, with string values that start like a spreadsheet
/// formula prefixed with `'`. No rows is an empty file.
fn csv(rows: &[Value]) -> String {
  let Some(first) = rows.first().and_then(Value::as_object) else {
    return String::new();
  };
  let fields: Vec<&String> = first.keys().collect();
  let line = |values: Vec<String>| values.join(",");

  let mut lines = vec![line(fields.iter().map(|name| csv_field(name)).collect())];
  for row in rows {
    lines.push(line(
      fields
        .iter()
        .map(|name| {
          match row.get(name.as_str()) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => csv_field(&escape_formula(s)),
            Some(Value::Number(n)) => csv_field(&js::number_to_string(n.as_f64().unwrap_or_default())),
            Some(Value::Bool(b)) => csv_field(&b.to_string()),
            Some(other) => csv_field(&other.to_string()),
          }
        })
        .collect(),
    ));
  }
  lines.join("\r\n")
}

/// The route's guard against spreadsheet formulas: a `'` before a leading `=`,
/// `+`, `-`, `@`, tab or carriage return.
fn escape_formula(value: &str) -> String {
  if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
    format!("'{value}")
  } else {
    value.to_owned()
  }
}

/// Papa Parse's `safe`: quotes doubled, and the value quoted if it holds a
/// quote, comma, line break or byte order mark, or starts or ends with a space.
fn csv_field(value: &str) -> String {
  let escaped = value.replace('"', "\"\"");
  if escaped.contains(['"', ',', '\r', '\n', '\u{feff}']) || escaped.starts_with(' ') || escaped.ends_with(' ') {
    format!("\"{escaped}\"")
  } else {
    escaped
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn writes_csv_like_papa_parse() {
    let rows = [
      json!({ "x": "/a", "y": 3 }),
      json!({ "x": "say \"hi\", then go", "y": 1.5 }),
      json!({ "x": " padded", "y": null }),
      json!({ "x": "=SUM(A1)", "y": 1 }),
      json!({ "x": "-1\r\n", "y": 1e21 }),
    ];
    assert_eq!(
      csv(&rows),
      "x,y\r\n/a,3\r\n\"say \"\"hi\"\", then go\",1.5\r\n\" padded\",\r\n'=SUM(A1),1\r\n\"'-1\r\n\",1e+21"
    );
    assert_eq!(csv(&[]), "");
  }
}
