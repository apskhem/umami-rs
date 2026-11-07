//! Raw analytics SQL, ported from upstream's `src/lib/prisma.ts`.
//!
//! Queries are written as upstream writes them, with `{{name}}` or
//! `{{name::type}}` placeholders, and rendered to positional parameters. Rows come
//! back as JSON with Prisma's types: `bigint` as a number, `numeric` as a string.

use std::collections::HashMap;
use std::sync::LazyLock;

use chrono::{
  DateTime,
  NaiveDate,
  NaiveDateTime,
  Utc,
};
use regex::Regex;
use rust_decimal::Decimal;
use serde_json::{
  Map,
  Value,
  json,
};
use sqlx::postgres::{
  PgArguments,
  PgRow,
};
use sqlx::{
  Arguments,
  Column,
  PgPool,
  Row,
  TypeInfo,
  ValueRef,
};
use uuid::Uuid;

use super::filters::{
  ColumnFilter,
  DATA_TYPE_ARRAY,
  DATA_TYPE_DATE,
  DATA_TYPE_NUMBER,
  Operator,
  PropertyFilter,
  QueryFilters,
  SESSION_COLUMNS,
  Values,
  filter_column,
};
use crate::error::{
  AppError,
  AppResult,
};
use crate::js;

/// A query parameter.
#[derive(Clone, Debug)]
pub enum Param {
  Uuid(Uuid),
  Text(String),
  Texts(Vec<String>),
  Numbers(Vec<i64>),
  Number(f64),
  Time(DateTime<Utc>),
}

/// Named parameters for a query.
#[derive(Clone, Debug, Default)]
pub struct Bindings(HashMap<String, Param>);

impl Bindings {
  pub fn set(&mut self, name: impl Into<String>, param: Param) {
    self.0.insert(name.into(), param);
  }

  fn extend(&mut self, other: Bindings) {
    self.0.extend(other.0);
  }
}

static PLACEHOLDER: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"\{\{\s*(\w+)(::\w+)?\s*\}\}").expect("valid regex"));

/// Upstream's `executeRawQuery`: placeholders become `$n` (one per occurrence).
fn render(sql: &str, bindings: &Bindings) -> AppResult<(String, PgArguments)> {
  let mut args = PgArguments::default();
  let mut count = 0;
  let mut missing = None;

  let rendered = PLACEHOLDER.replace_all(sql, |captures: &regex::Captures<'_>| {
    let name = &captures[1];
    let cast = captures.get(2).map_or("", |m| m.as_str());

    let added = match bindings.0.get(name) {
      Some(Param::Uuid(v)) => args.add(v),
      Some(Param::Text(v)) => args.add(v),
      Some(Param::Texts(v)) => args.add(v),
      Some(Param::Numbers(v)) => args.add(v),
      Some(Param::Number(v)) => args.add(v),
      Some(Param::Time(v)) => args.add(v),
      None => {
        missing.get_or_insert_with(|| name.to_owned());
        Ok(())
      }
    };
    if let Err(error) = added {
      missing.get_or_insert_with(|| format!("{name}: {error}"));
    }

    count += 1;
    format!("${count}{cast}")
  });

  if let Some(name) = missing {
    return Err(AppError::Internal(anyhow::anyhow!(
      "query parameter {name} is not bound"
    )));
  }

  Ok((rendered.into_owned(), args))
}

/// Runs a query and returns its rows as JSON objects.
pub async fn rows(db: &PgPool, sql: &str, bindings: &Bindings) -> AppResult<Vec<Value>> {
  let (sql, args) = render(sql, bindings)?;
  let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
    .fetch_all(db)
    .await
    .map_err(database_error)?;

  rows
    .iter()
    .map(|row| row_to_json(row).map(Value::Object))
    .collect::<Result<_, _>>()
    .map_err(AppError::from)
}

/// The first row, or `null`.
pub async fn row(db: &PgPool, sql: &str, bindings: &Bindings) -> AppResult<Value> {
  Ok(rows(db, sql, bindings).await?.into_iter().next().unwrap_or(Value::Null))
}

/// Upstream's `DEFAULT_PAGE_SIZE`.
const DEFAULT_PAGE_SIZE: i64 = 20;

