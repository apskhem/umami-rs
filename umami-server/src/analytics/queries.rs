//! The website analytics queries, ported from upstream's `src/queries/sql`
//! (Postgres variants).

use chrono::{
  Duration,
  Utc,
};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::filters::{
  FilterValue,
  QueryFilters,
  SESSION_COLUMNS,
  filter_column,
};
use super::sql::{
  self,
  Param,
  date_sql,
  parse_filters,
  timestamp_diff_sql,
};
use crate::error::AppResult;
use crate::js;

/// Upstream's `EVENT_TYPE.customEvent`.
const CUSTOM_EVENT: i64 = 2;
/// Upstream's default metric page size.
pub const DEFAULT_LIMIT: i64 = 500;

/// A page of a breakdown.
#[derive(Clone, Copy, Debug)]
pub struct Page {
  pub limit: i64,
  pub offset: i64,
}

/// Upstream's `getWebsiteStats`: totals for the period.
pub async fn website_stats(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Value> {
  let p = parse_filters(website_id, filters, false)?;
  let (cohort, bounce_join, join_session, filter) = (
    &p.cohort_query,
    p.exclude_bounce_query,
    p.join_session_query,
    &p.filter_query,
  );
  let totaltime = timestamp_diff_sql("t.min_time", "t.max_time");

  let query = if !filters.has_event_filters() {
    let bounces = if filters.exclude_bounce {
      "0"
    } else {
      "coalesce(sum(case when t.c = 1 and t.has_custom_event = 0 then 1 else 0 end), 0)"
    };
    format!(
      r#"
      select
        cast(coalesce(sum(t.c), 0) as bigint) as "pageviews",
        count(distinct t.session_id) as "visitors",
        count(distinct t.visit_id) as "visits",
        {bounces} as "bounces",
        cast(coalesce(sum({totaltime}), 0) as bigint) as "totaltime"
      from (
        select
          website_event.session_id,
          website_event.visit_id,
          sum(case when website_event.event_type NOT IN (2, 5) then 1 else 0 end) as "c",
          min(case when website_event.event_type NOT IN (2, 5) then website_event.created_at end) as "min_time",
          max(case when website_event.event_type NOT IN (2, 5) then website_event.created_at end) as "max_time",
          max(case when website_event.event_type = 2 then 1 else 0 end) as "has_custom_event"
        from website_event
        {cohort}
        {bounce_join}
        {join_session}
        where website_event.website_id = {{{{websiteId::uuid}}}}
          and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
          and website_event.event_type != 5
          {filter}
        group by 1, 2
        having sum(case when website_event.event_type NOT IN (2, 5) then 1 else 0 end) > 0
      ) as t
      "#
    )
  } else {
    let (bounces, visit_events) = if filters.exclude_bounce {
      ("0", String::new())
    } else {
      (
        "coalesce(sum(case when t.c = 1 and coalesce(e.has_custom_event, 0) = 0 then 1 else 0 end), 0)",
        VISIT_EVENTS_JOIN.replace("{{alias}}", "t"),
      )
    };
    format!(
      r#"
    select
      cast(coalesce(sum(t.c), 0) as bigint) as "pageviews",
      count(distinct t.session_id) as "visitors",
      count(distinct t.visit_id) as "visits",
      {bounces} as "bounces",
      cast(coalesce(sum({totaltime}), 0) as bigint) as "totaltime"
    from (
      select
        website_event.session_id,
        website_event.visit_id,
        count(*) as "c",
        min(website_event.created_at) as "min_time",
        max(website_event.created_at) as "max_time"
      from website_event
      {cohort}
      {bounce_join}
      {join_session}
      where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        and website_event.event_type NOT IN (2, 5)
        {filter}
      group by 1, 2
    ) as t
    {visit_events}
    "#
    )
  };

  sql::row(db, &query, &p.bindings).await
}

/// Visits with a custom event, joined to visit totals aliased `{{alias}}`.
const VISIT_EVENTS_JOIN: &str = r#"
      left join (
        select session_id, visit_id, 1 as "has_custom_event"
        from website_event
        where website_id = {{websiteId::uuid}}
          and created_at between {{startDate}} and {{endDate}}
          and event_type = 2
        group by 1, 2
      ) as e
        on e.session_id = {{alias}}.session_id
        and e.visit_id = {{alias}}.visit_id"#;

/// Upstream's `getPageviewStats` (`distinct` false) and `getSessionStats`
/// (`distinct` true): views or visitors per time bucket.
pub async fn series(
  db: &PgPool,
  website_id: Uuid,
  filters: &QueryFilters,
  distinct_sessions: bool,
) -> AppResult<Vec<Value>> {
  let p = parse_filters(website_id, filters, false)?;
  let count = if distinct_sessions {
    "count(distinct website_event.session_id)"
  } else {
    "count(*)"
  };
  let query = format!(
    "
    select
      {date} x,
      {count} y
    from website_event
    {cohort}
    {bounce}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and website_event.event_type NOT IN (2, 5)
      {filter}
    group by 1
    order by 1
    ",
    date = date_sql("website_event.created_at", filters),
    cohort = p.cohort_query,
    bounce = p.exclude_bounce_query,
    join = p.join_session_query,
    filter = p.filter_query,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// The column for an event dimension (`fullPath` reads `url_path`).
fn pageview_column(kind: &str) -> String {
  match kind {
    "fullPath" => "url_path".to_owned(),
    other => filter_column(other).unwrap_or(other).to_owned(),
  }
}

const FULL_PATH: &str = "case when website_event.url_query != '' then website_event.url_path || '?' || website_event.url_query else website_event.url_path end";

/// The full-path search clause, binding `fullPathSearch`.
fn full_path_search(kind: &str, filters: &QueryFilters, bindings: &mut sql::Bindings) -> String {
  match (kind, &filters.search) {
    ("fullPath", Some(search)) if !search.is_empty() => {
      bindings.set("fullPathSearch", Param::Text(format!("%{search}%")));
      format!("and ({FULL_PATH}) ilike {{{{fullPathSearch}}}}")
    }
    _ => String::new(),
  }
}

const EXCLUDE_OWN_DOMAIN: &str =
  "and website_event.referrer_domain != regexp_replace(website_event.hostname, '^www.', '')
      and website_event.referrer_domain != ''";

/// Upstream's `getPageviewMetrics`: visitors per value of an event dimension.
pub async fn pageview_metrics(
  db: &PgPool,
  website_id: Uuid,
  kind: &str,
  page: Page,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let mut p = parse_filters(website_id, filters, SESSION_COLUMNS.contains(&kind))?;
  let mut column = pageview_column(kind);
  let search = full_path_search(kind, filters, &mut p.bindings);
  let exclude_domain = if column == "referrer_domain" {
    EXCLUDE_OWN_DOMAIN
  } else {
    ""
  };

  let mut entry_exit = String::new();
  if kind == "entry" || kind == "exit" {
    let order = if kind == "entry" { "asc" } else { "desc" };
    entry_exit = format!(
      "
      join (
        select distinct on (visit_id)
          visit_id,
          url_path,
          url_query
        from website_event
        where website_event.website_id = {{{{websiteId::uuid}}}}
          and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
          and website_event.event_type NOT IN (2, 5)
        order by visit_id, created_at {order}, event_id
      ) x
      on x.visit_id = website_event.visit_id
    "
    );
    column = "x.url_path".to_owned();
  }

  let select = if kind == "fullPath" {
    FULL_PATH.to_owned()
  } else {
    column.clone()
  };
  let query = format!(
    "
    select {select} x,
      count(distinct website_event.session_id) as y
    from website_event
    {cohort}
    {bounce}
    {join}
    {entry_exit}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and website_event.event_type NOT IN (2, 5)
      and {column} != ''
      {exclude_domain}
      {search}
      {filter}
    group by 1
    order by 2 desc, 1
    limit {limit}
    offset {offset}
    ",
    cohort = p.cohort_query,
    bounce = p.exclude_bounce_query,
    join = p.join_session_query,
    filter = p.filter_query,
    limit = page.limit,
    offset = page.offset,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// The column for a session dimension (`language` groups by its first two letters).
fn session_column(kind: &str) -> String {
  match kind {
    "language" => "lower(left(language, 2))".to_owned(),
    other => filter_column(other).unwrap_or(other).to_owned(),
  }
}

/// Upstream's `getSessionMetrics`: visitors per value of a session dimension
/// (cities and regions also by country).
pub async fn session_metrics(
  db: &PgPool,
  website_id: Uuid,
  kind: &str,
  page: Page,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let p = parse_filters(website_id, filters, SESSION_COLUMNS.contains(&kind))?;
  let column = session_column(kind);
  let with_country = matches!(kind, "city" | "region");
  let query = format!(
    "
    select
      {column} x,
      count(distinct website_event.session_id) y
      {country}
    from website_event
    {cohort}
    {bounce}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and website_event.event_type NOT IN (2, 5)
      and {column} != ''
    {filter}
    group by 1
    {group_country}
    order by 2 desc, 1
    limit {limit}
    offset {offset}
    ",
    country = if with_country { ", country" } else { "" },
    group_country = if with_country { ", 3" } else { "" },
    cohort = p.cohort_query,
    bounce = p.exclude_bounce_query,
    join = p.join_session_query,
    filter = p.filter_query,
    limit = page.limit,
    offset = page.offset,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// The filters with only custom events selected.
fn custom_events(filters: &QueryFilters) -> QueryFilters {
  let mut filters = filters.clone();
  filters.set("eventType", FilterValue::Number(CUSTOM_EVENT));
  filters
}

/// Upstream's `getEventMetrics`: custom events per value.
pub async fn event_metrics(
  db: &PgPool,
  website_id: Uuid,
  kind: &str,
  page: Page,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let filters = custom_events(filters);
  let p = parse_filters(website_id, &filters, SESSION_COLUMNS.contains(&kind))?;
  let column = filter_column(kind).unwrap_or(kind);
  let query = format!(
    "
    select {column} x,
      count(*) as y
    from website_event
    {cohort}
    {join}
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      {filter}
    group by 1
    order by 2 desc, 1
    limit {limit}
    offset {offset}
    ",
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
    limit = page.limit,
    offset = page.offset,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's channel lists (`SEARCH_DOMAINS` and the rest).
pub(super) const SOCIAL_DOMAINS: &[&str] = &[
  "bsky.app",
  "facebook.com",
  "fb.com",
  "ig.com",
  "instagram.com",
  "linkedin.",
  "news.ycombinator.com",
  "pinterest.",
  "reddit.",
  "snapchat.",
  "t.co",
  "threads.net",
  "tiktok.",
  "twitter.com",
  "x.com",
];
pub(super) const SEARCH_DOMAINS: &[&str] = &[
  "baidu.com",
  "bing.com",
  "duckduckgo.com",
  "ecosia.org",
  "google.",
  "msn.com",
  "search.brave.com",
  "yandex.",
];
pub(super) const LLM_DOMAINS: &[&str] = &[
  "chatgpt.com",
  "claude.ai",
  "copilot.microsoft.com",
  "gemini.google.com",
  "meta.ai",
  "perplexity.ai",
];
pub(super) const SHOPPING_DOMAINS: &[&str] = &[
  "alibaba.com",
  "aliexpress.com",
  "amazon.",
  "bestbuy.com",
  "ebay.com",
  "etsy.com",
  "newegg.com",
  "target.com",
  "walmart.com",
];
pub(super) const EMAIL_DOMAINS: &[&str] = &[
  "gmail.",
  "hotmail.",
  "mail.yahoo.",
  "outlook.",
  "proton.me",
  "protonmail.",
];
pub(super) const VIDEO_DOMAINS: &[&str] = &["twitch.", "youtube."];
pub(super) const PAID_AD_PARAMS: &[&str] = &[
  "ad_id=",
  "aid=",
  "dclid=",
  "epik=",
  "gclid=",
  "li_fat_id=",
  "msclkid=",
  "ob_click_id=",
  "pc_id=",
  "rdt_cid=",
  "scid=",
  "ttclid=",
  "twclid=",
  "utm_medium=cpc",
  "utm_medium=paid",
  "utm_medium=paid_social",
  "utm_source=google",
];

/// `column ilike '%value%'` for each value, OR-ed, matching the values literally.
pub(super) fn like_any(column: &str, values: &[&str]) -> String {
  values
    .iter()
    .map(|value| {
      let escaped = value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
        .replace('\'', "''");
      format!("{column} ilike '%{escaped}%'")
    })
    .collect::<Vec<_>>()
    .join(" OR\n  ")
}

/// The channel of each event (upstream's `prefix` and `channels` CTEs), then the
/// first named channel of each visit (`visit_channels`), as `name`.
fn channel_ctes(p: &sql::Parsed) -> String {
  format!(
    "
    WITH prefix AS (
      select case when website_event.utm_medium LIKE 'p%' OR
          website_event.utm_medium LIKE '%ppc%' OR
          website_event.utm_medium LIKE '%retargeting%' OR
          website_event.utm_medium LIKE '%paid%' then 'paid' else 'organic' end prefix,
          website_event.referrer_domain,
          website_event.url_query,
          website_event.utm_medium,
          website_event.utm_source,
          website_event.session_id,
          website_event.visit_id,
          website_event.event_id,
          website_event.created_at,
          website_event.hostname
      from website_event
      {cohort}
      {bounce}
      {join}
      where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.event_type NOT IN (2, 5)
        {date}
        {filter}),

    channels as (
      select case
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
          else '' end AS name,
          session_id,
          visit_id,
          event_id,
          created_at
      from prefix),

    visit_channels as (
      select
        session_id,
        visit_id,
        coalesce(nullif(name, ''), 'direct') as name
      from (
        select
          name,
          session_id,
          visit_id,
          row_number() over (
            partition by session_id, visit_id
            order by case when name != '' then 0 else 1 end, created_at, event_id
          ) as row_num
        from channels
      ) as ranked_channels
      where row_num = 1)",
    cohort = p.cohort_query,
    bounce = p.exclude_bounce_query,
    join = p.join_session_query,
    date = p.date_query,
    filter = p.filter_query,
    paid = like_any("url_query", PAID_AD_PARAMS),
    referral = like_any("utm_medium", &["referral", "app", "link"]),
    llm = like_any("referrer_domain", LLM_DOMAINS),
    search = like_any("referrer_domain", SEARCH_DOMAINS),
    social = like_any("referrer_domain", SOCIAL_DOMAINS),
    email = like_any("referrer_domain", EMAIL_DOMAINS),
    shopping = like_any("referrer_domain", SHOPPING_DOMAINS),
    video = like_any("referrer_domain", VIDEO_DOMAINS),
  )
}

/// Upstream's `getChannelMetrics`: visitors per acquisition channel.
pub async fn channel_metrics(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Vec<Value>> {
  let p = parse_filters(website_id, filters, false)?;
  let query = format!(
    "{ctes}

    select name as x, count(distinct session_id) y
    from visit_channels
    group by name
    order by y desc, x
    ",
    ctes = channel_ctes(&p)
  );

  sql::rows(db, &query, &p.bindings).await
}

/// The bounce column and visit-events join of the expanded queries.
fn expanded_bounces(filters: &QueryFilters, alias: &str) -> (&'static str, String) {
  if filters.exclude_bounce {
    ("0 as \"bounces\",", String::new())
  } else {
    (
      "sum(case when t.c = 1 and coalesce(e.has_custom_event, 0) = 0 then 1 else 0 end) as \"bounces\",",
      VISIT_EVENTS_JOIN.replace("{{alias}}", alias),
    )
  }
}

/// Upstream's `GROUPED_DOMAINS`: referrers grouped by site.
const GROUPED_DOMAINS: &[(&str, &[&str])] = &[
  ("baidu.com", &["baidu."]),
  ("bing.com", &["bing."]),
  ("brave.com", &["brave."]),
  ("chatgpt.com", &["chatgpt."]),
  ("duckduckgo.com", &["duckduckgo."]),
  ("facebook.com", &["facebook."]),
  ("github.com", &["github."]),
  ("google.com", &["google."]),
  ("news.ycombinator.com", &["news.ycombinator.com"]),
  ("instagram.com", &["instagram.", "ig.com"]),
  ("linkedin.com", &["linkedin."]),
  ("pinterest.com", &["pinterest."]),
  ("reddit.com", &["reddit."]),
  ("snapchat.com", &["snapchat."]),
  ("twitter.com", &["twitter.", "t.co", "x.com"]),
  ("yahoo.com", &["yahoo."]),
  ("yandex.ru", &["yandex."]),
];

/// Upstream's `toPostgresGroupedReferrer`.
fn grouped_referrer() -> String {
  let mut sql = vec!["CASE".to_owned()];
  for (domain, matches) in GROUPED_DOMAINS {
    sql.push(format!("WHEN {} THEN '{domain}'", like_any("referrer_domain", matches)));
  }
  sql.push("  ELSE 'Other'".to_owned());
  sql.push("END".to_owned());
  sql.join("\n")
}

/// Upstream's `getPageviewExpandedMetrics`: views, visitors and visits per value
/// of an event dimension (`domain` groups referrers by site).
pub async fn pageview_expanded_metrics(
  db: &PgPool,
  website_id: Uuid,
  kind: &str,
  page: Page,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let mut p = parse_filters(website_id, filters, SESSION_COLUMNS.contains(&kind))?;
  let mut column = pageview_column(kind);
  let search = full_path_search(kind, filters, &mut p.bindings);

  let mut exclude_domain = "";
  if column == "referrer_domain" {
    exclude_domain = EXCLUDE_OWN_DOMAIN;
    if kind == "domain" {
      column = grouped_referrer();
    }
  }

  let mut entry_exit = String::new();
  if kind == "entry" || kind == "exit" {
    let aggregate = if kind == "entry" { "min" } else { "max" };
    entry_exit = format!(
      "
      join (
        select visit_id,
            {aggregate}(created_at) target_created_at
        from website_event
        where website_event.website_id = {{{{websiteId::uuid}}}}
          and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
          and website_event.event_type NOT IN (2, 5)
        group by visit_id
      ) x
      on x.visit_id = website_event.visit_id
          and x.target_created_at = website_event.created_at
    "
    );
  }

  let name = if kind == "fullPath" {
    FULL_PATH.to_owned()
  } else {
    column
  };
  let query = format!(
    r#"
    select
      name,
      sum(t.c) as "pageviews",
      count(distinct t.session_id) as "visitors",
      count(distinct t.visit_id) as "visits",
      0 as "bounces",
      0 as "totaltime"
    from (
      select
        {name} as "name",
        website_event.session_id,
        website_event.visit_id,
        count(*) as "c"
      from website_event
      {cohort}
      {bounce}
      {join}
      {entry_exit}
      where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      and website_event.event_type NOT IN (2, 5)
        {exclude_domain}
        {search}
        {filter}
      group by {name}, website_event.session_id, website_event.visit_id
    ) as t
    where name != ''
    group by name
    order by visitors desc, visits desc, name
    limit {limit}
    offset {offset}
    "#,
    cohort = p.cohort_query,
    bounce = p.exclude_bounce_query,
    join = p.join_session_query,
    filter = p.filter_query,
    limit = page.limit,
    offset = page.offset,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getSessionExpandedMetrics`: views, visitors, visits, bounces and
/// time per value of a session dimension.
pub async fn session_expanded_metrics(
  db: &PgPool,
  website_id: Uuid,
  kind: &str,
  page: Page,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let p = parse_filters(website_id, filters, SESSION_COLUMNS.contains(&kind))?;
  let column = session_column(kind);
  let with_country = matches!(kind, "city" | "region");
  let country = if with_country { "country," } else { "" };
  let group_country = if with_country { ", country" } else { "" };
  let (bounces, visit_events) = expanded_bounces(filters, "t");
  let totaltime = timestamp_diff_sql("t.min_time", "t.max_time");

  let query = format!(
    r#"
    select
      name,
      {country}
      sum(t.c) as "pageviews",
      count(distinct t.session_id) as "visitors",
      count(distinct t.visit_id) as "visits",
      {bounces}
      sum({totaltime}) as "totaltime"
    from (
      select
        {column} as "name",
        {country}
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
        and website_event.event_type NOT IN (2, 5)
        {filter}
      group by name, website_event.session_id, website_event.visit_id
      {group_country}
    ) as t
    {visit_events}
    where name != ''
    group by name
    {group_country}
    order by visitors desc, visits desc, name
    limit {limit}
    offset {offset}
    "#,
    cohort = p.cohort_query,
    bounce = p.exclude_bounce_query,
    join = p.join_session_query,
    filter = p.filter_query,
    limit = page.limit,
    offset = page.offset,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getEventExpandedMetrics`: counts, visitors and visits per custom
/// event value.
pub async fn event_expanded_metrics(
  db: &PgPool,
  website_id: Uuid,
  kind: &str,
  page: Page,
  filters: &QueryFilters,
) -> AppResult<Vec<Value>> {
  let filters = custom_events(filters);
  let p = parse_filters(website_id, &filters, SESSION_COLUMNS.contains(&kind))?;
  let column = filter_column(kind).unwrap_or(kind);
  let totaltime = timestamp_diff_sql("t.min_time", "t.max_time");

  let query = format!(
    r#"
    select
      name,
      sum(t.c) as "pageviews",
      count(distinct t.session_id) as "visitors",
      count(distinct t.visit_id) as "visits",
      sum(case when t.c = 1 then 1 else 0 end) as "bounces",
      sum({totaltime}) as "totaltime"
    from (
      select
        {column} as "name",
        website_event.session_id,
        website_event.visit_id,
        count(*) as "c",
        min(website_event.created_at) as "min_time",
        max(website_event.created_at) as "max_time"
      from website_event
      {cohort}
      {join}
      where website_event.website_id = {{{{websiteId::uuid}}}}
        and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
        {filter}
      group by name, website_event.session_id, website_event.visit_id
    ) as t
    where name != ''
    group by name
    order by visitors desc, visits desc, name
    limit {limit}
    offset {offset}
    "#,
    cohort = p.cohort_query,
    join = p.join_session_query,
    filter = p.filter_query,
    limit = page.limit,
    offset = page.offset,
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getChannelExpandedMetrics`: views, visitors, visits, bounces and
/// time per acquisition channel.
pub async fn channel_expanded_metrics(db: &PgPool, website_id: Uuid, filters: &QueryFilters) -> AppResult<Vec<Value>> {
  let p = parse_filters(website_id, filters, false)?;
  let (bounces, visit_events) = if filters.exclude_bounce {
    ("0 as \"bounces\",".to_owned(), String::new())
  } else {
    (
      "sum(case when visit_stats.c = 1 and coalesce(e.has_custom_event, 0) = 0 then 1 else 0 end) as \"bounces\","
        .to_owned(),
      VISIT_EVENTS_JOIN.replace("{{alias}}", "visit_stats"),
    )
  };
  let totaltime = timestamp_diff_sql("visit_stats.min_time", "visit_stats.max_time");

  let query = format!(
    r#"{ctes},

      visit_stats as (
        select
          session_id,
          visit_id,
          count(*) as c,
          min(created_at) as min_time,
          max(created_at) as max_time
        from prefix
        group by session_id, visit_id)

      select
        visit_channels.name,
        sum(visit_stats.c) as "pageviews",
        count(distinct visit_stats.session_id) as "visitors",
        count(distinct visit_stats.visit_id) as "visits",
        {bounces}
        sum({totaltime}) as "totaltime"
      from visit_stats
      join visit_channels
        on visit_channels.session_id = visit_stats.session_id
        and visit_channels.visit_id = visit_stats.visit_id
      {visit_events}
      group by visit_channels.name
      order by visitors desc, visits desc, visit_channels.name
      "#,
    ctes = channel_ctes(&p),
  );

  sql::rows(db, &query, &p.bindings).await
}

/// Upstream's `getActiveVisitors`: visitors in the last five minutes.
pub async fn active_visitors(db: &PgPool, website_id: Uuid) -> AppResult<Value> {
  let mut bindings = sql::Bindings::default();
  bindings.set("websiteId", Param::Uuid(website_id));
  bindings.set("startDate", Param::Time(Utc::now() - Duration::minutes(5)));

  sql::row(
    db,
    r#"
    select count(distinct session_id) as "visitors"
    from website_event
    where website_id = {{websiteId::uuid}}
    and created_at >= {{startDate}}
    "#,
    &bindings,
  )
  .await
}

/// Upstream's `getWebsiteDateRange`: the first and last event times.
pub async fn date_range(db: &PgPool, website_id: Uuid) -> AppResult<Value> {
  let mut bindings = sql::Bindings::default();
  bindings.set("websiteId", Param::Uuid(website_id));
  // Upstream's `DEFAULT_RESET_DATE`.
  bindings.set(
    "startDate",
    Param::Time("2000-01-01T00:00:00Z".parse().expect("valid date")),
  );

  sql::row(
    db,
    r#"
    select
      min(created_at) as "startDate",
      max(created_at) as "endDate"
    from website_event
    where website_id = {{websiteId::uuid}}
      and created_at >= {{startDate}}
    "#,
    &bindings,
  )
  .await
}

/// Upstream's `getValues`: the ten most common values of a dimension, for filter
/// suggestions. Comma-separated searches match any of up to five values exactly
/// (case-insensitively).
pub async fn values(db: &PgPool, website_id: Uuid, column: &str, filters: &QueryFilters) -> AppResult<Vec<Value>> {
  let mut bindings = sql::Bindings::default();
  bindings.set("websiteId", Param::Uuid(website_id));
  bindings.set("startDate", Param::Time(filters.start_date));
  bindings.set("endDate", Param::Time(filters.end_date));

  let mut search_query = String::new();
  if let Some(search) = &filters.search {
    bindings.set("search", Param::Text(format!("%{search}%")));
    let decoded = js::safe_decode_uri_component(search);
    if decoded.contains(',') {
      let clauses: Vec<String> = decoded
        .split(',')
        .take(5)
        .enumerate()
        .map(|(index, value)| {
          let key = format!("search{index}");
          bindings.set(&key, Param::Text(value.to_owned()));
          format!("{column} ilike {{{{{key}}}}}")
        })
        .collect();
      search_query = format!("AND ({})", clauses.join(" OR "));
    } else if !search.is_empty() {
      search_query = format!("and {column} ilike {{{{search}}}}");
    }
  }

  let session_columns = [
    "browser",
    "os",
    "device",
    "language",
    "country",
    "city",
    "region",
    "distinct_id",
  ];
  let query = if session_columns.contains(&column) {
    format!(
      r#"
      select {column} as "value", count(*) as "count"
      from session
      where website_id = {{{{websiteId::uuid}}}}
        and created_at between {{{{startDate}}}} and {{{{endDate}}}}
        {search_query}
      group by 1
      order by 2 desc, 1
      limit 10
      "#
    )
  } else {
    let exclude_domain = if column == "referrer_domain" {
      EXCLUDE_OWN_DOMAIN
    } else {
      ""
    };
    format!(
      r#"
    select {column} as "value", count(*) as "count"
    from website_event
    where website_event.website_id = {{{{websiteId::uuid}}}}
      and website_event.created_at between {{{{startDate}}}} and {{{{endDate}}}}
      {search_query}
      {exclude_domain}
    group by 1
    order by 2 desc, 1
    limit 10
    "#
    )
  };

  let rows = sql::rows(db, &query, &bindings).await?;

  Ok(rows.into_iter().filter(|row| !row["value"].is_null()).collect())
}
