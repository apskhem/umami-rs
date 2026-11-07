//! Reports: funnels, goals, journeys, retention, breakdowns, UTM, attribution,
//! performance and revenue. Ported from upstream's `src/queries/sql`
//! (`funnels`, `goals`, `journeys`, `retention`, `breakdown`, `utm`,
//! `attribution`, `performance` and `revenue`), with the parameter schemas of
//! `src/lib/schema.ts`.
//!
//! Lists that upstream orders only partly break ties by name, so results are
//! stable.

use serde_json::{
  Map,
  Value,
  json,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::filters::{
  FilterValue,
  QueryFilters,
  SESSION_COLUMNS,
  filter_column,
};
use super::queries::{
  EMAIL_DOMAINS,
  LLM_DOMAINS,
  PAID_AD_PARAMS,
  SEARCH_DOMAINS,
  SHOPPING_DOMAINS,
  SOCIAL_DOMAINS,
  VIDEO_DOMAINS,
  like_any,
};
use super::sql::{
  self,
  Param,
  Parsed,
  date_sql,
  parse_filters,
  timestamp_diff_sql,
};
use crate::error::AppResult;
use crate::js;

/// Upstream's `EVENT_TYPE.pageView` and `customEvent`.
const PAGE_VIEW: i64 = 1;
const CUSTOM_EVENT: i64 = 2;

/// zod's `z.coerce.number()` of a JSON value (`Number(value)`); `NaN` for
/// arrays and objects.
fn coerce_number(value: &Value) -> f64 {
  match value {
    Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
    Value::String(s) => js::string_to_number(s),
    Value::Bool(b) => f64::from(u8::from(*b)),
    Value::Null => 0.0,
    Value::Array(_) | Value::Object(_) => f64::NAN,
  }
}

/// A string field of a JSON object (`z.string()`).
fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
  object.get(key)?.as_str()
}

/// A step's value as SQL compares it: `=` the value, or `like` when it starts
/// or ends with `*` (each end's `*` becoming `%`).
fn step_match(value: &str) -> (&'static str, String) {
  let lead = value.starts_with('*');
  let rest = if lead { &value[1..] } else { value };
  let trail = rest.ends_with('*');
  if !lead && !trail {
    return ("=", value.to_owned());
  }
  let middle = if trail { &rest[..rest.len() - 1] } else { rest };
  let (start, end) = (if lead { "%" } else { "" }, if trail { "%" } else { "" });
  ("like", format!("{start}{middle}{end}"))
}

// Funnels

/// A filter on a funnel step's event data.
#[derive(Clone, Debug, PartialEq)]
pub struct StepFilter {
  pub property: String,
  pub operator: String,
  pub value: String,
}

/// A funnel step: a page view or custom event, optionally with data filters.
#[derive(Clone, Debug, PartialEq)]
pub struct FunnelStep {
  pub is_path: bool,
  pub value: String,
  pub filters: Option<Vec<StepFilter>>,
}

/// Upstream's funnel parameters: the steps, and the minutes allowed between
/// consecutive ones.
#[derive(Clone, Debug, PartialEq)]
pub struct Funnel {
  pub window: f64,
  pub steps: Vec<FunnelStep>,
}

impl Funnel {
  /// Upstream's `funnelParametersSchema`; `None` if invalid.
  pub fn parse(parameters: &Value) -> Option<Self> {
    let object = parameters.as_object()?;
    let window = coerce_number(object.get("window").unwrap_or(&Value::Null));
    if !(window.is_finite() && window > 0.0) {
      return None;
    }
    Some(Self {
      window,
      steps: Self::parse_steps(object.get("steps")?)?,
    })
  }

  /// The `steps` array: 2 to 8 steps.
  pub fn parse_steps(steps: &Value) -> Option<Vec<FunnelStep>> {
    let steps = steps.as_array()?;
    if !(2..=8).contains(&steps.len()) {
      return None;
    }
    steps
      .iter()
      .map(|step| {
        let step = step.as_object()?;
        let is_path = match string_field(step, "type")? {
          "path" => true,
          "event" => false,
          _ => return None,
        };
        let filters = match step.get("filters") {
          None => None,
          Some(filters) => {
            Some(
              filters
                .as_array()?
                .iter()
                .map(|filter| {
                  let filter = filter.as_object()?;
                  let property = string_field(filter, "property").filter(|p| !p.is_empty())?;
                  let operator =
                    string_field(filter, "operator").filter(|o| matches!(*o, "eq" | "neq" | "c" | "dnc"))?;
                  Some(StepFilter {
                    property: property.to_owned(),
                    operator: operator.to_owned(),
                    value: string_field(filter, "value")?.to_owned(),
                  })
                })
                .collect::<Option<Vec<_>>>()?,
            )
          }
        };
        Some(FunnelStep {
          is_path,
          value: string_field(step, "value")?.to_owned(),
          filters,
        })
      })
      .collect()
  }

  /// The parameters as zod outputs them (and upstream stores them).
  pub fn to_json(&self) -> Value {
    json!({
        "window": js::number(self.window),
        "steps": self.steps.iter().map(FunnelStep::to_json).collect::<Vec<_>>(),
    })
  }
}