/// The requested page's size and offset.
pub fn page_window(filters: &QueryFilters) -> (i64, i64) {
  let size = filters.page_size.unwrap_or(DEFAULT_PAGE_SIZE);
  let offset = size.saturating_mul(filters.page.unwrap_or(1) - 1);
  (size, offset)
}

/// Upstream's paged result: `{ data, count, page, pageSize, isCapped }`. With
/// `maxResults`, counting stops there and `isCapped` says whether it was reached.
pub fn page(data: Vec<Value>, count: i64, filters: &QueryFilters) -> Value {
  json!({
      "data": data,
      "count": count,
      "page": filters.page.unwrap_or(1),
      "pageSize": page_window(filters).0,
      "isCapped": filters.max_results.is_some_and(|max| count >= max),
  })
}

/// The number of rows `query` returns, up to `maxResults` if given.
pub async fn count(db: &PgPool, query: &str, bindings: &Bindings, filters: &QueryFilters) -> AppResult<i64> {
  let sql = match filters.max_results {
    Some(max) => {
      format!("select count(*) as num from (select 1 from ({query}) t limit {max}) t2")
    }
    None => format!("select count(*) as num from ({query}) t"),
  };

  Ok(row(db, &sql, bindings).await?["num"].as_i64().unwrap_or_default())
}

/// Upstream's `pagedRawQuery`, ordered by `order` (a trusted SQL fragment).
pub async fn paged(
  db: &PgPool,
  query: &str,
  bindings: &Bindings,
  filters: &QueryFilters,
  order: &str,
) -> AppResult<Value> {
  let count = count(db, query, bindings, filters).await?;
  let (size, offset) = page_window(filters);
  let data = rows(
    db,
    &format!("{query}\norder by {order}\nlimit {size} offset {offset}"),
    bindings,
  )
  .await?;

  Ok(page(data, count, filters))
}

/// Upstream's `pagedRawQuery` for a query that orders itself (no `orderBy`
/// in the schema): only the page is appended.
pub async fn paged_as_ordered(
  db: &PgPool,
  query: &str,
  bindings: &Bindings,
  filters: &QueryFilters,
) -> AppResult<Value> {
  let count = count(db, query, bindings, filters).await?;
  let (size, offset) = page_window(filters);
  let data = rows(db, &format!("{query}\nlimit {size} offset {offset}"), bindings).await?;

  Ok(page(data, count, filters))
}

/// Errors caused by the request's values (a bad regex or date in a filter) are a
/// 400; upstream fails with a 500.
fn database_error(error: sqlx::Error) -> AppError {
  if let sqlx::Error::Database(db) = &error
    && matches!(
      db.code().as_deref(),
      Some("2201B" | "22007" | "22008" | "22P02" | "22003")
    )
  {
    return AppError::bad_request();
  }
  error.into()
}

/// A row as Prisma's raw queries return it.
fn row_to_json(row: &PgRow) -> Result<Map<String, Value>, sqlx::Error> {
  let mut object = Map::new();

  for (index, column) in row.columns().iter().enumerate() {
    let raw = row.try_get_raw(index)?;
    let value = if raw.is_null() {
      Value::Null
    } else {
      let kind = raw.type_info().name().to_owned();
      match kind.as_str() {
        "INT8" => json!(row.try_get::<i64, _>(index)?),
        "INT4" => json!(row.try_get::<i32, _>(index)?),
        "INT2" => json!(row.try_get::<i16, _>(index)?),
        // JavaScript numbers: whole floats print as integers.
        "FLOAT8" => js::number(row.try_get::<f64, _>(index)?),
        "FLOAT4" => js::number(f64::from(row.try_get::<f32, _>(index)?)),
        "BOOL" => json!(row.try_get::<bool, _>(index)?),
        // Prisma's Decimal serializes as a string, without trailing zeros.
        "NUMERIC" => json!(row.try_get::<Decimal, _>(index)?.normalize().to_string()),
        "TIMESTAMPTZ" => json!(js::date(row.try_get::<DateTime<Utc>, _>(index)?)),
        "TIMESTAMP" => json!(js::date(row.try_get::<NaiveDateTime, _>(index)?.and_utc())),
        "DATE" => {
          json!(js::date(
            row
              .try_get::<NaiveDate, _>(index)?
              .and_hms_opt(0, 0, 0)
              .expect("midnight exists")
              .and_utc()
          ))
        }
        "UUID" => json!(row.try_get::<Uuid, _>(index)?),
        "JSON" | "JSONB" => row.try_get::<Value, _>(index)?,
        "TEXT[]" | "VARCHAR[]" => json!(row.try_get::<Vec<Option<String>>, _>(index)?),
        _ => json!(row.try_get::<String, _>(index)?),
      }
    };
    object.insert(column.name().to_owned(), value);
  }

  Ok(object)
}

