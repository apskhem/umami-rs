//! Event collection: `POST /api/send` and `POST /api/batch`, ported from upstream's
//! `src/app/api/send/route.ts` and `src/app/api/batch/route.ts`.

mod client;
mod data;
mod geo;
mod page;
mod payload;
pub(crate) mod record;
mod store;
mod token;

use std::convert::Infallible;
use std::net::SocketAddr;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{
  ConnectInfo,
  FromRequestParts,
  OriginalUri,
  Path,
  State,
};
use axum::http::request::Parts;
use axum::http::{
  HeaderMap,
  Uri,
  header,
};
use axum::response::{
  IntoResponse,
  Redirect,
  Response,
};
use chrono::{
  DateTime,
  Utc,
};
use crate_ua_rules as ua;
use serde_json::{
  Map,
  Value,
  json,
};
use url::Url;
use uuid::Uuid;

pub use self::geo::GeoDatabase;
use self::payload::{
  Kind,
  Payload,
  SendBody,
  Source,
};
pub use self::record::record;
use self::store::{
  NewEvent,
  NewRevenue,
  NewSession,
};
pub use self::token::CacheToken;
use crate::error::{
  AppError,
  AppResult,
};
use crate::queries::link as links;
use crate::state::AppState;
use crate::{
  crypto,
  js,
};

/// Upstream's `FIELD_LENGTH`.
mod len {
  pub const BROWSER: usize = 20;
  pub const OS: usize = 20;
  pub const DEVICE: usize = 20;
  pub const SCREEN: usize = 11;
  pub const LANGUAGE: usize = 35;
  pub const COUNTRY: usize = 2;
  pub const REGION: usize = 20;
  pub const CITY: usize = 50;
  pub const DISTINCT_ID: usize = 50;
  pub const URL: usize = 500;
  pub const PAGE_TITLE: usize = 500;
  pub const EVENT_NAME: usize = 50;
  pub const TAG: usize = 50;
  pub const HOSTNAME: usize = 100;
  pub const FIELD_VALUE: usize = 255;
  pub const CURRENCY: usize = 10;
}

/// Upstream's `EVENT_TYPE`.
pub(crate) mod event_type {
  pub const PAGE_VIEW: i32 = 1;
  pub const CUSTOM_EVENT: i32 = 2;
  pub const LINK_EVENT: i32 = 3;
  pub const PIXEL_EVENT: i32 = 4;
  pub const PERFORMANCE: i32 = 5;
}

/// Visits end after 30 minutes without activity.
const VISIT_TIMEOUT_SECS: i64 = 30 * 60;

/// Upstream's batch limit.
const MAX_BATCH_SIZE: usize = 500;

/// The connecting socket's address, when the server records it.
pub struct PeerAddr(Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
  type Rejection = Infallible;

  async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
    Ok(Self(
      parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr),
    ))
  }
}

/// `POST /api/send`
pub async fn send(
  State(state): State<AppState>,
  PeerAddr(peer): PeerAddr,
  headers: HeaderMap,
  body: Bytes,
) -> AppResult<Json<Value>> {
  // Like upstream, the body is parsed as JSON whatever its content type.
  let body = serde_json::from_slice(&body).unwrap_or(Value::Null);

  collect(&state, &headers, peer, &body).await.map(Json)
}

/// `POST /api/batch`: up to 500 `/api/send` bodies, processed in order.
pub async fn batch(
  State(state): State<AppState>,
  PeerAddr(peer): PeerAddr,
  headers: HeaderMap,
  body: Bytes,
) -> AppResult<Json<Value>> {
  let items = match serde_json::from_slice(&body) {
    Ok(Value::Array(items)) if items.len() <= MAX_BATCH_SIZE && items.iter().all(Value::is_object) => items,
    _ => return Err(AppError::bad_request()),
  };

  let mut details = Vec::new();
  let mut cache = Value::Null;

  for (index, item) in items.iter().enumerate() {
    match collect(&state, &headers, peer, item).await {
      Ok(response) => {
        if cache.is_null() {
          cache = response.get("cache").cloned().unwrap_or(Value::Null);
        }
      }
      Err(error) => {
        let (_, response) = error.into_parts();
        details.push(json!({ "index": index, "response": response }));
      }
    }
  }

  Ok(Json(json!({
      "size": items.len(),
      "processed": items.len() - details.len(),
      "errors": details.len(),
      "details": details,
      "cache": cache,
  })))
}