impl FunnelStep {
  fn to_json(&self) -> Value {
    let mut step = Map::new();
    step.insert("type".to_owned(), json!(if self.is_path { "path" } else { "event" }));
    step.insert("value".to_owned(), json!(self.value));
    if let Some(filters) = &self.filters {
      let filters: Vec<Value> = filters
        .iter()
        .map(|f| json!({ "property": f.property, "operator": f.operator, "value": f.value }))
        .collect();
      step.insert("filters".to_owned(), json!(filters));
    }
    Value::Object(step)
  }
}

/// Upstream's `getFunnel`: per step, the sessions that reached it within the
/// window after the previous one, and how many dropped off.
pub async fn funnel(db: &PgPool, website_id: Uuid, funnel: &Funnel, filters: &QueryFilters) -> AppResult<Vec<Value>> {
  let mut p = parse_filters(website_id, filters, false)?;
  p.bindings.set("window", Param::Number(funnel.window));

  let mut levels = String::new();
  let mut sums = Vec::new();
  for (i, step) in funnel.steps.iter().enumerate() {
    let level = i + 1;
    let column = if step.is_path { "url_path" } else { "event_name" };
    let (operator, value) = step_match(&step.value);
    p.bindings.set(format!("step{i}"), Param::Text(value));

    let alias = if level == 1 { "website_event" } else { "we" };
    let exists = match (&step.filters, step.is_path) {
      (Some(step_filters), false) => step_filters_sql(i, step_filters, alias, &mut p),
      _ => String::new(),
    };

    if level == 1 {
      levels.push_str(&format!(
        "
          WITH level1 AS (
            select distinct website_event.session_id, website_event.created_at
            from website_event
            {cohort}
            {join}
            where website_event.website_id = {{{{websiteId::uuid}}}}
              and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
              and {column} {operator} {{{{step{i}}}}}
              {filter}
              {exists}
          )",
        cohort = p.cohort_query,
        join = p.join_session_query,
        filter = p.filter_query,
      ));
    } else {
      levels.push_str(&format!(
        "
          , level{level} AS (
            select distinct we.session_id, we.created_at
            from level{i} l
            join website_event we
                on l.session_id = we.session_id
            where we.website_id = {{{{websiteId::uuid}}}}
                and we.created_at between l.created_at and l.created_at + interval '1 minute' * {{{{window}}}}
                and we.{column} {operator} {{{{step{i}}}}}
                and we.created_at <= {{{{endDate}}}}
                {exists}
          )"
      ));
    }
    sums.push(format!(
      "select {level} as level, count(distinct(session_id)) as count from level{level}"
    ));
  }

  let query = format!("{levels}\n{}\nORDER BY level", sums.join("\nunion "));
  let rows = sql::rows(db, &query, &p.bindings).await?;

  // Upstream's `formatResults`, with JavaScript's arithmetic (a division by
  // zero is not a number, which JSON has as `null`).
  let count = |i: usize| rows.get(i).and_then(|row| row["count"].as_i64()).unwrap_or(0) as f64;
  let first = count(0);
  let number = |x: f64| if x.is_finite() { js::number(x) } else { Value::Null };

  Ok(
    funnel
      .steps
      .iter()
      .enumerate()
      .map(|(i, step)| {
        let visitors = count(i);
        let previous = if i == 0 { 0.0 } else { count(i - 1) };
        let mut result = match step.to_json() {
          Value::Object(object) => object,
          _ => Map::new(),
        };
        result.insert("visitors".to_owned(), number(visitors));
        result.insert("previous".to_owned(), number(previous));
        let dropped = if previous > 0.0 { previous - visitors } else { 0.0 };
        result.insert("dropped".to_owned(), number(dropped));
        result.insert("dropoff".to_owned(), number(1.0 - visitors / previous));
        result.insert("remaining".to_owned(), number(visitors / first));
        Value::Object(result)
      })
      .collect(),
  )
}

/// Upstream's `buildExistsFilters`: the step's event must have each data
/// filter's property.
fn step_filters_sql(step: usize, filters: &[StepFilter], alias: &str, p: &mut Parsed) -> String {
  filters
    .iter()
    .enumerate()
    .map(|(fi, filter)| {
      let (key, val) = (format!("f_{step}_{fi}_k"), format!("f_{step}_{fi}_v"));
      p.bindings.set(&key, Param::Text(filter.property.clone()));
      let (op, value) = match filter.operator.as_str() {
        "neq" => ("!=", filter.value.clone()),
        "c" => ("ilike", format!("%{}%", filter.value)),
        "dnc" => ("not ilike", format!("%{}%", filter.value)),
        _ => ("=", filter.value.clone()),
      };
      p.bindings.set(&val, Param::Text(value));
      let ed = format!("_ed{step}_{fi}");

      format!(
        "and exists (
          select 1 from event_data {ed}
          where {ed}.website_event_id = {alias}.event_id
            and {ed}.website_id = {{{{websiteId::uuid}}}}
            and {ed}.created_at between {{{{startDate}}}} and {{{{endDate}}}}
            and {ed}.data_key = {{{{{key}}}}}
            and case when {ed}.data_type = 2 then replace({ed}.string_value, '.0000', '') else {ed}.string_value end {op} {{{{{val}}}}}
        )"
      )
    })
    .collect::<Vec<_>>()
    .join("\n")
}

// Goals

/// Upstream's goal parameters: a page path or event name to reach.
#[derive(Clone, Debug, PartialEq)]
pub struct Goal {
  pub kind: String,
  pub value: String,
}