/// Upstream's `getDateSQL`: the bucket of `field` in the request's time zone.
pub fn date_sql(field: &str, filters: &QueryFilters) -> String {
  format!(
    "to_char(date_trunc('{unit}', {field} at time zone {{{{tz}}}}), '{format}')",
    unit = filters.unit.as_str(),
    format = filters.unit.sql_format(),
  )
}

/// Upstream's `getTimestampDiffSQL`: whole seconds from `start` to `end`.
pub fn timestamp_diff_sql(start: &str, end: &str) -> String {
  format!("floor(extract(epoch from ({end} - {start})))")
}

/// SQL fragments and parameters for a query's filters (upstream's `parseFilters`).
#[derive(Default)]
pub struct Parsed {
  pub join_session_query: &'static str,
  pub date_query: &'static str,
  pub filter_query: String,
  pub cohort_query: String,
  pub exclude_bounce_query: &'static str,
  pub bindings: Bindings,
}

/// Upstream's `parseFilters`, with `joinSession` forcing the session join.
pub fn parse_filters(website_id: Uuid, filters: &QueryFilters, join_session: bool) -> AppResult<Parsed> {
  let columns = filters.column_filters()?;
  let mut bindings = Bindings::default();
  bindings.set("websiteId", Param::Uuid(website_id));
  bindings.set("startDate", Param::Time(filters.start_date));
  bindings.set("endDate", Param::Time(filters.end_date));
  bindings.set("tz", Param::Text(time_zone(filters).to_owned()));
  if let Some(timezone) = &filters.timezone {
    bindings.set("timezone", Param::Text(timezone.clone()));
  }
  bind_columns(&mut bindings, &columns);

  let mut cohort_query = String::new();
  if let Some(cohort) = &filters.cohort {
    let cohort_columns = cohort.column_filters()?;
    bind_columns(&mut bindings, &cohort_columns);
    bindings.set("cohort_startDate", Param::Time(cohort.start_date));
    bindings.set("cohort_endDate", Param::Time(cohort.end_date));

    let filter_query = filter_query(&cohort_columns, cohort.match_any, Some(&cohort.action_key));
    cohort_query = format!(
      "join
    (select distinct website_event.session_id
    from website_event
    join session on session.session_id = website_event.session_id
      and session.website_id = website_event.website_id
    where website_event.website_id = {{{{websiteId}}}}
      and website_event.created_at between {{{{cohort_startDate}}}} and {{{{cohort_endDate}}}}
      {filter_query}
    ) cohort
    on cohort.session_id = website_event.session_id
    "
    );
  }

  let (event_sql, event_bindings) = event_property_filter_query(&filters.event_property_filters, filters);
  let (session_sql, session_bindings) = session_property_filter_query(&filters.session_property_filters, filters);
  bindings.extend(event_bindings);
  bindings.extend(session_bindings);

  let joins_session = join_session
    || filters
      .names()
      .any(|name| name == "referrer" || SESSION_COLUMNS.contains(&name));

  Ok(Parsed {
    join_session_query: if joins_session {
      "inner join session on website_event.session_id = session.session_id and website_event.website_id = session.website_id"
    } else {
      ""
    },
    date_query: "and website_event.created_at between {{startDate}} and {{endDate}}",
    filter_query: [filter_query(&columns, filters.match_any, None), event_sql, session_sql]
      .into_iter()
      .filter(|part| !part.is_empty())
      .collect::<Vec<_>>()
      .join("\n"),
    cohort_query,
    exclude_bounce_query: if filters.exclude_bounce {
      EXCLUDE_BOUNCE_QUERY
    } else {
      ""
    },
    bindings,
  })
}

/// The zone for `at time zone`: the request's, or UTC.
pub fn time_zone(filters: &QueryFilters) -> &str {
  filters
    .timezone
    .as_deref()
    .filter(|tz| !tz.eq_ignore_ascii_case("utc"))
    .unwrap_or("UTC")
}

