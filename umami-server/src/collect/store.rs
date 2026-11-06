//! Database writes for event collection, mirroring upstream's `src/queries/sql`
//! (`createSession`, `saveEvent`, `saveEventData`, `saveRevenue`, `saveSessionData`,
//! `saveSessionLink`, `updateSession`). Values are truncated to upstream's
//! `FIELD_LENGTH` limits by the caller.

use chrono::{
  DateTime,
  Utc,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::data::Field;

/// Whether a website exists and is not deleted (upstream's `fetchWebsite`).
pub async fn website_exists(db: &PgPool, website_id: Uuid) -> sqlx::Result<bool> {
  sqlx::query_scalar("select exists(select 1 from website where website_id = $1 and deleted_at is null)")
    .bind(website_id)
    .fetch_one(db)
    .await
}

pub struct NewSession<'a> {
  pub id: Uuid,
  pub website_id: Uuid,
  pub browser: Option<&'a str>,
  pub os: Option<&'a str>,
  pub device: Option<&'a str>,
  pub screen: Option<&'a str>,
  pub language: Option<&'a str>,
  pub country: Option<&'a str>,
  pub region: Option<&'a str>,
  pub city: Option<&'a str>,
  pub distinct_id: Option<&'a str>,
  pub created_at: DateTime<Utc>,
}

pub async fn create_session(db: &PgPool, s: &NewSession<'_>) -> sqlx::Result<()> {
  sqlx::query(
    "insert into session (session_id, website_id, browser, os, device, screen, language, \
         country, region, city, distinct_id, created_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
         on conflict (session_id) do nothing",
  )
  .bind(s.id)
  .bind(s.website_id)
  .bind(s.browser)
  .bind(s.os)
  .bind(s.device)
  .bind(s.screen)
  .bind(s.language)
  .bind(s.country)
  .bind(s.region)
  .bind(s.city)
  .bind(s.distinct_id)
  .bind(s.created_at)
  .execute(db)
  .await?;

  Ok(())
}

#[derive(Default)]
pub struct NewEvent {
  pub id: Uuid,
  pub website_id: Uuid,
  pub session_id: Uuid,
  pub visit_id: Uuid,
  pub event_type: i32,
  pub created_at: DateTime<Utc>,
  pub url_path: String,
  pub url_query: Option<String>,
  pub referrer_path: Option<String>,
  pub referrer_query: Option<String>,
  pub referrer_domain: Option<String>,
  pub page_title: Option<String>,
  pub hostname: Option<String>,
  pub event_name: Option<String>,
  pub tag: Option<String>,
  pub utm_source: Option<String>,
  pub utm_medium: Option<String>,
  pub utm_campaign: Option<String>,
  pub utm_content: Option<String>,
  pub utm_term: Option<String>,
  pub gclid: Option<String>,
  pub fbclid: Option<String>,
  pub msclkid: Option<String>,
  pub ttclid: Option<String>,
  pub lifatid: Option<String>,
  pub twclid: Option<String>,
  /// Web vitals, as decimal text (cast to `numeric` like Prisma's `Decimal`).
  pub lcp: Option<String>,
  pub inp: Option<String>,
  pub cls: Option<String>,
  pub fcp: Option<String>,
  pub ttfb: Option<String>,
}

pub struct NewRevenue {
  pub event_name: Option<String>,
  pub currency: String,
  pub revenue: String,
}