impl Goal {
  /// Upstream's `goalParametersSchema`; `None` if invalid.
  pub fn parse(parameters: &Value) -> Option<Self> {
    let object = parameters.as_object()?;
    Some(Self {
      kind: string_field(object, "type")?.to_owned(),
      value: string_field(object, "value")?.to_owned(),
    })
  }

  /// The parameters as zod outputs them (and upstream stores them).
  pub fn to_json(&self) -> Value {
    json!({ "type": self.kind, "value": self.value })
  }
}

/// Upstream's `getGoal`: the sessions that reached the goal (`num`) out of
/// the sessions matching the filters, whatever their event type (`total`).
pub async fn goal(db: &PgPool, website_id: Uuid, goal: &Goal, filters: &QueryFilters) -> AppResult<Value> {
  let is_path = goal.kind == "path";
  let column = if is_path { "url_path" } else { "event_name" };
  let event_type = if is_path { PAGE_VIEW } else { CUSTOM_EVENT };
  let (operator, value) = step_match(&goal.value);

  let mut with_type = filters.clone();
  with_type.set("eventType", FilterValue::Number(event_type));
  let mut p = parse_filters(website_id, &with_type, false)?;
  p.bindings.set("value", Param::Text(value));
  let total = parse_filters(website_id, &filters.without("eventType"), false)?;

  let query = format!(
    "
    select count(distinct website_event.session_id) as num,
    (
      select count(distinct website_event.session_id)
      from website_event
      {total_cohort}
      {total_join}
      where website_event.website_id = {{{{websiteId::uuid}}}}
      {date}
      {total_filter}
    ) as total
    from website_event
    {cohort}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and {column} {operator} {{{{value}}}}
      {date}
      {filter}
    ",
    total_cohort = total.cohort_query,
    total_join = total.join_session_query,
    total_filter = total.filter_query,
    date = p.date_query,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::row(db, &query, &p.bindings).await
}

// Journeys

/// Upstream's journey parameters.
pub struct Journey<'a> {
  /// How many steps to follow (2 to 7).
  pub steps: usize,
  pub start_step: Option<&'a str>,
  pub end_step: Option<&'a str>,
}

/// Upstream's `getJourney`: the most common sequences of pages and events
/// within visits.
pub async fn journey(
  db: &PgPool,
  website_id: Uuid,
  journey: &Journey<'_>,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let mut p = parse_filters(website_id, filters, false)?;
  let steps = journey.steps;
  let columns: Vec<String> = (1..=steps).map(|i| format!("e{i}")).collect();
  let max_columns: Vec<String> = (1..=steps)
    .map(|i| format!("max(CASE WHEN event_number = {i} THEN \"event\" ELSE NULL END) AS e{i}"))
    .collect();

  let start_step = match journey.start_step.filter(|step| !step.is_empty()) {
    Some(step) => {
      p.bindings.set("startStep", Param::Text(step.to_owned()));
      "and e1 = {{startStep}}".to_owned()
    }
    None => String::new(),
  };
  let end_step = match journey.end_step.filter(|step| !step.is_empty()) {
    Some(step) => {
      p.bindings.set("endStep", Param::Text(step.to_owned()));
      let mut clauses: Vec<String> = (1..steps)
        .map(|i| format!("(e{i} = {{{{endStep}}}} and e{} is null)", i + 1))
        .collect();
      clauses.push(format!("(e{steps} = {{{{endStep}}}})"));
      format!("and ({})", clauses.join("\nor "))
    }
    None => String::new(),
  };

  let query = format!(
    r#"
    WITH events AS (
      select distinct
          website_event.visit_id,
          website_event.referrer_path,
          coalesce(nullIf(website_event.event_name, ''), website_event.url_path) "event",
          row_number() OVER (PARTITION BY visit_id ORDER BY website_event.created_at, website_event.event_id) AS event_number
      from website_event
      {cohort}
      {join}
      where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        {filter}),
    sequences as (
      select {select},
          count(*) count
      FROM (
        select visit_id,
            {max}
        FROM events
        group by visit_id) s
      group by {group})
    select *
    from sequences
    where 1 = 1
    {start_step}
    {end_step}
    order by count desc, {order}
    limit 100
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
    select = columns.iter().map(|c| format!("s.{c}")).collect::<Vec<_>>().join(", "),
    max = max_columns.join(",\n            "),
    group = columns.iter().map(|c| format!("s.{c}")).collect::<Vec<_>>().join(", "),
    order = columns.join(", "),
  );
  let rows = sql::rows(db, &query, &p.bindings).await?;

  // Upstream's `parseResult`: seven items, consecutive repeats merged. Steps
  // beyond `steps` are `undefined` (merged with each other, not with nulls),
  // which JSON shows as `null`.
  Ok(
    rows
      .iter()
      .map(|row| {
        let all: Vec<Option<Value>> = (1..=7).map(|i| row.get(format!("e{i}")).cloned()).collect();
        let mut items: Vec<Option<Value>> = Vec::new();
        for item in all {
          if items.last() != Some(&item) || items.is_empty() {
            items.push(item);
          }
        }
        let items: Vec<Value> = items.into_iter().map(|item| item.unwrap_or(Value::Null)).collect();
        json!({ "items": items, "count": row["count"].as_i64().unwrap_or_default() })
      })
      .collect(),
  )
}

// Retention

