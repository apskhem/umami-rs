//! Event and session data: the custom properties sent with events and
//! `identify` calls, and the data explorer's pivots and series. Ported from
//! upstream's `getEventData*` and `getSessionData*` queries.
//!
//! Lists that upstream orders only partly break ties by the remaining
//! columns, so results are stable.

use serde_json::{
  Map,
  Value,
  json,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::filters::{
  DATA_TYPE_ARRAY,
  FilterValue,
  PropertyFilter,
  QueryFilters,
};
use super::sql::{
  self,
  Bindings,
  DataOf,
  Param,
  Parsed,
  data_filter_query,
  date_sql,
  parse_filters,
};
use crate::error::AppResult;

/// Upstream's `EVENT_TYPE.performance`, which session data ignores.
const PERFORMANCE: i32 = 5;

/// A metric of numeric data over time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
  Sum,
  Avg,
  Count,
}

impl Metric {
  /// `z.enum(['sum', 'avg', 'count'])`.
  pub fn parse(value: &str) -> Option<Self> {
    Some(match value {
      "sum" => Self::Sum,
      "avg" => Self::Avg,
      "count" => Self::Count,
      _ => return None,
    })
  }

  /// The aggregate over `table`'s numbers; sessions are counted once each.
  fn sql(self, table: &str) -> String {
    match (self, table) {
      (Self::Avg, _) => format!("avg(cast({table}.number_value as decimal))"),
      (Self::Count, "session_data") => "count(distinct session_data.session_id)".to_owned(),
      (Self::Count, _) => "count(*)".to_owned(),
      (Self::Sum, _) => format!("sum(cast({table}.number_value as decimal))"),
    }
  }
}

/// Upstream's `getDateStringSQL(field, 'second', timezone)`.
fn date_string_sql(field: &str) -> String {
  format!("to_char({field} at time zone {{{{tz}}}}, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
}

/// Upstream's `getDateSQL(field, 'hour')` without a time zone: UTC hours.
fn utc_hour_sql(field: &str) -> String {
  format!("to_char(date_trunc('hour', {field} at time zone 'UTC'), 'YYYY-MM-DD\"T\"HH24:00:00\"Z\"')")
}

/// The filters' SQL with `extra` parameters bound, and the data explorer's
/// `pf_` filters on `of`'s data.
fn prepare(
  website_id: Uuid,
  filters: &QueryFilters,
  extra: &[(&str, &str)],
  explorer: Option<(&[PropertyFilter], DataOf)>,
) -> AppResult<(Parsed, String)> {
  let mut p = parse_filters(website_id, filters, false)?;
  for (name, value) in extra {
    p.bindings.set(*name, Param::Text((*value).to_owned()));
  }
  let pf = match explorer {
    Some((list, of)) => data_filter_query(list, of, filters, &mut p.bindings),
    None => String::new(),
  };

  Ok((p, pf))
}

/// The value of a data row as text, by its type (the pivots' columns).
fn value_text(table: &str, date: &str) -> String {
  format!(
    "coalesce(
          case when {table}.data_type = 1 then {table}.string_value end,
          case when {table}.data_type = 2 then cast({table}.number_value as varchar) end,
          case when {table}.data_type = 3 then {table}.string_value end,
          case when {table}.data_type = 4 then {date} end,
          case when {table}.data_type = 5 then {table}.string_value end,
          ''
        )"
  )
}

// Event data