const EXCLUDE_BOUNCE_QUERY: &str = "join
    (select session_id, visit_id
    from website_event
    where website_id = {{websiteId}}
      and created_at between {{startDate}} and {{endDate}}
      and event_type != 5
    group by session_id, visit_id
    having sum(case when event_type NOT IN (2, 5) then 1 else 0 end) > 1
      or (
        sum(case when event_type NOT IN (2, 5) then 1 else 0 end) = 1
        and sum(case when event_type = 2 then 1 else 0 end) > 0
      )
    ) excludeBounce
    on excludeBounce.session_id = website_event.session_id
      and excludeBounce.visit_id = website_event.visit_id
    ";

/// Upstream's `getQueryParams`: each filter's parameter, named by its key.
fn bind_columns(bindings: &mut Bindings, columns: &[ColumnFilter]) {
  for filter in columns {
    let param = match &filter.values {
      Values::List(values) => Param::Texts(values.clone()),
      Values::Numbers(values) => Param::Numbers(values.clone()),
      Values::One(value) if matches!(filter.operator, Operator::Contains | Operator::DoesNotContain) => {
        Param::Text(format!("%{value}%"))
      }
      Values::One(value) => Param::Text(value.clone()),
    };
    bindings.set(filter.key.clone(), param);
  }
}

/// Upstream's `getFilterQuery`. With `match any`, filters are OR-ed, except the
/// event type and a cohort's action, which always apply.
fn filter_query(columns: &[ColumnFilter], match_any: bool, cohort_action: Option<&str>) -> String {
  let mut ors = Vec::new();
  let mut ands = Vec::new();

  for filter in columns {
    let name = filter.name.strip_prefix("cohort_").unwrap_or(&filter.name);
    let Some(column) = filter_column(name) else {
      continue;
    };
    let clause = map_filter(column, filter.operator, name, &filter.key);
    let always = filter.name == "eventType"
      || cohort_action.is_some_and(|action| super::filters::base_name(action) == filter.name);

    if match_any && !always {
      ors.push(clause);
    } else {
      ands.push(format!("and {clause}"));
    }

    if filter.name == "referrer" {
      ands.push(
                "and (website_event.referrer_domain != regexp_replace(website_event.hostname, '^www.', '') or website_event.referrer_domain is null)".to_owned(),
            );
    }
  }

  let mut parts = Vec::new();
  if !ors.is_empty() {
    parts.push(format!("and (\n  {}\n)", ors.join("\n  or ")));
  }
  parts.extend(ands);
  parts.join("\n")
}

/// Upstream's `mapFilter`. Only the operators `ColumnFilter` accepts reach here.
fn map_filter(column: &str, operator: Operator, name: &str, param: &str) -> String {
  let table = if SESSION_COLUMNS.contains(&name) {
    "session"
  } else {
    "website_event"
  };
  let value = format!("{{{{{param}}}}}");

  match operator {
    Operator::Equals => format!("{table}.{column} = ANY({value})"),
    Operator::NotEquals => format!("{table}.{column} != ALL({value})"),
    Operator::Contains => format!("{table}.{column} ilike {value}"),
    Operator::DoesNotContain => format!("{table}.{column} not ilike {value}"),
    Operator::Regex => format!("{table}.{column} ~* {value}"),
    Operator::NotRegex => format!("{table}.{column} !~* {value}"),
    _ => unreachable!("rejected when parsing the filter"),
  }
}