/// Upstream's 1×1 transparent GIF, served by `/p/{slug}`.
const PIXEL_GIF: &[u8] = &[
  0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00,
  0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02,
  0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// `GET /q/{slug}`: records a visit to the link, then redirects to its URL
/// (upstream's `src/app/(collect)/q/[slug]/route.ts`).
pub async fn link(
  State(state): State<AppState>,
  PeerAddr(peer): PeerAddr,
  OriginalUri(uri): OriginalUri,
  headers: HeaderMap,
  Path(slug): Path<String>,
) -> AppResult<Response> {
  let found = links::by_slug(&state.db, links::Kind::Link, &slug)
    .await?
    .ok_or(AppError::NotFound)?;
  // Upstream records the visit, then fails with a 500 on a URL it cannot
  // redirect to.
  let target = Url::parse(found.url.as_deref().unwrap_or_default())
    .map_err(|_| AppError::BadRequest("The link's URL is invalid.".to_owned()))?;

  visit(&state, &headers, peer, &uri, "link", found.id).await;

  Ok(Redirect::temporary(target.as_str()).into_response())
}

/// `GET /p/{slug}`: records a view of the pixel and serves it (upstream's
/// `src/app/(collect)/p/[slug]/route.ts`).
pub async fn pixel(
  State(state): State<AppState>,
  PeerAddr(peer): PeerAddr,
  OriginalUri(uri): OriginalUri,
  headers: HeaderMap,
  Path(slug): Path<String>,
) -> AppResult<Response> {
  let found = links::by_slug(&state.db, links::Kind::Pixel, &slug)
    .await?
    .ok_or(AppError::NotFound)?;

  visit(&state, &headers, peer, &uri, "pixel", found.id).await;

  let headers = [
    (header::CONTENT_TYPE, "image/gif"),
    (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
    (header::PRAGMA, "no-cache"),
    (header::EXPIRES, "0"),
  ];
  Ok((headers, PIXEL_GIF).into_response())
}

/// Records a link or pixel visit as upstream does: as an `/api/send` event for
/// the requested URL, from the visitor. The visitor is served whatever the
/// outcome, so failures are only logged.
///
/// Upstream reports the URL with its server's listening address as the host
/// (so events record `0.0.0.0` as their hostname); this uses the `Host` header.
async fn visit(state: &AppState, headers: &HeaderMap, peer: Option<SocketAddr>, uri: &Uri, source: &str, id: Uuid) {
  let host = client::header_string(headers, header::HOST).unwrap_or_else(|| "localhost".to_owned());
  let path = uri.path_and_query().map_or("/", |path| path.as_str());

  let mut payload = Map::new();
  payload.insert(source.to_owned(), json!(id));
  payload.insert("url".to_owned(), json!(format!("http://{host}{path}")));
  if let Some(referrer) = client::header_string(headers, header::REFERER).filter(|r| !r.is_empty()) {
    payload.insert("referrer".to_owned(), json!(referrer));
  }
  let body = json!({ "type": "event", "payload": payload });

  if let Err(error) = collect(state, headers, peer, &body).await {
    if error.status().is_server_error() {
      tracing::error!(%error, source, %id, "failed to record a visit");
    } else {
      tracing::debug!(%error, source, %id, "visit not recorded");
    }
  }
}

/// Processes one `/api/send` body and returns the response JSON.
async fn collect(state: &AppState, headers: &HeaderMap, peer: Option<SocketAddr>, body: &Value) -> AppResult<Value> {
  let SendBody { kind, payload } = SendBody::parse(body).map_err(|error| {
    tracing::debug!(%error, "rejected /api/send body");
    AppError::bad_request()
  })?;

  let config = &state.config;
  let settings = &config.collect;
  let source = payload.source.expect("validated: exactly one source");
  let source_id = source.id();
  let website_id = source.website();
  let source_text = payload.source_text.as_str();

  // A valid cache token lets repeat requests skip the website and session checks.
  // Unlike upstream, a token issued for a different website is ignored.
  let cache = website_id.and_then(|_| {
    headers
      .get("x-umami-cache")
      .and_then(|value| value.to_str().ok())
      .and_then(|token| CacheToken::parse(token, &config.secret))
      .filter(|cache| cache.website_id.as_deref() == Some(source_text))
  });

  if let Some(website_id) = website_id
    && cache.is_none()
    && !store::website_exists(&state.db, website_id).await?
  {
    return Err(AppError::BadRequest("Website not found.".to_owned()));
  }

  let user_agent = truthy(payload.user_agent.clone()).or_else(|| client::header_string(headers, header::USER_AGENT));

  if !settings.disable_bot_check && ua::is_bot(user_agent.as_deref().unwrap_or("")) {
    return Ok(json!({ "beep": "boop" }));
  }

  let ip =
    truthy(payload.ip.clone()).or_else(|| client::client_ip(headers, settings.client_ip_header.as_deref(), peer));

  if client::is_ignored(ip.as_deref(), &settings.ignore_ips) {
    return Err(AppError::Forbidden);
  }

  let client = ClientInfo::new(state, headers, &payload, ip.as_deref(), user_agent.as_deref());

  let now = Utc::now();
  let created_at = match payload.timestamp.filter(|&ts| ts != 0) {
    Some(ts) => DateTime::from_timestamp(ts, 0).ok_or_else(AppError::bad_request)?,
    None => now,
  };
  let distinct_id = js::truncate_opt(payload.id.clone(), len::DISTINCT_ID);

  let session_salt = crypto::session_salt(settings.salt_rotation, created_at);
  let visit_salt = crypto::visit_salt(created_at);

  // Identified users get a session separate from anonymous users who happen to
  // share their IP address and user agent.
  let session_id = crypto::deterministic_uuid(
    &[
      source_text,
      ip.as_deref().unwrap_or(""),
      user_agent.as_deref().unwrap_or(""),
      &session_salt,
      distinct_id.as_deref().unwrap_or(""),
    ],
    &config.secret,
  );
  let new_visit_id = || crypto::deterministic_uuid(&[&session_id.to_string(), &visit_salt], &config.secret);

  let cached_session = cache.as_ref().and_then(|c| truthy(c.session_id.clone()));
  let session_drift = cached_session
    .as_deref()
    .is_some_and(|cached| cached != session_id.to_string());

  if cached_session.is_none() || session_drift {
    store::create_session(
      &state.db,
      &NewSession {
        id: session_id,
        website_id: source_id,
        browser: client.browser.as_deref(),
        os: client.os.as_deref(),
        device: client.device.as_deref(),
        screen: payload.screen.as_deref().map(|s| js::truncate(s, len::SCREEN)),
        language: payload.language.as_deref().map(|s| js::truncate(s, len::LANGUAGE)),
        country: client.country.as_deref(),
        region: client.region.as_deref(),
        city: client.city.as_deref(),
        distinct_id: distinct_id.as_deref(),
        created_at,
      },
    )
    .await?;
  }

  let now_secs = now.timestamp();
  let mut visit_id = cache
    .as_ref()
    .and_then(|c| c.visit_id.as_deref())
    .and_then(|id| id.parse().ok())
    .unwrap_or_else(new_visit_id);
  let mut iat = cache
    .as_ref()
    .and_then(|c| c.iat)
    .filter(|&iat| iat != 0)
    .unwrap_or(now_secs);

  // A drifted session starts a fresh visit, as does 30 minutes of inactivity.
  if session_drift || (payload.timestamp.unwrap_or(0) == 0 && now_secs - iat > VISIT_TIMEOUT_SECS) {
    visit_id = new_visit_id();
    iat = now_secs;
  }

  let mut session_link_id = cache.as_ref().and_then(|c| c.session_link_id.clone());
  let new_id = || crypto::random_uuid(settings.use_uuidv7);

  match kind {
    Kind::Event => {
      let page = page::event_page(
        payload.url.as_deref(),
        payload.hostname.as_deref(),
        payload.referrer.as_deref(),
        settings.remove_trailing_slash,
      )
      .map_err(|_| AppError::bad_request())?;

      let event_type = match source {
        Source::Link(_) => event_type::LINK_EVENT,
        Source::Pixel(_) => event_type::PIXEL_EVENT,
        Source::Website(_) if truthy(payload.name.clone()).is_some() => event_type::CUSTOM_EVENT,
        Source::Website(_) => event_type::PAGE_VIEW,
      };

      let event_name = js::truncate_opt(payload.name.clone(), len::EVENT_NAME);
      let fields = payload.data.as_ref().map(data::flatten).unwrap_or_default();
      let revenue = payload.data.as_ref().and_then(revenue).map(|(revenue, currency)| {
        NewRevenue {
          event_name: event_name.clone(),
          currency: js::truncate(&currency, len::CURRENCY).to_owned(),
          revenue,
        }
      });

      let url = |value: Option<String>| js::truncate_opt(value, len::URL);
      let field = |value: Option<String>| js::truncate_opt(value, len::FIELD_VALUE);

      let event = NewEvent {
        id: new_id(),
        website_id: source_id,
        session_id,
        visit_id,
        event_type,
        created_at,
        url_path: js::truncate(&page.url_path, len::URL).to_owned(),
        url_query: url(Some(page.url_query)),
        referrer_path: url(page.referrer_path),
        referrer_query: url(page.referrer_query),
        referrer_domain: url(page.referrer_domain),
        page_title: page_title(payload.title.as_deref()),
        hostname: Some(js::truncate(&page.hostname, len::HOSTNAME).to_owned()),
        event_name,
        tag: js::truncate_opt(payload.tag.clone(), len::TAG),
        utm_source: field(page.utm_source),
        utm_medium: field(page.utm_medium),
        utm_campaign: field(page.utm_campaign),
        utm_content: field(page.utm_content),
        utm_term: field(page.utm_term),
        gclid: field(page.gclid),
        fbclid: field(page.fbclid),
        msclkid: field(page.msclkid),
        ttclid: field(page.ttclid),
        lifatid: field(page.lifatid),
        twclid: field(page.twclid),
        ..NewEvent::default()
      };

      store::save_event(&state.db, &event, &fields, revenue.as_ref(), new_id).await?;
    }

    Kind::Identify => {
      if let Some(website_id) = website_id
        && let Some(distinct_id) = truthy(distinct_id.clone())
      {
        let link_id = crypto::hash(&[&session_id.to_string(), &distinct_id]);

        if session_link_id.as_deref() != Some(link_id.as_str()) {
          // Best effort: a failed link must not block the session data below.
          match store::link_session(&state.db, website_id, session_id, &distinct_id, created_at).await {
            Ok(()) => session_link_id = Some(link_id),
            Err(error) => tracing::error!(%error, "failed to save session link"),
          }
        }
      }

      if let (Some(website_id), Some(data)) = (website_id, &payload.data) {
        store::save_session_data(
          &state.db,
          website_id,
          session_id,
          distinct_id.as_deref(),
          created_at,
          &data::flatten(data),
          new_id,
        )
        .await?;
      }
    }

    Kind::Performance => {
      let url_path = page::performance_path(payload.url.as_deref(), payload.hostname.as_deref())
        .map_err(|_| AppError::bad_request())?;
      let vital = |value: Option<f64>| value.map(js::number_to_string);

      let event = NewEvent {
        id: new_id(),
        website_id: source_id,
        session_id,
        visit_id,
        event_type: event_type::PERFORMANCE,
        created_at,
        url_path: js::truncate(&url_path, len::URL).to_owned(),
        page_title: page_title(payload.title.as_deref()),
        lcp: vital(payload.lcp),
        inp: vital(payload.inp),
        cls: vital(payload.cls),
        fcp: vital(payload.fcp),
        ttfb: vital(payload.ttfb),
        ..NewEvent::default()
      };

      store::save_event(&state.db, &event, &[], None, new_id).await?;
    }
  }

  let token = CacheToken::new(
    website_id.map(|_| source_text.to_owned()),
    session_id.to_string(),
    visit_id.to_string(),
    iat,
    session_link_id,
  )
  .sign(&config.secret);

  Ok(json!({
      "cache": token,
      "sessionId": session_id,
      "visitId": visit_id,
  }))
}

/// Session attributes derived from the request (upstream's `getClientInfo`),
/// truncated to their column lengths.
struct ClientInfo {
  browser: Option<String>,
  os: Option<String>,
  device: Option<String>,
  country: Option<String>,
  region: Option<String>,
  city: Option<String>,
}

impl ClientInfo {
  fn new(state: &AppState, headers: &HeaderMap, payload: &Payload, ip: Option<&str>, user_agent: Option<&str>) -> Self {
    let settings = &state.config.collect;
    let use_headers = truthy(payload.ip.clone()).is_none() && !settings.skip_location_headers;
    let location = geo::locate(ip, headers, use_headers, state.geo.as_deref()).unwrap_or_default();
    let decoded =
      |value: Option<String>, max| js::truncate_opt(value.map(|v| js::safe_decode_uri_component(&v).into_owned()), max);

    // Explicit payload values win, even empty ones (upstream uses `??`).
    let browser = payload
      .browser
      .clone()
      .or_else(|| user_agent.and_then(ua::browser_name).map(str::to_owned));
    let os = payload
      .os
      .clone()
      .or_else(|| user_agent.and_then(ua::detect_os).map(str::to_owned));
    let device = payload
      .device
      .clone()
      .or_else(|| Some(device_type(user_agent, payload.screen.as_deref())));

    Self {
      browser: js::truncate_opt(browser, len::BROWSER),
      os: js::truncate_opt(os, len::OS),
      device: js::truncate_opt(device, len::DEVICE),
      country: decoded(location.country, len::COUNTRY),
      region: decoded(location.region, len::REGION),
      city: decoded(location.city, len::CITY),
    }
  }
}

/// Upstream's `getDevice`: the user agent's device type, with desktops split into
/// "laptop" when the screen is at most 1920 pixels wide.
fn device_type(user_agent: Option<&str>, screen: Option<&str>) -> String {
  let kind = user_agent
    .and_then(ua::device_type)
    .filter(|kind| !kind.is_empty())
    .unwrap_or_else(|| "desktop".to_owned());
  let screen = screen.unwrap_or("");

  if kind == "desktop" && !screen.is_empty() {
    let width = screen.split('x').next().unwrap_or("");
    if js::string_to_number(width) <= 1920.0 {
      return "laptop".to_owned();
    }
  }

  kind
}

fn page_title(title: Option<&str>) -> Option<String> {
  title.map(|t| js::truncate(&js::safe_decode_uri_component(t), len::PAGE_TITLE).to_owned())
}

/// `revenue > 0 && currency`, with the revenue as decimal text.
fn revenue(data: &Map<String, Value>) -> Option<(String, String)> {
  let currency = data.get("currency")?.as_str().filter(|c| !c.is_empty())?;
  let revenue = match data.get("revenue")? {
    Value::Number(n) => n.as_f64().filter(|&x| x > 0.0).map(js::number_to_string)?,
    Value::String(s) => (js::string_to_number(s) > 0.0).then(|| s.trim().to_owned())?,
    _ => return None,
  };

  Some((revenue, currency.to_owned()))
}

/// JS truthiness for optional strings: `""` counts as absent.
fn truthy(value: Option<String>) -> Option<String> {
  value.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn device_type_splits_desktops_by_screen_width() {
    let mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
    let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

    assert_eq!(device_type(Some(mac), None), "desktop");
    assert_eq!(device_type(Some(mac), Some("1920x1080")), "laptop");
    assert_eq!(device_type(Some(mac), Some("2560x1440")), "desktop");
    assert_eq!(device_type(Some(mac), Some("wide")), "desktop");
    assert_eq!(device_type(Some(iphone), Some("390x844")), "mobile");
    assert_eq!(device_type(None, Some("1280x720")), "laptop");
  }

  #[test]
  fn revenue_requires_positive_amount_and_currency() {
    let data = |v: Value| v.as_object().unwrap().clone();

    assert_eq!(
      revenue(&data(json!({ "revenue": 19.99, "currency": "USD" }))),
      Some(("19.99".into(), "USD".into()))
    );
    assert_eq!(
      revenue(&data(json!({ "revenue": "5", "currency": "EUR" }))),
      Some(("5".into(), "EUR".into()))
    );
    assert_eq!(revenue(&data(json!({ "revenue": 0, "currency": "USD" }))), None);
    assert_eq!(revenue(&data(json!({ "revenue": 10 }))), None);
    assert_eq!(revenue(&data(json!({ "revenue": 10, "currency": "" }))), None);
  }
}