/// Saves an event with its data and revenue in one transaction.
pub async fn save_event(
  db: &PgPool,
  event: &NewEvent,
  data: &[Field],
  revenue: Option<&NewRevenue>,
  new_id: impl Fn() -> Uuid,
) -> sqlx::Result<()> {
  let mut tx = db.begin().await?;

  sqlx::query(
    "insert into website_event (event_id, website_id, session_id, visit_id, event_type, \
         created_at, url_path, url_query, referrer_path, referrer_query, referrer_domain, \
         page_title, hostname, event_name, tag, utm_source, utm_medium, utm_campaign, \
         utm_content, utm_term, gclid, fbclid, msclkid, ttclid, li_fat_id, twclid, \
         lcp, inp, cls, fcp, ttfb) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, \
         $18, $19, $20, $21, $22, $23, $24, $25, $26, \
         $27::numeric, $28::numeric, $29::numeric, $30::numeric, $31::numeric)",
  )
  .bind(event.id)
  .bind(event.website_id)
  .bind(event.session_id)
  .bind(event.visit_id)
  .bind(event.event_type)
  .bind(event.created_at)
  .bind(&event.url_path)
  .bind(&event.url_query)
  .bind(&event.referrer_path)
  .bind(&event.referrer_query)
  .bind(&event.referrer_domain)
  .bind(&event.page_title)
  .bind(&event.hostname)
  .bind(&event.event_name)
  .bind(&event.tag)
  .bind(&event.utm_source)
  .bind(&event.utm_medium)
  .bind(&event.utm_campaign)
  .bind(&event.utm_content)
  .bind(&event.utm_term)
  .bind(&event.gclid)
  .bind(&event.fbclid)
  .bind(&event.msclkid)
  .bind(&event.ttclid)
  .bind(&event.lifatid)
  .bind(&event.twclid)
  .bind(&event.lcp)
  .bind(&event.inp)
  .bind(&event.cls)
  .bind(&event.fcp)
  .bind(&event.ttfb)
  .execute(&mut *tx)
  .await?;

  if !data.is_empty() {
    sqlx::query(
      "insert into event_data (event_data_id, website_event_id, website_id, data_key, \
             string_value, number_value, date_value, data_type, created_at) \
             select id, $2, $3, key, sv, nv::numeric, dv, dt, $4 \
             from unnest($1::uuid[], $5::text[], $6::text[], $7::text[], $8::timestamptz[], $9::int[]) \
             as t(id, key, sv, nv, dv, dt)",
    )
    .bind(data.iter().map(|_| new_id()).collect::<Vec<_>>())
    .bind(event.id)
    .bind(event.website_id)
    .bind(event.created_at)
    .bind(data.iter().map(|f| f.key.as_str()).collect::<Vec<_>>())
    .bind(data.iter().map(|f| f.string_value.as_deref()).collect::<Vec<_>>())
    .bind(data.iter().map(|f| f.number_value.as_deref()).collect::<Vec<_>>())
    .bind(data.iter().map(|f| f.date_value).collect::<Vec<_>>())
    .bind(data.iter().map(|f| f.data_type as i32).collect::<Vec<_>>())
    .execute(&mut *tx)
    .await?;
  }

  if let Some(revenue) = revenue {
    sqlx::query(
      "insert into revenue (revenue_id, website_id, session_id, event_id, event_name, \
             currency, revenue, created_at) \
             values ($1, $2, $3, $4, $5, $6, $7::numeric, $8)",
    )
    .bind(new_id())
    .bind(event.website_id)
    .bind(event.session_id)
    .bind(event.id)
    .bind(&revenue.event_name)
    .bind(&revenue.currency)
    .bind(&revenue.revenue)
    .bind(event.created_at)
    .execute(&mut *tx)
    .await?;
  }

  tx.commit().await
}

/// Upserts session properties, one row per key (upstream's `saveSessionData`).
pub async fn save_session_data(
  db: &PgPool,
  website_id: Uuid,
  session_id: Uuid,
  distinct_id: Option<&str>,
  created_at: DateTime<Utc>,
  data: &[Field],
  new_id: impl Fn() -> Uuid,
) -> sqlx::Result<()> {
  for field in data {
    sqlx::query(
      "insert into session_data (session_data_id, website_id, session_id, data_key, \
             string_value, number_value, date_value, data_type, distinct_id, created_at) \
             values ($1, $2, $3, $4, $5, $6::numeric, $7, $8, $9, coalesce($10, now())) \
             on conflict (session_id, data_key) do update set \
             website_id = excluded.website_id, \
             string_value = excluded.string_value, \
             number_value = excluded.number_value, \
             date_value = excluded.date_value, \
             data_type = excluded.data_type, \
             distinct_id = excluded.distinct_id, \
             created_at = coalesce($10, session_data.created_at)",
    )
    .bind(new_id())
    .bind(website_id)
    .bind(session_id)
    .bind(&field.key)
    .bind(&field.string_value)
    .bind(&field.number_value)
    .bind(field.date_value)
    .bind(field.data_type as i32)
    .bind(distinct_id)
    .bind(created_at)
    .execute(db)
    .await?;
  }

  Ok(())
}

/// Links a session to a distinct ID and backfills the session's distinct ID
/// (upstream's `saveSessionLink` + `updateSession`).
pub async fn link_session(
  db: &PgPool,
  website_id: Uuid,
  session_id: Uuid,
  distinct_id: &str,
  created_at: DateTime<Utc>,
) -> sqlx::Result<()> {
  sqlx::query(
    "insert into session_link (website_id, session_id, distinct_id, created_at) \
         values ($1, $2, $3, $4) \
         on conflict (website_id, distinct_id, session_id) do nothing",
  )
  .bind(website_id)
  .bind(session_id)
  .bind(distinct_id)
  .bind(created_at)
  .execute(db)
  .await?;

  sqlx::query(
    "update session set distinct_id = $3 \
         where website_id = $1 and session_id = $2 and coalesce(distinct_id, '') = ''",
  )
  .bind(website_id)
  .bind(session_id)
  .bind(distinct_id)
  .execute(db)
  .await?;

  Ok(())
}