/// The condition one property filter puts on a data row, binding its value as
/// `value_param`; `None` when the filter has nothing to match (upstream skips it).
fn property_condition(
  filter: &PropertyFilter,
  value_param: &str,
  filters: &QueryFilters,
  bindings: &mut Bindings,
) -> Option<String> {
  let value = &filter.value;
  let param = format!("{{{{{value_param}}}}}");

  let condition = match filter.data_type {
    DATA_TYPE_NUMBER => {
      let col = "cast(number_value as decimal)";
      bindings.set(value_param, Param::Number(parse_float(value)));
      let param = format!("{{{{{value_param}::numeric}}}}");
      let op = match filter.operator {
        Operator::NotEquals => "!=",
        Operator::GreaterThan => ">",
        Operator::LessThan => "<",
        Operator::GreaterThanEquals => ">=",
        Operator::LessThanEquals => "<=",
        _ => "=",
      };
      format!("{col} {op} {param}")
    }
    DATA_TYPE_DATE => {
      if value.is_empty() {
        return None;
      }
      bindings.set(value_param, Param::Text(value.clone()));
      let date_col = if filters.timezone.is_some() && time_zone(filters) != "UTC" {
        "(date_value at time zone {{timezone}})::date"
      } else {
        "(date_value at time zone 'utc')::date"
      };
      let param = format!("{{{{{value_param}::date}}}}");
      let op = match filter.operator {
        Operator::Before => "<",
        Operator::After => ">",
        _ => "=",
      };
      format!("{date_col} {op} {param}")
    }
    DATA_TYPE_ARRAY => {
      if value.is_empty() {
        return None;
      }
      bindings.set(value_param, Param::Text(value.clone()));
      let negate = if filter.operator == Operator::Contains {
        ""
      } else {
        "not "
      };
      format!(
        "{negate}exists (
                select 1
                from jsonb_array_elements_text(coalesce(string_value, '[]')::jsonb) as array_item(value)
                where array_item.value = {param}
              )"
      )
    }
    _ => {
      match filter.operator {
        Operator::Equals | Operator::NotEquals => {
          let values: Vec<String> = value.split(',').filter(|v| !v.is_empty()).map(str::to_owned).collect();
          if values.is_empty() {
            return None;
          }
          bindings.set(value_param, Param::Texts(values));
          if filter.operator == Operator::Equals {
            format!("string_value = ANY({param})")
          } else {
            format!("string_value != ALL({param})")
          }
        }
        Operator::Regex | Operator::NotRegex => {
          if value.is_empty() {
            return None;
          }
          bindings.set(value_param, Param::Text(value.clone()));
          let op = if filter.operator == Operator::Regex {
            "~*"
          } else {
            "!~*"
          };
          format!("string_value {op} {param}")
        }
        _ => {
          if value.is_empty() {
            return None;
          }
          bindings.set(value_param, Param::Text(format!("%{value}%")));
          let op = if filter.operator == Operator::Contains {
            "ilike"
          } else {
            "not ilike"
          };
          format!("string_value {op} {param}")
        }
      }
    }
  };

  Some(condition)
}

/// The `having` clauses for property filters, and the keys they look up.
fn property_clauses(
  list: &[PropertyFilter],
  prefix: &str,
  filters: &QueryFilters,
  bindings: &mut Bindings,
) -> (Vec<String>, Vec<String>) {
  let mut clauses = Vec::new();
  let mut keys = Vec::new();

  for (index, filter) in list.iter().enumerate() {
    let key_param = format!("{prefix}_key_{index}");
    bindings.set(&key_param, Param::Text(filter.name.clone()));
    keys.push(format!("{{{{{key_param}}}}}"));

    let value_param = format!("{prefix}_val_{index}");
    if let Some(condition) = property_condition(filter, &value_param, filters, bindings) {
      clauses.push(format!(
        "bool_or(data_key = {{{{{key_param}}}}} and data_type = {} and {condition})",
        filter.data_type
      ));
    }
  }

  (clauses, keys)
}

/// Upstream's `getEventPropertyFilterQuery`: events with all the given data.
fn event_property_filter_query(list: &[PropertyFilter], filters: &QueryFilters) -> (String, Bindings) {
  let mut bindings = Bindings::default();
  let (clauses, keys) = property_clauses(list, "epf", filters, &mut bindings);
  if clauses.is_empty() {
    return (String::new(), Bindings::default());
  }

  let sql = format!(
    "and website_event.event_id in (
      select website_event_id
      from event_data
      where website_id = {{{{websiteId::uuid}}}}
        and created_at between {{{{startDate}}}} and {{{{endDate}}}}
        and data_key in ({keys})
      group by website_event_id
      having {clauses}
    )",
    keys = keys.join(", "),
    clauses = clauses.join("\n        and "),
  );

  (sql, bindings)
}