/// Upstream's `getRetention`: per first-visit day, the share of sessions seen
/// again each day after (up to 31 days).
pub async fn retention(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Vec<Value>> {
  let p = parse_filters(website_id, filters, false)?;
  let day = |field: &str| {
    format!("to_char(date_trunc('day', {field} at time zone {{{{tz}}}}), 'YYYY-MM-DD\"T\"HH24:00:00\"Z\"')")
  };

  let query = format!(
    r#"
    WITH cohort_items AS (
      select
        min({first_day}) as cohort_date,
        website_event.session_id
      from website_event
      {cohort}
      {join}
      where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        {filter}
      group by website_event.session_id
    ),
    user_activities AS (
      select distinct
        website_event.session_id,
        {activity_day}::date - cohort_items.cohort_date::date as day_number
      from website_event
      join cohort_items
      on website_event.session_id = cohort_items.session_id
      where website_id = {{{{websiteId::uuid}}}}
          and created_at between {{{{startDate}}}} and {{{{endDate}}}}
      ),
    cohort_size as (
      select cohort_date,
        count(*) as visitors
      from cohort_items
      group by 1
      order by 1
    ),
    cohort_date as (
      select
        c.cohort_date,
        a.day_number,
        count(*) as visitors
      from user_activities a
      join cohort_items c
      on a.session_id = c.session_id
      group by 1, 2
    )
    select
      c.cohort_date as date,
      c.day_number as day,
      s.visitors,
      c.visitors as "returnVisitors",
      c.visitors::float * 100 / s.visitors  as percentage
    from cohort_date c
    join cohort_size s
    on c.cohort_date = s.cohort_date
    where c.day_number <= 31
    order by 1, 2"#,
    first_day = day("website_event.created_at"),
    activity_day = day("created_at"),
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::rows(db, &query, &p.bindings).await
}

// Breakdown

/// Upstream's `fieldsParam`: the dimensions a breakdown may group by.
pub const BREAKDOWN_FIELDS: &[&str] = &[
  "path",
  "referrer",
  "title",
  "query",
  "os",
  "browser",
  "device",
  "country",
  "region",
  "city",
  "tag",
  "hostname",
  "distinctId",
  "language",
  "event",
  "utmSource",
  "utmMedium",
  "utmCampaign",
  "utmContent",
  "utmTerm",
];

/// Upstream's `getBreakdown`: page views, visitors, visits, bounces and time
/// spent per combination of `fields` (known, distinct field names).
pub async fn breakdown(
  db: &PgPool,
  website_id: Uuid,
  fields: &[&str],
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let mut filters = filters.clone();
  filters.set("eventType", FilterValue::Number(PAGE_VIEW));
  let join_session = fields.iter().any(|field| SESSION_COLUMNS.contains(field));
  let p = parse_filters(website_id, &filters, join_session)?;

  let names = fields.iter().map(|f| format!("\"{f}\"")).collect::<Vec<_>>().join(",");
  let columns = fields
    .iter()
    .map(|f| format!("{} as \"{f}\"", filter_column(f).unwrap_or("null")))
    .collect::<Vec<_>>()
    .join(",");
  let (bounces, visit_events) = if filters.exclude_bounce {
    ("0 as \"bounces\",", String::new())
  } else {
    (
      "sum(case when t.c = 1 and coalesce(e.has_custom_event, 0) = 0 then 1 else 0 end) as \"bounces\",",
      format!(
        "left join (
      select session_id, visit_id, 1 as \"has_custom_event\"
      from website_event
      where website_id = {{{{websiteId::uuid}}}}
        and created_at between {{{{startDate}}}} and {{{{endDate}}}}
        and event_type = {CUSTOM_EVENT}
      group by 1, 2
    ) as e
      on e.session_id = t.session_id
      and e.visit_id = t.visit_id"
      ),
    )
  };

  let query = format!(
    r#"
    select
      sum(t.c) as "views",
      count(distinct t.session_id) as "visitors",
      count(distinct t.visit_id) as "visits",
      {bounces}
      sum({time}) as "totaltime",
      {names}
    from (
      select
        {columns},
        website_event.session_id,
        website_event.visit_id,
        count(*) as "c",
        min(website_event.created_at) as "min_time",
        max(website_event.created_at) as "max_time"
      from website_event
      {cohort}
      {bounce}
      {join}
      where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        {filter}
      group by {names},
        website_event.session_id, website_event.visit_id
    ) as t
    {visit_events}
    group by {names}
    order by 2 desc, 1 desc, {names}
    limit 500
    "#,
    time = timestamp_diff_sql("t.min_time", "t.max_time"),
    cohort = p.cohort_query,
    bounce = p.exclude_bounce_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::rows(db, &query, &p.bindings).await
}

// UTM

/// Upstream's `utmMetricsQuerySchema` types: the UTM columns.
pub const UTM_COLUMNS: &[&str] = &["utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content"];

/// Upstream's `getUTM`: page views per value of a UTM `column` (one of
/// [`UTM_COLUMNS`]).
pub async fn utm(db: &PgPool, website_id: Uuid, column: &str, filters: &QueryFilters) -> AppResult<Vec<Value>> {
  let mut filters = filters.clone();
  filters.set("eventType", FilterValue::Number(PAGE_VIEW));
  let p = parse_filters(website_id, &filters, false)?;

  let query = format!(
    "
    select website_event.{column} utm, count(*) as views
    from website_event
    {cohort}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and coalesce(website_event.{column}, '') != ''
      {filter}
    group by 1
    order by 2 desc, 1 asc
    limit 50
    ",
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::rows(db, &query, &p.bindings).await
}

// Attribution

/// Upstream's attribution parameters: the conversion (a page path or event
/// name), and whether the first or last touch before it gets the credit.
pub struct Attribution<'a> {
  pub first_click: bool,
  pub is_path: bool,
  pub step: &'a str,
}

/// Upstream's `getAttribution`: for the sessions that converted, the
/// referrers, paid ad networks and UTM values of their first or last touch,
/// with the conversion totals.
pub async fn attribution(
  db: &PgPool,
  website_id: Uuid,
  attribution: &Attribution<'_>,
  filters: &QueryFilters,
) -> AppResult<Value> {
  let column = if attribution.is_path { "url_path" } else { "event_name" };
  let mut filters = filters.clone();
  let event_type = if attribution.is_path { PAGE_VIEW } else { CUSTOM_EVENT };
  filters.set("eventType", FilterValue::Number(event_type));
  let mut p = parse_filters(website_id, &filters, false)?;
  p.bindings.set("step", Param::Text(attribution.step.to_owned()));

  let events = format!(
    "WITH events AS (
        select distinct
            website_event.session_id,
            max(website_event.created_at) max_dt
        from website_event
        {cohort}
        {join}
        where website_event.website_id = {{{{websiteId::uuid}}}}
          and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
          and website_event.{column} = {{{{step}}}}
          {filter}
        group by 1),",
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );
  let (touch, before_conversion) = if attribution.first_click {
    ("min", "")
  } else {
    ("max", "and we.created_at < e.max_dt")
  };
  let model = format!(
    "
    model AS (select e.session_id,
        {touch}(we.created_at) created_at
    from events e
    join website_event we
    on we.session_id = e.session_id
    where we.website_id = {{{{websiteId::uuid}}}}
          and we.created_at between {{{{startDate}}}} and {{{{endDate}}}}
          {before_conversion}
    group by e.session_id)"
  );
  let touches = "from model m
    join website_event we
    on we.created_at = m.created_at
        and we.session_id = m.session_id";
  let in_period = "where we.website_id = {{websiteId::uuid}}
          and we.created_at between {{startDate}} and {{endDate}}";

  let referrer = format!(
    r#"
    {events}
    {model}
    select coalesce(we.referrer_domain, '') as "name",
        count(distinct we.session_id) value
    {touches}
    join session s
    on s.session_id = m.session_id
    {in_period}
          and we.referrer_domain != regexp_replace(we.hostname, '^www.', '')
          and we.referrer_domain != ''
    group by 1
    order by 2 desc, 1 asc
    limit 20
    "#
  );
  let paid_ads = format!(
    r#"
    {events}
    {model},

    results AS (
    select case
            when coalesce(gclid, '') != '' then 'Google Ads'
            when coalesce(fbclid, '') != '' then 'Facebook / Meta'
            when coalesce(msclkid, '') != '' then 'Microsoft Ads'
            when coalesce(ttclid, '') != '' then 'TikTok Ads'
            when coalesce(li_fat_id, '') != '' then 'LinkedIn Ads'
            when coalesce(twclid, '') != '' then 'Twitter Ads (X)'
            else ''
          end as "name",
        count(distinct we.session_id) as "value"
    {touches}
    {in_period}
    group by 1
    order by 2 desc, 1 asc
    limit 20)
    SELECT *
    FROM results
    WHERE name != ''
    order by value desc, name asc
    "#
  );
  let utm = |utm_column: &str| {
    format!(
      r#"
    {events}
    {model}
    select
        coalesce(we.{utm_column}, '') as "name",
        count(distinct we.session_id) as "value"
    {touches}
    {in_period}
          and we.{utm_column} != ''
    group by 1
    order by 2 desc, 1 asc
    limit 20"#
    )
  };
  let total = format!(
    r#"
    select
        count(*) as "pageviews",
        count(distinct website_event.session_id) as "visitors",
        count(distinct website_event.visit_id) as "visits"
    from website_event
    {join}
    {cohort}
    where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        and website_event.{column} = {{{{step}}}}
        {filter}
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  let b = &p.bindings;
  Ok(json!({
      "referrer": sql::rows(db, &referrer, b).await?,
      "paidAds": sql::rows(db, &paid_ads, b).await?,
      "utm_source": sql::rows(db, &utm("utm_source"), b).await?,
      "utm_medium": sql::rows(db, &utm("utm_medium"), b).await?,
      "utm_campaign": sql::rows(db, &utm("utm_campaign"), b).await?,
      "utm_content": sql::rows(db, &utm("utm_content"), b).await?,
      "utm_term": sql::rows(db, &utm("utm_term"), b).await?,
      "total": sql::row(db, &total, b).await?,
  }))
}

// Performance

/// Upstream's web-vital metrics, which are also their columns.
pub const VITALS: &[&str] = &["lcp", "inp", "cls", "fcp", "ttfb"];

/// The performance events of the period that match the filters.
fn performance_events(p: &Parsed) -> String {
  format!(
    "from website_event
    {cohort}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.event_type = 5
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      {filter}",
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
  )
}

/// The 50th, 75th and 95th percentiles of a vital.
fn percentiles(metric: &str) -> String {
  format!(
    "percentile_cont(0.5) within group (order by {metric}) as p50,
      percentile_cont(0.75) within group (order by {metric}) as p75,
      percentile_cont(0.95) within group (order by {metric}) as p95"
  )
}

/// Upstream's `getPerformanceChart`: a vital's percentiles per time bucket.
pub async fn performance_chart(
  db: &PgPool,
  website_id: Uuid,
  metric: &str,
  filters: &QueryFilters,
) -> AppResult<Value> {
  let p = parse_filters(website_id, filters, false)?;
  let query = format!(
    "
    select
      {t} t,
      {percentiles}
    {events}
    group by t
    order by t
    ",
    t = date_sql("website_event.created_at", filters),
    percentiles = percentiles(metric),
    events = performance_events(&p),
  );

  Ok(json!({ "chart": sql::rows(db, &query, &p.bindings).await? }))
}

/// Upstream's `getPerformanceMetrics`: a vital's percentiles per value of
/// `column`, worst first, up to `limit`.
pub async fn performance_metrics(
  db: &PgPool,
  website_id: Uuid,
  metric: &str,
  (column, limit): (&str, Option<i64>),
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let join_session = matches!(column, "device" | "browser");
  let p = parse_filters(website_id, filters, join_session)?;
  let limit = limit.map(|limit| format!("limit {limit}")).unwrap_or_default();
  let query = format!(
    r#"
    select
      {column} as "name",
      {percentiles},
      count(*) as count
    {events}
    group by {column}
    order by p75 desc, 1 asc
    {limit}
    "#,
    percentiles = percentiles(metric),
    events = performance_events(&p),
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getPerformanceStats`: every vital's percentiles, and the
/// number of measurements.
pub async fn performance_stats(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Value> {
  let p = parse_filters(website_id, filters, false)?;
  let columns = VITALS
    .iter()
    .flat_map(|vital| {
      [("0.5", "p50"), ("0.75", "p75"), ("0.95", "p95")].map(|(fraction, name)| {
        format!("percentile_cont({fraction}) within group (order by {vital}) as {vital}_{name}")
      })
    })
    .collect::<Vec<_>>()
    .join(",\n      ");
  let query = format!(
    "
    select
      {columns},
      count(*) as count
    {events}
    ",
    events = performance_events(&p),
  );
  let row = sql::row(db, &query, &p.bindings).await?;

  // `Number(value || 0)`.
  let number = |key: &str| js::number(row[key].as_f64().unwrap_or(0.0));
  let mut summary = Map::new();
  for vital in VITALS {
    summary.insert(
      (*vital).to_owned(),
      json!({
          "p50": number(&format!("{vital}_p50")),
          "p75": number(&format!("{vital}_p75")),
          "p95": number(&format!("{vital}_p95")),
      }),
    );
  }
  summary.insert("count".to_owned(), number("count"));

  Ok(Value::Object(summary))
}

// Revenue

/// The sessions active in the period that match the filters, and their
/// revenue in the currency (upstream's `filtered_sessions` and
/// `filtered_revenue` CTEs).
fn revenue_ctes(p: &Parsed, revenue_columns: &str) -> String {
  format!(
    "
      filtered_sessions as (
        select distinct website_event.website_id, website_event.session_id
        from website_event
        {cohort}
        {join}
        where website_event.website_id = {{{{websiteId::uuid}}}}
          and website_event.event_type != 5
        {date}
        {filter}
      ),
      filtered_revenue as (
        select {revenue_columns}
        from revenue
        join filtered_sessions
          on filtered_sessions.website_id = revenue.website_id
         and filtered_sessions.session_id = revenue.session_id
        where revenue.website_id = {{{{websiteId::uuid}}}}
          and revenue.created_at between {{{{startDate}}}} and {{{{endDate}}}}
          and upper(revenue.currency) = {{{{currency}}}}
      )",
    cohort = p.cohort_query,
    join = p.join_session_query,
    date = p.date_query,
    filter = p.filter_query,
  )
}

/// The filters with the currency bound (compared with upper-cased stored
/// currencies, as given).
fn revenue_filters(website_id: Uuid, currency: &str, filters: &QueryFilters) -> AppResult<Parsed> {
  let mut p = parse_filters(website_id, filters, false)?;
  p.bindings.set("currency", Param::Text(currency.to_owned()));
  Ok(p)
}

/// Upstream's `getRevenueChart`: revenue and its number of events per event
/// name and time bucket.
pub async fn revenue_chart(db: &PgPool, website_id: Uuid, currency: &str, filters: &QueryFilters) -> AppResult<Value> {
  let p = revenue_filters(website_id, currency, filters)?;
  let query = format!(
    "
    with {ctes}
    select
      filtered_revenue.event_name x,
      {t} t,
      sum(filtered_revenue.revenue) y,
      count(filtered_revenue.event_id) count
    from filtered_revenue
    group by x, t
    order by t, x
    ",
    ctes = revenue_ctes(
      &p,
      "revenue.event_id, revenue.event_name, revenue.created_at, revenue.revenue"
    ),
    t = date_sql("filtered_revenue.created_at", filters),
  );

  Ok(json!({ "chart": sql::rows(db, &query, &p.bindings).await? }))
}

/// Upstream's `getRevenueStats`: total revenue, its events and sessions, and
/// the averages per event and per session (`arpu`).
pub async fn revenue_stats(db: &PgPool, website_id: Uuid, currency: &str, filters: &QueryFilters) -> AppResult<Value> {
  let p = revenue_filters(website_id, currency, filters)?;
  let query = format!(
    "
    with {ctes}
    select
      sum(filtered_revenue.revenue) as sum,
      count(distinct filtered_revenue.event_id) as count,
      count(distinct filtered_revenue.session_id) as unique_count,
      (select count(*) from filtered_sessions) as total_sessions
    from filtered_revenue
    ",
    ctes = revenue_ctes(
      &p,
      "revenue.website_id, revenue.session_id, revenue.event_id, revenue.revenue"
    ),
  );
  let mut total = sql::row(db, &query, &p.bindings).await?;

  // `Number(sum)`: the numeric arrives as a string, or null.
  let sum = total["sum"].as_str().map_or(0.0, js::string_to_number);
  let per = |key: &str| {
    let n = total[key].as_i64().unwrap_or(0);
    js::number(if n > 0 { sum / n as f64 } else { 0.0 })
  };
  let (average, arpu) = (per("count"), per("total_sessions"));
  total["average"] = average;
  total["arpu"] = arpu;

  Ok(total)
}

/// The revenue breakdowns upstream offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevenueMetric {
  Country,
  Region,
  Referrer,
  Channel,
}

impl RevenueMetric {
  pub fn parse(value: &str) -> Option<Self> {
    Some(match value {
      "country" => Self::Country,
      "region" => Self::Region,
      "referrer" => Self::Referrer,
      "channel" => Self::Channel,
      _ => return None,
    })
  }
}

/// Upstream's `getRevenueMetrics`: revenue per country, region, referrer (of
/// each session's first event) or channel.
pub async fn revenue_metrics(
  db: &PgPool,
  website_id: Uuid,
  currency: &str,
  metric: RevenueMetric,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let p = revenue_filters(website_id, currency, filters)?;
  let ctes = revenue_ctes(
    &p,
    "revenue.website_id, revenue.session_id, revenue.created_at, revenue.revenue",
  );
  let by_session = |extra: &str| {
    format!(
      "
      with {ctes},
      events as (
        select
          filtered_revenue.website_id,
          filtered_revenue.session_id,
          sum(filtered_revenue.revenue) as \"value\"
        from filtered_revenue
        group by filtered_revenue.website_id, filtered_revenue.session_id),

      revenue_data as (
        select
          e.website_id,
          e.session_id,
          e.value,
          we.min_date as created_at
        from events e
        join (
          select session_id, min(created_at) as min_date
          from website_event
          where website_id = {{{{websiteId::uuid}}}}
            and created_at between {{{{startDate}}}} and {{{{endDate}}}}
          group by session_id
        ) we on we.session_id = e.session_id){extra}"
    )
  };

  let query = match metric {
    RevenueMetric::Country | RevenueMetric::Region => {
      let (columns, group) = if metric == RevenueMetric::Country {
        (r#"session.country as "name""#, "session.country")
      } else {
        (
          r#"session.country,
        session.region as "name""#,
          "session.country, session.region",
        )
      };
      format!(
        r#"
      with {ctes}
      select
        {columns},
        sum(filtered_revenue.revenue) as "value"
      from filtered_revenue
      join session
        on session.website_id = filtered_revenue.website_id
          and session.session_id = filtered_revenue.session_id
      group by {group}
      order by value desc, "name" asc
      "#
      )
    }
    RevenueMetric::Referrer => {
      format!(
        r#"{}

      select
        we.referrer_domain as "name",
        sum(revenue_data.value) as "value"
      from revenue_data
      join (
        select website_id, session_id, referrer_domain, created_at
        from website_event
        where website_id = {{{{websiteId::uuid}}}}
          and created_at between {{{{startDate}}}} and {{{{endDate}}}}) we
      on we.website_id = revenue_data.website_id
        and we.session_id = revenue_data.session_id
        and we.created_at = revenue_data.created_at
      group by we.referrer_domain
      order by value desc, "name" asc
      "#,
        by_session("")
      )
    }
    RevenueMetric::Channel => {
      let channels = format!(
        ",

    revenue_prefix as (
      select
        case when we.utm_medium ilike '%cp%' OR
              we.utm_medium ilike '%ppc%' OR
              we.utm_medium ilike '%retargeting%' OR
              we.utm_medium ilike '%paid%' then 'paid' else 'organic' end AS prefix,
        coalesce(we.referrer_domain, '') as referrer_domain,
        coalesce(we.url_query, '') as url_query,
        we.utm_medium,
        we.utm_source,
        we.hostname,
        r.value
      from revenue_data r
      join (
        select website_id, session_id, referrer_domain, url_query, utm_medium, utm_source, hostname, created_at
        from website_event
        where website_id = {{{{websiteId::uuid}}}}
          and created_at between {{{{startDate}}}} and {{{{endDate}}}}) we
      on we.website_id = r.website_id
        and we.session_id = r.session_id
        and we.created_at = r.created_at),

    channels AS (
      select
        case
          when referrer_domain = '' and url_query = '' then 'direct'
          when {paid} then 'paidAds'
          when {referral} then 'referral'
          when utm_medium ilike '%affiliate%' then 'affiliate'
          when utm_medium ilike '%sms%' or utm_source ilike '%sms%' then 'sms'
          when {llm} then 'llm'
          when {search} or utm_medium ilike '%organic%' then concat(prefix, 'Search')
          when {social} then concat(prefix, 'Social')
          when {email} or utm_medium ilike '%mail%' then 'email'
          when {shopping} or utm_medium ilike '%shop%' then concat(prefix, 'Shopping')
          when {video} or utm_medium ilike '%video%' then concat(prefix, 'Video')
          when referrer_domain != regexp_replace(hostname, '^www.', '') and referrer_domain != '' then 'referral'
          else 'Unknown' end AS \"name\",
        value
      from revenue_prefix)",
        paid = like_any("url_query", PAID_AD_PARAMS),
        referral = like_any("utm_medium", &["referral", "app", "link"]),
        llm = like_any("referrer_domain", LLM_DOMAINS),
        search = like_any("referrer_domain", SEARCH_DOMAINS),
        social = like_any("referrer_domain", SOCIAL_DOMAINS),
        email = like_any("referrer_domain", EMAIL_DOMAINS),
        shopping = like_any("referrer_domain", SHOPPING_DOMAINS),
        video = like_any("referrer_domain", VIDEO_DOMAINS),
      );
      format!(
        "{}

    select name, sum(value) as value
    from channels
    group by name
    order by value desc, name asc
    ",
        by_session(&channels)
      )
    }
  };

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getRevenueSessions`: the sessions with revenue in the
/// currency, most recently active first, paged and searchable.
///
/// As for the sessions list, each session is one row, with the hostname of
/// its latest event (upstream lists a session once per hostname).
pub async fn revenue_sessions(
  db: &PgPool,
  website_id: Uuid,
  currency: &str,
  filters: &QueryFilters,
) -> AppResult<Value> {
  let mut p = revenue_filters(website_id, currency, filters)?;
  let search = match filters.search.as_deref().filter(|s| !s.is_empty()) {
    Some(search) => {
      p.bindings.set("search", Param::Text(format!("%{search}%")));
      "and (session.browser ilike {{search}}
           or session.os ilike {{search}}
           or session.device ilike {{search}}
           or session.city ilike {{search}})"
    }
    None => "",
  };

  let query = format!(
    r#"
    select
      session.session_id as "id",
      session.website_id as "websiteId",
      (array_agg(website_event.hostname order by website_event.created_at desc, website_event.event_id desc))[1] as hostname,
      session.browser,
      session.os,
      session.device,
      session.screen,
      session.language,
      session.country,
      session.region,
      session.city,
      min(website_event.created_at) as "firstAt",
      max(website_event.created_at) as "lastAt",
      count(distinct website_event.visit_id) as "visits",
      sum(case when website_event.event_type = 1 then 1 else 0 end) as "views",
      sum(case when website_event.event_type = 2 then 1 else 0 end) as "events",
      max(website_event.created_at) as "createdAt"
    from website_event
    {cohort}
    join session
      on session.session_id = website_event.session_id
      and session.website_id = website_event.website_id
    join (
      select distinct session_id
      from revenue
      where website_id = {{{{websiteId::uuid}}}}
      and revenue.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        and upper(currency) = {{{{currency}}}}
    ) rev on rev.session_id = website_event.session_id
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.event_type != 5
    {date}
    {filter}
    {search}
    group by
      session.session_id,
      session.website_id,
      session.browser,
      session.os,
      session.device,
      session.screen,
      session.language,
      session.country,
      session.region,
      session.city
    "#,
    cohort = p.cohort_query,
    date = p.date_query,
    filter = p.filter_query,
  );

  sql::paged(
    db,
    &query,
    &p.bindings,
    filters,
    "max(website_event.created_at) desc, session.session_id",
  )
  .await
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn matches_steps_like_upstream() {
    assert_eq!(step_match("/pricing"), ("=", "/pricing".to_owned()));
    assert_eq!(step_match("/blog/*"), ("like", "/blog/%".to_owned()));
    assert_eq!(step_match("*sign*"), ("like", "%sign%".to_owned()));
    assert_eq!(step_match("*"), ("like", "%".to_owned()));
    assert_eq!(step_match("**"), ("like", "%%".to_owned()));
  }

  #[test]
  fn parses_funnels_like_zod() {
    let funnel = Funnel::parse(&json!({
        "window": "60",
        "startDate": "ignored",
        "steps": [
            { "type": "path", "value": "/", "extra": 1 },
            { "type": "event", "value": "signup", "filters": [{ "property": "plan", "operator": "eq", "value": "pro" }] },
        ],
    }))
    .unwrap();
    assert_eq!(
      funnel.to_json(),
      json!({ "window": 60, "steps": [
          { "type": "path", "value": "/" },
          { "type": "event", "value": "signup", "filters": [{ "property": "plan", "operator": "eq", "value": "pro" }] },
      ] })
    );

    let steps = json!([{ "type": "path", "value": "/" }, { "type": "path", "value": "/a" }]);
    for invalid in [
      json!({ "window": 0, "steps": steps }),
      json!({ "window": "x", "steps": steps }),
      json!({ "window": 1, "steps": [{ "type": "path", "value": "/" }] }),
      json!({ "window": 1, "steps": [{ "type": "page", "value": "/" }, { "type": "path", "value": "/" }] }),
      json!({ "window": 1, "steps": [{ "type": "event", "value": "e", "filters": null }, { "type": "path", "value": "/" }] }),
      json!({ "window": 1, "steps": [{ "type": "event", "value": "e", "filters": [{ "property": "", "operator": "eq", "value": "" }] }, { "type": "path", "value": "/" }] }),
    ] {
      assert!(Funnel::parse(&invalid).is_none(), "{invalid}");
    }
  }
}