/// Upstream's `getEventData`: the events with data, newest first and paged,
/// each with its properties (the route's grouping).
pub async fn event_data(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Value> {
  let (p, _) = prepare(website_id, filters, &[], None)?;
  let (size, offset) = sql::page_window(filters);

  let events = format!(
    r#"
    select website_event.event_id
    from website_event
    join event_data on event_data.website_event_id = website_event.event_id
      and event_data.website_id = {{{{websiteId::uuid}}}}
      and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
    {cohort}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      {filter}
    group by website_event.event_id
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  let count = sql::row(db, &format!("select count(*) as num from ({events}) t"), &p.bindings).await?["num"]
    .as_i64()
    .unwrap_or_default();

  let rows = sql::rows(
    db,
    &format!(
      r#"
    with paged_events as (
      {events}
      order by max(website_event.created_at) desc, website_event.event_id
      limit {size} offset {offset}
    )
    select
      event_data.website_id as "websiteId",
      event_data.website_event_id as "eventId",
      website_event.event_name as "eventName",
      event_data.data_key as "dataKey",
      event_data.string_value as "stringValue",
      event_data.number_value as "numberValue",
      event_data.date_value as "dateValue",
      event_data.data_type as "dataType",
      event_data.created_at as "createdAt"
    from event_data
    join website_event on website_event.event_id = event_data.website_event_id
      and website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
    join paged_events on paged_events.event_id = event_data.website_event_id
    where event_data.website_id = {{{{websiteId::uuid}}}}
      and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
    order by event_data.created_at desc, event_data.website_event_id, event_data.data_key
    "#
    ),
    &p.bindings,
  )
  .await?;

  // One entry per event, in first-seen order, with its properties.
  let mut events: Vec<Map<String, Value>> = Vec::new();
  for row in rows {
    let Value::Object(mut row) = row else { continue };
    let (website, event, name) = (
      row.shift_remove("websiteId").unwrap_or_default(),
      row.shift_remove("eventId").unwrap_or_default(),
      row.shift_remove("eventName").unwrap_or_default(),
    );
    let index = match events.iter().position(|entry| entry["eventId"] == event) {
      Some(index) => index,
      None => {
        let mut entry = Map::new();
        entry.insert("websiteId".to_owned(), website);
        entry.insert("eventId".to_owned(), event);
        entry.insert("eventName".to_owned(), name);
        entry.insert("eventProperties".to_owned(), json!([]));
        events.push(entry);
        events.len() - 1
      }
    };
    if let Some(Value::Array(properties)) = events[index].get_mut("eventProperties") {
      properties.push(Value::Object(row));
    }
  }

  Ok(json!({
      "data": events,
      "count": count,
      "page": filters.page.unwrap_or(1),
      "pageSize": size,
  }))
}

/// Upstream's `getEventDataById`: the event's data.
pub async fn event_data_by_id(db: &PgPool, website_id: Uuid, event_id: Uuid) -> AppResult<Vec<Value>> {
  let mut bindings = Bindings::default();
  bindings.set("websiteId", Param::Uuid(website_id));
  bindings.set("eventId", Param::Uuid(event_id));

  sql::rows(
    db,
    r#"
    select event_data.website_id as "websiteId",
       event_data.website_event_id as "eventId",
       website_event.event_name as "eventName",
       event_data.data_key as "dataKey",
       event_data.string_value as "stringValue",
       event_data.number_value as "numberValue",
       event_data.date_value as "dateValue",
       event_data.data_type as "dataType",
       event_data.created_at as "createdAt"
    from event_data
    join website_event on website_event.event_id = event_data.website_event_id
      and website_event.website_id = {{websiteId::uuid}}
    where event_data.website_id = {{websiteId::uuid}}
      and event_data.website_event_id = {{eventId::uuid}}
    order by event_data.data_key
    "#,
    &bindings,
  )
  .await
}

/// Upstream's `getEventDataEvents`: properties per event name, with their
/// values when an `event` filter is given.
pub async fn event_data_events(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Vec<Value>> {
  let (p, _) = prepare(website_id, filters, &[], None)?;
  let by_value = matches!(filters.get("event"), Some(FilterValue::Text(event)) if !event.is_empty());

  let (value_column, value_group, order) = if by_value {
    (
      r#"event_data.string_value as "propertyValue","#,
      ", event_data.string_value",
      "order by 1 asc, 2 asc, 3 asc, 5 desc, 4 asc",
    )
  } else {
    ("", "", "order by 1 asc, 2 asc, 3 asc\n    limit 500")
  };

  let query = format!(
    r#"
    select
      website_event.event_name as "eventName",
      event_data.data_key as "propertyName",
      event_data.data_type as "dataType",
      {value_column}
      count(*) as "total"
    from event_data
    inner join website_event
      on website_event.event_id = event_data.website_event_id
    {cohort}
    {join}
    where event_data.website_id = {{{{websiteId::uuid}}}}
      and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
    {filter}
    group by website_event.event_name, event_data.data_key, event_data.data_type{value_group}
    {order}
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// The join on the period's events that most event-data queries share.
const EVENT_JOIN: &str = "join website_event on website_event.event_id = event_data.website_event_id
      and website_event.website_id = {{websiteId::uuid}}
      and website_event.created_at between {{startDate}} and {{endDate}}";

/// Upstream's `getEventDataFields`: properties by how often they occur,
/// optionally for one event name.
pub async fn event_data_fields(
  db: &PgPool,
  website_id: Uuid,
  event_name: Option<&str>,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let event_name = event_name.filter(|name| !name.is_empty());
  let (p, _) = prepare(
    website_id,
    filters,
    &[("eventName", event_name.unwrap_or_default())],
    None,
  )?;
  let name_filter = if event_name.is_some() {
    "and website_event.event_name = {{eventName}}"
  } else {
    ""
  };

  let query = format!(
    r#"
    select
      data_key as "propertyName",
      data_type as "dataType",
      count(*) as "total"
    from event_data
    {EVENT_JOIN}
      {name_filter}
    {cohort}
    {join}
    where event_data.website_id = {{{{websiteId::uuid}}}}
      and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
    {filter}
    group by data_key, data_type
    order by "total" desc, "propertyName" asc, "dataType" asc
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getEventDataProperties`: properties per event name, most
/// frequent first.
pub async fn event_data_properties(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Vec<Value>> {
  let (p, _) = prepare(website_id, filters, &[], None)?;

  let query = format!(
    r#"
    select
      website_event.event_name as "eventName",
      event_data.data_key as "propertyName",
      event_data.data_type as "dataType",
      count(*) as "total"
    from event_data
    {EVENT_JOIN}
    {cohort}
    {join}
    where event_data.website_id = {{{{websiteId::uuid}}}}
      and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
    {filter}
    group by website_event.event_name, event_data.data_key, event_data.data_type
    order by 4 desc, 1 asc, 2 asc, 3 asc
    limit 500
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getEventDataStats`: events with data, distinct properties and
/// data records.
pub async fn event_data_stats(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Value> {
  let (p, _) = prepare(website_id, filters, &[], None)?;

  let query = format!(
    r#"
    select
      count(distinct t.website_event_id) as "events",
      count(distinct t.data_key) as "properties",
      sum(t.total) as "records"
    from (
      select
        website_event_id,
        data_key,
        count(*) as "total"
      from event_data
      {EVENT_JOIN}
      {cohort}
      {join}
      where event_data.website_id = {{{{websiteId::uuid}}}}
        and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      {filter}
      group by website_event_id, data_key
      ) as t
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::row(db, &query, &p.bindings).await
}

/// The parameters of a values list: the property, and optionally its type.
pub struct ValuesOf<'a> {
  pub property_name: &'a str,
  /// Upstream's `dataType`; `0` means any.
  pub data_type: i64,
}

/// The `value` column of a values list: numbers without upstream's padding,
/// dates by UTC hour, the rest as stored.
fn value_column(table: &str) -> String {
  format!(
    "case
        when {table}.data_type = 2 then replace({table}.string_value, '.0000', '')
        when {table}.data_type = 4 then {date}
        else {table}.string_value
      end",
    date = utc_hour_sql(&format!("{table}.date_value")),
  )
}

/// Upstream's `getEventDataValues`: a custom-event property's most frequent
/// values (array elements for arrays), optionally for one event name.
pub async fn event_data_values(
  db: &PgPool,
  website_id: Uuid,
  event_name: Option<&str>,
  of: &ValuesOf<'_>,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let event_name = event_name.filter(|name| !name.is_empty());
  let (p, _) = prepare(
    website_id,
    filters,
    &[
      ("eventName", event_name.unwrap_or_default()),
      ("propertyName", of.property_name),
    ],
    None,
  )?;
  let name_filter = if event_name.is_some() {
    "and website_event.event_name = {{eventName}}"
  } else {
    ""
  };

  let query = if of.data_type == i64::from(DATA_TYPE_ARRAY) {
    format!(
      r#"
      select
        array_item.value as "value",
        count(*) as "total"
      from event_data
      {EVENT_JOIN}
        and website_event.event_type = 2
        {name_filter}
      cross join lateral jsonb_array_elements_text(coalesce(event_data.string_value, '[]')::jsonb) as array_item(value)
      {cohort}
      {join}
      where event_data.website_id = {{{{websiteId::uuid}}}}
        and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        and event_data.data_key = {{{{propertyName}}}}
        and event_data.data_type = {DATA_TYPE_ARRAY}
      {filter}
      group by array_item.value
      order by 2 desc, 1 asc
      limit 100
      "#,
      cohort = p.cohort_query,
      join = p.join_session_query,
      filter = p.filter_query,
    )
  } else {
    let type_filter = match of.data_type {
      0 => String::new(),
      data_type => format!("and event_data.data_type = {data_type}"),
    };
    format!(
      r#"
    select
      {value} as "value",
      count(*) as "total"
    from event_data
    {EVENT_JOIN}
      and website_event.event_type = 2
      {name_filter}
    {cohort}
    {join}
    where event_data.website_id = {{{{websiteId::uuid}}}}
      and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and event_data.data_key = {{{{propertyName}}}}
      {type_filter}
    {filter}
    group by 1
    order by 2 desc, 1 asc
    limit 100
    "#,
      value = value_column("event_data"),
      cohort = p.cohort_query,
      join = p.join_session_query,
      filter = p.filter_query,
    )
  };

  sql::rows(db, &query, &p.bindings).await
}

/// The join on one custom event name's events in the period that the event
/// explorer's queries share.
const EXPLORER_EVENT_JOIN: &str = "join website_event on website_event.event_id = event_data.website_event_id
      and website_event.website_id = {{websiteId::uuid}}
      and website_event.created_at between {{startDate}} and {{endDate}}
      and website_event.event_type = 2
      and website_event.event_name = {{eventName}}";

/// The event explorer's query parameters: the event, and its `pf_` filters.
pub struct EventExplorer<'a> {
  pub event_name: &'a str,
  pub filters: &'a [PropertyFilter],
}

/// Upstream's `getEventDataPivot`: the event name's events with data, newest
/// first and paged, each with its properties as parallel key and value lists.
pub async fn event_data_pivot(
  db: &PgPool,
  website_id: Uuid,
  explorer: &EventExplorer<'_>,
  filters: &QueryFilters,
) -> AppResult<Value> {
  let (p, pf) = prepare(
    website_id,
    filters,
    &[("eventName", explorer.event_name)],
    Some((explorer.filters, DataOf::Event)),
  )?;

  let query = format!(
    r#"
    with paged_events as (
      select website_event.event_id, max(website_event.created_at) as sort_created_at
      from website_event
      join event_data on event_data.website_event_id = website_event.event_id
        and event_data.website_id = {{{{websiteId::uuid}}}}
        and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      {cohort}
      {join}
      where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        and website_event.event_name = {{{{eventName}}}}
        {filter}
        {pf}
      group by website_event.event_id
    )
    select
      website_event.event_id as "eventId",
      website_event.session_id as "sessionId",
      website_event.event_name as "eventName",
      website_event.url_path as "urlPath",
      max(website_event.created_at) as "createdAt",
      array_agg(event_data.data_key order by event_data.data_key asc) as "propertyKeys",
      array_agg(
        {value}
        order by event_data.data_key asc
      ) as "propertyValues"
    from event_data
    join website_event on website_event.event_id = event_data.website_event_id
      and website_event.website_id = {{{{websiteId::uuid}}}}
    join paged_events on paged_events.event_id = event_data.website_event_id
    where event_data.website_id = {{{{websiteId::uuid}}}}
      and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
    group by
      website_event.event_id,
      website_event.session_id,
      website_event.event_name,
      website_event.url_path,
      paged_events.sort_created_at
    order by paged_events.sort_created_at desc, website_event.event_id
    "#,
    value = value_text("event_data", &date_string_sql("event_data.date_value")),
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::paged_as_ordered(db, &query, &p.bindings, filters).await
}

/// The event explorer's series and stats: `select` over one property of the
/// event name's events, restricted to `data_types`, with `tail` after the
/// filters.
async fn event_explorer_query(
  db: &PgPool,
  website_id: Uuid,
  explorer: &EventExplorer<'_>,
  property_name: &str,
  filters: &QueryFilters,
  (select, data_types, extra_join, tail): (&str, &str, &str, &str),
) -> AppResult<Vec<Value>> {
  let (p, pf) = prepare(
    website_id,
    filters,
    &[("eventName", explorer.event_name), ("propertyName", property_name)],
    Some((explorer.filters, DataOf::Event)),
  )?;

  let query = format!(
    r#"
    select
      {select}
    from event_data
    {EXPLORER_EVENT_JOIN}
    {extra_join}
    {cohort}
    {join}
    where event_data.website_id = {{{{websiteId::uuid}}}}
      and event_data.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and event_data.data_key = {{{{propertyName}}}}
      and event_data.data_type {data_types}
      {filter}
      {pf}
    {tail}
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// `cross join` over the elements of an array property of `table`.
fn array_elements(table: &str) -> String {
  format!(
    "cross join lateral jsonb_array_elements_text(coalesce({table}.string_value, '[]')::jsonb) as array_item(value)"
  )
}

/// Upstream's `getEventDataArraySeries`: occurrences of each element of an
/// array property per time bucket.
pub async fn event_data_array_series(
  db: &PgPool,
  website_id: Uuid,
  explorer: &EventExplorer<'_>,
  property_name: &str,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let select = format!(
    "array_item.value as x,\n      {} as t,\n      count(*) as y",
    date_sql("event_data.created_at", filters)
  );
  let parts = (
    select.as_str(),
    "= 5",
    &*array_elements("event_data"),
    "group by 1, 2\n    order by 2, 1",
  );
  event_explorer_query(db, website_id, explorer, property_name, filters, parts).await
}

/// Upstream's `getEventDataDateSeries`: occurrences of each value of a date
/// property.
pub async fn event_data_date_series(
  db: &PgPool,
  website_id: Uuid,
  explorer: &EventExplorer<'_>,
  property_name: &str,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let select = format!(
    "{} as t,\n      count(*) as y",
    date_string_sql("event_data.date_value")
  );
  let parts = (select.as_str(), "= 4", "", "group by 1\n    order by 1");
  event_explorer_query(db, website_id, explorer, property_name, filters, parts).await
}

/// Upstream's `getEventDataNumericSeries`: a number property's sum, average or
/// count per time bucket.
pub async fn event_data_numeric_series(
  db: &PgPool,
  website_id: Uuid,
  explorer: &EventExplorer<'_>,
  property_name: &str,
  metric: Metric,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let select = format!(
    "{} t,\n      {} y",
    date_sql("event_data.created_at", filters),
    metric.sql("event_data")
  );
  let parts = (select.as_str(), "= 2", "", "group by 1\n    order by 1");
  event_explorer_query(db, website_id, explorer, property_name, filters, parts).await
}

/// The numeric stats columns over `table`'s numbers.
fn numeric_stats(table: &str) -> String {
  format!(
    r#"coalesce(sum(cast({table}.number_value as decimal)), 0) as "total",
      coalesce(avg(cast({table}.number_value as decimal)), 0) as "average",
      coalesce(percentile_cont(0.5) within group (order by {table}.number_value), 0) as "median",
      coalesce(max({table}.number_value), 0) as "max",
      coalesce(min({table}.number_value), 0) as "min""#
  )
}

/// Upstream's `getEventDataNumericStats`: total, average, median, maximum and
/// minimum of a number property.
pub async fn event_data_numeric_stats(
  db: &PgPool,
  website_id: Uuid,
  explorer: &EventExplorer<'_>,
  property_name: &str,
  filters: &QueryFilters,
) -> AppResult<Value> {
  let select = numeric_stats("event_data");
  let parts = (select.as_str(), "= 2", "", "");
  let rows = event_explorer_query(db, website_id, explorer, property_name, filters, parts).await?;

  Ok(rows.into_iter().next().unwrap_or(Value::Null))
}

/// Upstream's `getEventDataPropertySeries`: occurrences of each value of a
/// string or boolean property per time bucket.
pub async fn event_data_property_series(
  db: &PgPool,
  website_id: Uuid,
  explorer: &EventExplorer<'_>,
  property_name: &str,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let select = format!(
    "event_data.string_value as x,\n      {} t,\n      count(*) y",
    date_sql("event_data.created_at", filters)
  );
  let parts = (select.as_str(), "in (1, 3)", "", "group by 1, 2\n    order by 2, 1");
  event_explorer_query(db, website_id, explorer, property_name, filters, parts).await
}

// Session data

/// The sessions active in the period that match the filters (and the
/// explorer's), as the session-data queries' first CTE.
fn filtered_sessions(p: &Parsed, pf: &str, columns: &str) -> String {
  format!(
    "filtered_sessions as (
      select distinct {columns}
      from website_event
      {cohort}
      {join}
      where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        and website_event.event_type != {PERFORMANCE}
        {filter}
        {pf}
    )",
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  )
}

/// Upstream's `getSessionDataPivot`: the sessions with the property, newest
/// first and paged, each with its latest value of every property.
pub async fn session_data_pivot(
  db: &PgPool,
  website_id: Uuid,
  property_name: &str,
  explorer: &[PropertyFilter],
  filters: &QueryFilters,
) -> AppResult<Value> {
  let (p, pf) = prepare(
    website_id,
    filters,
    &[("propertyName", property_name)],
    Some((explorer, DataOf::Session)),
  )?;

  let query = format!(
    r#"
    with {sessions},
    latest_session_properties as (
      select
        ranked.session_id,
        ranked.distinct_id,
        ranked.created_at,
        ranked.data_key,
        ranked.data_type,
        ranked.string_value,
        ranked.number_value,
        ranked.date_value
      from (
        select
          session_data.session_id,
          session_data.distinct_id,
          session_data.created_at,
          session_data.data_key,
          session_data.data_type,
          session_data.string_value,
          session_data.number_value,
          session_data.date_value,
          row_number() over (
            partition by session_data.session_id, session_data.data_key
            order by session_data.created_at desc, session_data.session_data_id desc
          ) as row_num
        from session_data
        join filtered_sessions
          on filtered_sessions.session_id = session_data.session_id
        where session_data.website_id = {{{{websiteId::uuid}}}}
      ) ranked
      where ranked.row_num = 1
    ),
    paged_sessions as (
      select
        latest_session_properties.session_id,
        latest_session_properties.created_at as sort_created_at
      from latest_session_properties
      where latest_session_properties.data_key = {{{{propertyName}}}}
    )
    select
      latest_session_properties.session_id as "sessionId",
      coalesce(max(latest_session_properties.distinct_id), '') as "distinctId",
      max(latest_session_properties.created_at) as "createdAt",
      array_agg(latest_session_properties.data_key order by latest_session_properties.data_key asc) as "propertyKeys",
      array_agg(
        {value}
        order by latest_session_properties.data_key asc
      ) as "propertyValues"
    from latest_session_properties
    join paged_sessions
      on paged_sessions.session_id = latest_session_properties.session_id
    group by latest_session_properties.session_id, paged_sessions.sort_created_at
    order by paged_sessions.sort_created_at desc, latest_session_properties.session_id
    "#,
    sessions = filtered_sessions(&p, &pf, "website_event.session_id"),
    value = value_text(
      "latest_session_properties",
      &date_string_sql("latest_session_properties.date_value")
    ),
  );

  sql::paged_as_ordered(db, &query, &p.bindings, filters).await
}

/// The session explorer's queries that read session data through the
/// period's events: `select` over one property, restricted to `data_types`,
/// with `tail` after the filters.
async fn session_events_query(
  db: &PgPool,
  website_id: Uuid,
  property_name: &str,
  explorer: &[PropertyFilter],
  filters: &QueryFilters,
  (select, data_types, extra_join, tail): (&str, &str, &str, &str),
) -> AppResult<Vec<Value>> {
  let (p, pf) = prepare(
    website_id,
    filters,
    &[("propertyName", property_name)],
    Some((explorer, DataOf::Session)),
  )?;

  let query = format!(
    r#"
    select
      {select}
    from website_event
    {cohort}
    {join}
    join session_data
      on session_data.session_id = website_event.session_id
        and session_data.website_id = website_event.website_id
    {extra_join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and website_event.event_type != {PERFORMANCE}
      and session_data.data_key = {{{{propertyName}}}}
      and session_data.data_type {data_types}
      {filter}
      {pf}
    {tail}
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getSessionDataArraySeries`: sessions with each element of an
/// array property per time bucket.
pub async fn session_data_array_series(
  db: &PgPool,
  website_id: Uuid,
  property_name: &str,
  explorer: &[PropertyFilter],
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let select = format!(
    "array_item.value as x,\n      {} as t,\n      count(distinct session_data.session_id) as y",
    date_sql("session_data.created_at", filters)
  );
  let parts = (
    select.as_str(),
    "= 5",
    &*array_elements("session_data"),
    "group by 1, 2\n    order by 2, 1",
  );
  session_events_query(db, website_id, property_name, explorer, filters, parts).await
}

/// Upstream's `getSessionDataDateSeries`: sessions with each value of a date
/// property.
pub async fn session_data_date_series(
  db: &PgPool,
  website_id: Uuid,
  property_name: &str,
  explorer: &[PropertyFilter],
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let select = format!(
    "{} as t,\n      count(distinct session_data.session_id) as y",
    date_string_sql("session_data.date_value")
  );
  let parts = (select.as_str(), "= 4", "", "group by 1\n    order by 1");
  session_events_query(db, website_id, property_name, explorer, filters, parts).await
}

/// Upstream's `getSessionDataPropertySeries`: sessions with each value of a
/// string or boolean property per time bucket.
pub async fn session_data_property_series(
  db: &PgPool,
  website_id: Uuid,
  property_name: &str,
  explorer: &[PropertyFilter],
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let select = format!(
    "session_data.string_value as x,\n      {} t,\n      count(distinct session_data.session_id) y",
    date_sql("session_data.created_at", filters)
  );
  let parts = (select.as_str(), "in (1, 3)", "", "group by 1, 2\n    order by 2, 1");
  session_events_query(db, website_id, property_name, explorer, filters, parts).await
}

/// The session explorer's queries over the filtered sessions' data: `select`
/// over one number property, with `tail`.
async fn session_numbers_query(
  db: &PgPool,
  website_id: Uuid,
  property_name: &str,
  explorer: &[PropertyFilter],
  filters: &QueryFilters,
  (select, tail): (&str, &str),
) -> AppResult<Vec<Value>> {
  let (p, pf) = prepare(
    website_id,
    filters,
    &[("propertyName", property_name)],
    Some((explorer, DataOf::Session)),
  )?;

  let query = format!(
    r#"
    with {sessions}
    select
      {select}
    from session_data
    join filtered_sessions
      on filtered_sessions.session_id = session_data.session_id
        and filtered_sessions.website_id = session_data.website_id
    where session_data.website_id = {{{{websiteId::uuid}}}}
      and session_data.data_key = {{{{propertyName}}}}
      and session_data.data_type = 2
    {tail}
    "#,
    sessions = filtered_sessions(&p, &pf, "website_event.session_id, website_event.website_id"),
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getSessionDataNumericSeries`: a number property's sum, average
/// or session count per time bucket.
pub async fn session_data_numeric_series(
  db: &PgPool,
  website_id: Uuid,
  property_name: &str,
  metric: Metric,
  explorer: &[PropertyFilter],
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let select = format!(
    "{} t,\n      {} y",
    date_sql("session_data.created_at", filters),
    metric.sql("session_data")
  );
  let parts = (select.as_str(), "group by 1\n    order by 1");
  session_numbers_query(db, website_id, property_name, explorer, filters, parts).await
}

/// Upstream's `getSessionDataNumericStats`.
pub async fn session_data_numeric_stats(
  db: &PgPool,
  website_id: Uuid,
  property_name: &str,
  explorer: &[PropertyFilter],
  filters: &QueryFilters,
) -> AppResult<Value> {
  let select = numeric_stats("session_data");
  let rows = session_numbers_query(db, website_id, property_name, explorer, filters, (&select, "")).await?;

  Ok(rows.into_iter().next().unwrap_or(Value::Null))
}

/// Upstream's `getSessionDataProperties`: the properties of the filtered
/// sessions (those with `property_name`, if given) by how many sessions have
/// them.
pub async fn session_data_properties(
  db: &PgPool,
  website_id: Uuid,
  property_name: Option<&str>,
  explorer: &[PropertyFilter],
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let property_name = property_name.filter(|name| !name.is_empty());
  let (p, pf) = prepare(
    website_id,
    filters,
    &[("propertyName", property_name.unwrap_or_default())],
    Some((explorer, DataOf::Session)),
  )?;
  let property_filter = if property_name.is_some() {
    "where session_data.data_key = {{propertyName}}"
  } else {
    ""
  };

  let query = format!(
    r#"
    with {sessions},
    selected_property_sessions as (
      select distinct session_data.session_id, session_data.website_id
      from session_data
      join filtered_sessions
        on filtered_sessions.session_id = session_data.session_id
        and filtered_sessions.website_id = session_data.website_id
      {property_filter}
    )
    select
        data_key as "propertyName",
        data_type as "dataType",
        count(distinct session_data.session_id) as "total"
    from selected_property_sessions
    join session_data
        on session_data.session_id = selected_property_sessions.session_id
          and session_data.website_id = selected_property_sessions.website_id
    group by 1, 2
    order by 3 desc, 1 asc, 2 asc
    limit 500
    "#,
    sessions = filtered_sessions(&p, &pf, "website_event.session_id, website_event.website_id"),
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getSessionDataActivityStats`: per value of a string property,
/// its sessions and their visits, views, events and activity in the period.
pub async fn session_data_activity_stats(
  db: &PgPool,
  website_id: Uuid,
  property_name: &str,
  explorer: &[PropertyFilter],
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let (p, pf) = prepare(
    website_id,
    filters,
    &[("propertyName", property_name)],
    Some((explorer, DataOf::Session)),
  )?;

  let query = format!(
    r#"
    with {sessions},
    session_rollup as (
      select
        website_event.session_id,
        website_event.visit_id,
        count(*) as activity,
        sum(case when website_event.event_type = 1 then 1 else 0 end) as views,
        sum(case when website_event.event_type = 2 then 1 else 0 end) as events,
        min(website_event.created_at) as min_time,
        max(website_event.created_at) as max_time
      from website_event
      join filtered_sessions
        on filtered_sessions.session_id = website_event.session_id
        and filtered_sessions.website_id = website_event.website_id
      where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        and website_event.event_type != {PERFORMANCE}
      group by website_event.session_id, website_event.visit_id
    ),
    session_stats as (
      select
        session_id,
        count(*) as visits,
        sum(activity) as activity,
        sum(views) as views,
        sum(events) as events
      from session_rollup
      group by session_id
    ),
    property_values as (
      select
        session_data.session_id,
        session_data.string_value as value
      from session_data
      join filtered_sessions
        on filtered_sessions.session_id = session_data.session_id
        and filtered_sessions.website_id = session_data.website_id
      where session_data.website_id = {{{{websiteId::uuid}}}}
        and session_data.data_key = {{{{propertyName}}}}
        and session_data.data_type = 1
        and coalesce(session_data.string_value, '') != ''
    )
    select
      property_values.value as label,
      coalesce(sum(session_stats.activity), 0) as activity,
      count(distinct property_values.session_id) as sessions,
      coalesce(sum(session_stats.visits), 0) as visits,
      coalesce(sum(session_stats.views), 0) as views,
      coalesce(sum(session_stats.events), 0) as events
    from property_values
    join session_stats on session_stats.session_id = property_values.session_id
    group by property_values.value
    order by activity desc, property_values.value asc
    limit 100
    "#,
    sessions = filtered_sessions(&p, &pf, "website_event.session_id, website_event.website_id"),
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getSessionDataValues`: a session property's most frequent
/// values (array elements for arrays). Without a property there are none.
pub async fn session_data_values(
  db: &PgPool,
  website_id: Uuid,
  property_name: Option<&str>,
  data_type: i64,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let Some(property_name) = property_name else {
    return Ok(Vec::new());
  };
  let (p, _) = prepare(website_id, filters, &[("propertyName", property_name)], None)?;

  let (value, extra_join, type_filter, group) = if data_type == i64::from(DATA_TYPE_ARRAY) {
    (
      "array_item.value".to_owned(),
      array_elements("session_data"),
      format!("and session_data.data_type = {DATA_TYPE_ARRAY}"),
      "array_item.value",
    )
  } else {
    let type_filter = match data_type {
      0 => String::new(),
      data_type => format!("and session_data.data_type = {data_type}"),
    };
    (value_column("session_data"), String::new(), type_filter, "1")
  };

  let query = format!(
    r#"
    select
      {value} as "value",
      count(*) as "total"
    from website_event
    {cohort}
    {join}
    join session_data
        on session_data.session_id = website_event.session_id
          and session_data.website_id = website_event.website_id
    {extra_join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and website_event.event_type != {PERFORMANCE}
      and session_data.data_key = {{{{propertyName}}}}
      {type_filter}
    {filter}
    group by {group}
    order by 2 desc, 1 asc
    limit 500
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::rows(db, &query, &p.bindings).await
}
