//! End-to-end `/api/send` tests against a real Postgres.
//!
//! `TEST_DATABASE_URL=postgresql://umami:umami@localhost:5432/umami cargo test -- --ignored`
//! (see `compose.yaml`). Each test works in a throwaway schema.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{
  Request,
  StatusCode,
};
use http_body_util::BodyExt;
use serde_json::{
  Value,
  json,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{
  Connection,
  PgConnection,
  PgPool,
};
use tower::ServiceExt;
use umami_server::app;
use umami_server::config::Config;
use umami_server::state::AppState;
use uuid::Uuid;

const MAC_CHROME: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

struct Harness {
  base_url: String,
  schema: String,
  state: AppState,
  website: Uuid,
}

impl Harness {
  async fn new() -> Self {
    let base_url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL must be set");
    let schema = format!("umami_test_{}", Uuid::new_v4().simple());

    let mut conn = PgConnection::connect(&base_url).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA \"{schema}\"")))
      .execute(&mut conn)
      .await
      .unwrap();

    let separator = if base_url.contains('?') { '&' } else { '?' };
    let url = format!("{base_url}{separator}schema={schema}");
    let options = umami_db::connect_options(&url).unwrap();

    let mut conn = PgConnection::connect_with(&options).await.unwrap();
    umami_db::migrate::run(&mut conn).await.unwrap();

    let website = Uuid::new_v4();
    sqlx::query("insert into website (website_id, name, domain) values ($1, 'Test', 'example.com')")
      .bind(website)
      .execute(&mut conn)
      .await
      .unwrap();

    let config = Config::from_lookup(|key| {
      match key {
        "DATABASE_URL" => Some(url.clone()),
        "APP_SECRET" => Some("test-app-secret".to_owned()),
        _ => None,
      }
    })
    .unwrap();

    let state = AppState {
      db: PgPoolOptions::new().connect_with(options).await.unwrap(),
      config: Arc::new(config),
      geo: None,
    };

    Self {
      base_url,
      schema,
      state,
      website,
    }
  }

  fn db(&self) -> &PgPool {
    &self.state.db
  }

  async fn post(&self, uri: &str, headers: &[(&str, &str)], body: Value) -> (StatusCode, Value) {
    let mut builder = Request::post(uri)
      .header("user-agent", MAC_CHROME)
      .header("x-forwarded-for", "203.0.113.7");
    for (name, value) in headers {
      builder = builder.header(*name, *value);
    }

    let response = app(self.state.clone())
      .oneshot(builder.body(Body::from(body.to_string())).unwrap())
      .await
      .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();

    (status, serde_json::from_slice(&bytes).unwrap())
  }

  async fn send(&self, kind: &str, payload: Value) -> Value {
    let mut payload = payload;
    payload["website"] = json!(self.website);

    let (status, body) = self
      .post("/api/send", &[], json!({ "type": kind, "payload": payload }))
      .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
  }

  async fn count(&self, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(self.db()).await.unwrap()
  }

  async fn drop(self) {
    self.state.db.close().await;
    let mut conn = PgConnection::connect(&self.base_url).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA \"{}\" CASCADE", self.schema)))
      .execute(&mut conn)
      .await
      .unwrap();
  }
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn pageview_creates_session_and_event() {
  let h = Harness::new().await;

  let body = h
    .send(
      "event",
      json!({
          "hostname": "example.com",
          "url": "/docs/?utm_source=newsletter",
          "referrer": "https://www.google.com/search?q=umami",
          "screen": "1440x900",
          "language": "en-US",
          "title": "Docs%20page",
      }),
    )
    .await;

  let session_id: Uuid = body["sessionId"].as_str().unwrap().parse().unwrap();
  assert!(body["cache"].is_string());

  let (browser, os, device, screen, language): (String, String, String, String, String) =
    sqlx::query_as("select browser, os, device, screen, language from session where session_id = $1")
      .bind(session_id)
      .fetch_one(h.db())
      .await
      .unwrap();
  assert_eq!(
    (
      browser.as_str(),
      os.as_str(),
      device.as_str(),
      screen.as_str(),
      language.as_str()
    ),
    ("chrome", "Mac OS", "laptop", "1440x900", "en-US")
  );

  let event: (
    i32,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
  ) = sqlx::query_as(
    "select event_type, url_path, url_query, hostname, referrer_domain, utm_source, page_title \
         from website_event where session_id = $1",
  )
  .bind(session_id)
  .fetch_one(h.db())
  .await
  .unwrap();
  assert_eq!(
    event,
    (
      1,
      "/docs/".into(),
      "utm_source=newsletter".into(),
      "example.com".into(),
      Some("google.com".into()),
      Some("newsletter".into()),
      Some("Docs page".into()),
    )
  );

  // The cache token keeps the same session and visit.
  let token = body["cache"].as_str().unwrap();
  let payload = json!({ "website": h.website, "url": "/pricing", "hostname": "example.com" });
  let (status, second) = h
    .post(
      "/api/send",
      &[("x-umami-cache", token)],
      json!({ "type": "event", "payload": payload }),
    )
    .await;

  assert_eq!(status, StatusCode::OK);
  assert_eq!(second["sessionId"], body["sessionId"]);
  assert_eq!(second["visitId"], body["visitId"]);
  assert_eq!(h.count("select count(*) from session").await, 1);
  assert_eq!(h.count("select count(*) from website_event").await, 2);

  h.drop().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn custom_event_saves_data_and_revenue() {
  let h = Harness::new().await;

  h.send(
    "event",
    json!({
        "url": "/checkout",
        "name": "purchase",
        "data": { "revenue": 19.99, "currency": "USD", "plan": { "tier": "pro" }, "seats": 3, "trial": false },
    }),
  )
  .await;

  let (event_type, event_name): (i32, String) = sqlx::query_as("select event_type, event_name from website_event")
    .fetch_one(h.db())
    .await
    .unwrap();
  assert_eq!((event_type, event_name.as_str()), (2, "purchase"));

  let rows: Vec<(String, Option<String>, Option<String>, i32)> =
    sqlx::query_as("select data_key, string_value, number_value::text, data_type from event_data order by data_key")
      .fetch_all(h.db())
      .await
      .unwrap();
  assert_eq!(
    rows,
    vec![
      ("currency".into(), Some("USD".into()), None, 1),
      ("plan.tier".into(), Some("pro".into()), None, 1),
      ("revenue".into(), Some("19.9900".into()), Some("19.9900".into()), 2),
      ("seats".into(), Some("3.0000".into()), Some("3.0000".into()), 2),
      ("trial".into(), Some("false".into()), None, 3),
    ]
  );

  let revenue: (String, String) = sqlx::query_as("select currency, revenue::text from revenue")
    .fetch_one(h.db())
    .await
    .unwrap();
  assert_eq!(revenue, ("USD".into(), "19.9900".into()));

  h.drop().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn identify_links_session_and_upserts_data() {
  let h = Harness::new().await;

  let body = h
    .send("identify", json!({ "id": "user-42", "data": { "plan": "free" } }))
    .await;
  h.send("identify", json!({ "id": "user-42", "data": { "plan": "pro" } }))
    .await;

  let session_id: Uuid = body["sessionId"].as_str().unwrap().parse().unwrap();

  let distinct_id: Option<String> = sqlx::query_scalar("select distinct_id from session where session_id = $1")
    .bind(session_id)
    .fetch_one(h.db())
    .await
    .unwrap();
  assert_eq!(distinct_id.as_deref(), Some("user-42"));

  assert_eq!(h.count("select count(*) from session_link").await, 1);

  let plan: String = sqlx::query_scalar("select string_value from session_data where data_key = 'plan'")
    .fetch_one(h.db())
    .await
    .unwrap();
  assert_eq!(plan, "pro");
  assert_eq!(h.count("select count(*) from session_data").await, 1);

  h.drop().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn performance_saves_web_vitals() {
  let h = Harness::new().await;

  h.send("performance", json!({ "url": "/a#b", "lcp": 1234.56, "cls": 0.123456 }))
    .await;

  let row: (i32, String, Option<String>, Option<String>, Option<String>) =
    sqlx::query_as("select event_type, url_path, lcp::text, cls::text, inp::text from website_event")
      .fetch_one(h.db())
      .await
      .unwrap();
  assert_eq!(
    row,
    (5, "/a".into(), Some("1234.6".into()), Some("0.1235".into()), None)
  );

  h.drop().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn unknown_website_is_rejected() {
  let h = Harness::new().await;

  let payload = json!({ "website": Uuid::new_v4(), "url": "/" });
  let (status, body) = h
    .post("/api/send", &[], json!({ "type": "event", "payload": payload }))
    .await;

  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert_eq!(body["error"]["message"], "Website not found.");
  assert_eq!(h.count("select count(*) from session").await, 0);

  h.drop().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn batch_records_every_event() {
  let h = Harness::new().await;
  let event = |url: &str| json!({ "type": "event", "payload": { "website": h.website, "url": url } });

  let (status, body) = h
    .post("/api/batch", &[], json!([event("/a"), event("/b"), event("/c")]))
    .await;

  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["processed"], 3);
  assert!(body["cache"].is_string());
  assert_eq!(h.count("select count(*) from website_event").await, 3);

  h.drop().await;
}