/// Upstream's `getSessionPropertyFilterQuery`: sessions with all the given data.
fn session_property_filter_query(list: &[PropertyFilter], filters: &QueryFilters) -> (String, Bindings) {
  let mut bindings = Bindings::default();
  let (clauses, keys) = property_clauses(list, "spf", filters, &mut bindings);
  if clauses.is_empty() {
    return (String::new(), Bindings::default());
  }

  let sql = format!(
    "and exists (
      select 1
      from session_data
      where website_id = website_event.website_id
        and session_id = website_event.session_id
        and data_key in ({keys})
      group by website_id, session_id
      having {clauses}
    )",
    keys = keys.join(", "),
    clauses = clauses.join("\n        and "),
  );

  (sql, bindings)
}

/// Whose data a data-explorer filter looks at.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DataOf {
  Event,
  Session,
}

/// Upstream's `getPropertyFilterQuery`: one condition per `pf_` filter, each
/// requiring the event (or session) to have that data. Binds the filters'
/// keys and values into `bindings`.
pub fn data_filter_query(
  list: &[PropertyFilter],
  of: DataOf,
  filters: &QueryFilters,
  bindings: &mut Bindings,
) -> String {
  let mut parts = Vec::new();

  for (index, filter) in list.iter().enumerate() {
    let key_param = format!("pf_key_{index}");
    bindings.set(&key_param, Param::Text(filter.name.clone()));
    let Some(condition) = property_condition(filter, &format!("pf_val_{index}"), filters, bindings) else {
      continue;
    };
    let data_type = filter.data_type;

    parts.push(match of {
      DataOf::Session => {
        format!(
          "and exists (
      select 1
      from session_data
      where website_id = website_event.website_id
        and session_id = website_event.session_id
        and data_key = {{{{{key_param}}}}}
        and data_type = {data_type}
        and {condition}
    )"
        )
      }
      DataOf::Event => {
        format!(
          "and website_event.event_id in (
      select website_event_id
      from event_data
      where website_id = {{{{websiteId::uuid}}}}
        and created_at between {{{{startDate}}}} and {{{{endDate}}}}
        and data_key = {{{{{key_param}}}}}
        and data_type = {data_type}
        and {condition}
    )"
        )
      }
    });
  }

  parts.join("\n")
}

/// JavaScript's `parseFloat(value) || 0`: the longest numeric prefix, else 0.
fn parse_float(value: &str) -> f64 {
  static NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[+-]?(Infinity|(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?)").expect("valid regex"));

  let text = value.trim_start_matches(js::is_js_whitespace);
  let number = NUMBER.find(text).map_or(f64::NAN, |m| {
    let matched = m.as_str();
    match matched.trim_start_matches(['+', '-']) {
      "Infinity" if matched.starts_with('-') => f64::NEG_INFINITY,
      "Infinity" => f64::INFINITY,
      _ => matched.parse().unwrap_or(f64::NAN),
    }
  });

  if number.is_nan() || number == 0.0 { 0.0 } else { number }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn renders_placeholders_in_order() {
    let mut bindings = Bindings::default();
    bindings.set("a", Param::Text("x".into()));
    bindings.set("b", Param::Uuid(Uuid::nil()));

    let (sql, _) = render("select {{a}}, {{ b::uuid }}, {{a}}", &bindings).unwrap();
    assert_eq!(sql, "select $1, $2::uuid, $3");

    assert!(render("select {{missing}}", &bindings).is_err());
  }

  #[test]
  fn parses_floats_like_javascript() {
    assert_eq!(parse_float("12.5kg"), 12.5);
    assert_eq!(parse_float("  -3e2"), -300.0);
    assert_eq!(parse_float(".5"), 0.5);
    assert_eq!(parse_float("abc"), 0.0);
    assert_eq!(parse_float("-Infinity"), f64::NEG_INFINITY);
  }

  #[test]
  fn groups_any_match_filters() {
    let filter = |key: &str, name: &str, operator| {
      ColumnFilter {
        key: key.into(),
        name: name.into(),
        operator,
        values: Values::List(vec![]),
      }
    };
    let columns = [
      filter("browser", "browser", Operator::Equals),
      filter("path", "path", Operator::Contains),
      filter("eventType", "eventType", Operator::Equals),
    ];

    assert_eq!(
      filter_query(&columns, true, None),
      "and (\n  session.browser = ANY({{browser}})\n  or website_event.url_path ilike {{path}}\n)\n\
             and website_event.event_type = ANY({{eventType}})"
    );
    assert_eq!(
      filter_query(&columns[..1], false, None),
      "and session.browser = ANY({{browser}})"
    );
  }
}
