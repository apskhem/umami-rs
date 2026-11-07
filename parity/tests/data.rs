//! Event and session data parity: `/api/websites/{id}/event-data*`,
//! `event-data-pivot*`, `session-data*` and `session-data-pivot`, over custom
//! events and `identify` calls sent to both servers.
//!
//! Upstream breaks ties in its lists arbitrarily (umami-rs by the remaining
//! columns), so lists are compared as sets.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use chrono::{
  DateTime,
  Duration,
  DurationRound,
  Utc,
};
use reqwest::Method;
use serde_json::{
  Value,
  json,
};
use umami_parity::fixtures::{
  Entity,
  seed_entity,
  share_token,
};
use umami_parity::{
  Response,
  Stack,
  assert_same,
};
use uuid::Uuid;

const AGENTS: [&str; 3] = [
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:121.0) Gecko/20100101 Firefox/121.0",
  "Mozilla/5.0 (iPhone; CPU iPhone OS 17_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.1 Mobile/15E148 Safari/604.1",
];

struct Dataset {
  website: Uuid,
  tokens: [String; 2],
  outsider: [String; 2],
  start: DateTime<Utc>,
  end: DateTime<Utc>,
}

impl Dataset {
  /// `path` under the website, for the dataset's period, with `query`.
  fn path(&self, path: &str, query: &str) -> String {
    let (start, end) = (self.start.timestamp_millis(), self.end.timestamp_millis());
    format!(
      "/api/websites/{}/{path}?startAt={start}&endAt={end}&{query}",
      self.website
    )
  }
}

/// Six visitors over a day and a half, each identifying with session data and
/// sending custom events with data, through both servers' `/api/send`.
async fn seed(stack: &Stack) -> Dataset {
  let (owner, tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let website = seed_entity(stack, Entity::Website, "Data", Some(owner.id), None).await;
  let base = Utc::now().duration_trunc(Duration::hours(1)).unwrap() - Duration::hours(40);

  for i in 0..6_i64 {
    let ip = format!("203.0.113.{}", i + 1);
    let headers = [("user-agent", AGENTS[i as usize % 3]), ("x-forwarded-for", ip.as_str())];
    let at = base + Duration::hours(i * 6);
    let send = |kind: &str, offset: i64, payload: Value| {
      let mut payload = payload;
      payload["website"] = json!(website);
      payload["hostname"] = json!("example.com");
      // The same identity throughout, so the data and events share a session.
      payload["id"] = json!(format!("user-{i}"));
      payload["timestamp"] = json!((at + Duration::minutes(offset)).timestamp());
      json!({ "type": kind, "payload": payload })
    };

    let tier = ["gold", "silver", "bronze"][i as usize % 3];
    let source = ["ads", "organic"][i as usize % 4 / 2];
    let mut bodies = vec![
      send("event", 0, json!({ "url": "/" })),
      send(
        "identify",
        1,
        json!({ "data": {
            "tier": tier,
            "score": i * 10,
            "beta": i % 2 == 0,
            "signup": format!("2026-0{}-15T10:00:00.000Z", 1 + i % 3),
            "langs": if i % 2 == 0 { json!(["en", "th"]) } else { json!(["de"]) },
        } }),
      ),
      send(
        "event",
        2,
        json!({ "url": "/pricing", "name": "purchase", "data": {
            "plan": if i % 3 == 0 { "pro" } else { "free" },
            "amount": 9.5 * i as f64,
            "coupon": i % 2 == 1,
            "items": if i % 2 == 0 { json!(["a", "b"]) } else { json!(["c"]) },
            "when": format!("2026-0{}-01T08:30:00.000Z", 1 + i % 2),
        } }),
      ),
    ];
    if i % 2 == 0 {
      bodies.push(send(
        "event",
        3,
        json!({ "url": "/signup", "name": "signup", "data": { "source": source } }),
      ));
    }
    if i == 4 {
      // A returning visitor, a day later.
      bodies.push(send(
        "event",
        60 * 20,
        json!({ "url": "/", "name": "purchase", "data": { "plan": "pro", "amount": 1 } }),
      ));
    }
    for body in bodies {
      stack.send("/api/send", &headers, &body).await;
    }
  }

  Dataset {
    website,
    tokens,
    outsider,
    start: base - Duration::hours(1),
    end: Utc::now() + Duration::hours(1),
  }
}

/// Sorts every list (deeply) so lists compare as sets.
fn as_sets(value: &mut Value) {
  match value {
    Value::Array(items) => {
      items.iter_mut().for_each(as_sets);
      items.sort_by_key(Value::to_string);
    }
    Value::Object(object) => object.values_mut().for_each(as_sets),
    _ => {}
  }
}

/// Whether a response has rows: a non-empty list, a page with data, or an
/// object with a non-zero number (stats).
fn has_rows(body: &Value) -> bool {
  match body {
    Value::Array(items) => !items.is_empty(),
    Value::Object(object) => {
      match object.get("data") {
        Some(data) => has_rows(data),
        None => {
          object.values().any(|v| {
            v.as_f64()
              .or_else(|| v.as_str()?.parse().ok())
              .is_some_and(|n| n != 0.0)
          })
        }
      }
    }
    _ => false,
  }
}

/// Requests `path` from both servers and compares the responses, lists as
/// sets unless `ordered`.
async fn compare(stack: &Stack, tokens: &[String; 2], path: &str, ordered: bool) -> Response {
  let (u, r) = stack.call_as(tokens, Method::GET, path, None).await;
  let (mut u_cmp, mut r_cmp) = (u.clone(), r.clone());
  if !ordered {
    as_sets(&mut u_cmp.body);
    as_sets(&mut r_cmp.body);
  }
  assert_same(&u_cmp, &r_cmp, &[], path);

  u
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn event_data_matches_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let t = &data.tokens;

  for (path, query) in [
    ("event-data/events", ""),
    ("event-data/events", "event=purchase"),
    ("event-data/events", "event=eq.signup&os=neq.iOS"),
    ("event-data/fields", ""),
    ("event-data/fields", "eventName=signup"),
    ("event-data/properties", ""),
    ("event-data/properties", "browser=firefox"),
    ("event-data/stats", ""),
    ("event-data/stats", "path=/pricing"),
    ("event-data/values", "propertyName=plan"),
    ("event-data/values", "propertyName=plan&eventName=purchase"),
    ("event-data/values", "propertyName=amount&dataType=2"),
    ("event-data/values", "propertyName=items&dataType=5"),
    ("event-data/values", "propertyName=when"),
    ("event-data/values", "propertyName=coupon&dataType=3"),
    ("event-data/values", "propertyName=missing"),
  ] {
    let response = compare(&stack, t, &data.path(path, query), false).await;
    assert_eq!(response.status, 200, "{path}?{query}: {response:?}");
    assert_eq!(
      has_rows(&response.body),
      !query.contains("missing"),
      "{path}?{query}: {response:?}"
    );
  }

  // The event list, paged, with each event's properties (whose order upstream
  // leaves to the database).
  for query in ["", "pageSize=2&page=2", "pageSize=3&eventType=2", "excludeBounce=true"] {
    let path = data.path("event-data", query);
    let (mut u, mut r) = stack.call_as(t, Method::GET, &path, None).await;
    for response in [&mut u, &mut r] {
      for event in response.body["data"].as_array_mut().into_iter().flatten() {
        if let Some(properties) = event["eventProperties"].as_array_mut() {
          properties.sort_by_key(|p| p["dataKey"].to_string());
        }
        event["eventId"] = json!("<masked>");
      }
    }
    assert_eq!(u.status, 200, "{path}: {u:?}");
    assert!(has_rows(&u.body), "{path}: {u:?}");
    assert_same(&u, &r, &[], &path);
  }

  // One event's data, by each server's own ID for the same event.
  let latest = "select event_id from website_event where website_id = $1[1] and event_name = 'signup' \
                order by created_at desc limit 1";
  let mut bodies = Vec::new();
  for target in stack.targets() {
    let id = target.snapshot(latest, &[data.website]).await[0]["event_id"].clone();
    let path = format!("/api/websites/{}/event-data/{}", data.website, id.as_str().unwrap());
    let token = if target.name == "upstream" { &t[0] } else { &t[1] };
    let response = stack
      .call(
        target,
        Method::GET,
        &path,
        &[("authorization", &format!("Bearer {token}"))],
        None,
      )
      .await;
    let mut body = response.body;
    as_sets(&mut body);
    umami_parity::mask(&mut body, &["eventId"]);
    bodies.push((response.status, body));
  }
  assert_eq!(bodies[0], bodies[1]);
  assert_eq!(bodies[0].1.as_array().map(Vec::len), Some(1), "{:?}", bodies[0]);
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn event_explorer_matches_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let t = &data.tokens;

  for query in [
    "eventName=purchase",
    "eventName=purchase&pageSize=2&page=2",
    "eventName=purchase&pf_plan=eq.pro",
    "eventName=purchase&pf_amount=2.gt.10&pf_coupon=3.eq.true",
    "eventName=purchase&pf_items=5.c.a&timezone=Asia/Bangkok",
    "eventName=purchase&pf_when=4.af.2026-01-15",
    "eventName=signup&browser=chrome",
  ] {
    let path = data.path("event-data-pivot", query);
    let (mut u, mut r) = stack.call_as(t, Method::GET, &path, None).await;
    umami_parity::mask(&mut u.body, &["eventId"]);
    umami_parity::mask(&mut r.body, &["eventId"]);
    assert_eq!(u.status, 200, "{path}: {u:?}");
    assert!(has_rows(&u.body), "{path}: {u:?}");
    assert_same(&u, &r, &[], &path);
  }

  for (path, query) in [
    ("array-series", "propertyName=items"),
    ("array-series", "propertyName=items&unit=hour&timezone=Asia/Bangkok"),
    ("date-series", "propertyName=when"),
    ("date-series", "propertyName=when&timezone=America/New_York"),
    ("numeric-series", "propertyName=amount"),
    ("numeric-series", "propertyName=amount&metric=avg&unit=hour"),
    ("numeric-series", "propertyName=amount&metric=count&pf_plan=eq.free"),
    ("numeric-stats", "propertyName=amount"),
    ("numeric-stats", "propertyName=amount&pf_plan=neq.pro"),
    ("numeric-stats", "propertyName=nothing"),
    ("property-series", "propertyName=plan"),
    ("property-series", "propertyName=coupon&unit=hour"),
  ] {
    let path = data.path(
      &format!("event-data-pivot/{path}"),
      &format!("eventName=purchase&{query}"),
    );
    let response = compare(&stack, t, &path, false).await;
    assert_eq!(response.status, 200, "{path}: {response:?}");
    assert_eq!(
      has_rows(&response.body),
      !query.contains("nothing"),
      "{path}: {response:?}"
    );
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn session_data_matches_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let t = &data.tokens;

  for query in [
    "propertyName=tier",
    "propertyName=tier&pageSize=2",
    "propertyName=score&pf_score=2.gte.20",
    "propertyName=tier&pf_beta=3.eq.true&timezone=Asia/Bangkok",
    "propertyName=tier&pf_langs=5.dnc.th",
  ] {
    let response = compare(&stack, t, &data.path("session-data-pivot", query), true).await;
    assert_eq!(response.status, 200, "{query}: {response:?}");
    assert!(has_rows(&response.body), "{query}: {response:?}");
  }

  for (path, query) in [
    ("session-data/array-series", "propertyName=langs"),
    ("session-data/date-series", "propertyName=signup"),
    ("session-data/date-series", "propertyName=signup&timezone=Asia/Tokyo"),
    ("session-data/numeric-series", "propertyName=score"),
    ("session-data/numeric-series", "propertyName=score&metric=avg&unit=hour"),
    ("session-data/numeric-series", "propertyName=score&metric=count"),
    ("session-data/numeric-stats", "propertyName=score"),
    (
      "session-data/numeric-stats",
      "propertyName=score&pf_tier=eq.gold,silver",
    ),
    ("session-data/properties", ""),
    ("session-data/properties", "propertyName=tier&os=iOS"),
    ("session-data/property-series", "propertyName=tier"),
    ("session-data/property-series", "propertyName=beta&unit=hour"),
    ("session-data/stats", "propertyName=tier"),
    ("session-data/stats", "propertyName=tier&pf_score=2.lt.30"),
    ("session-data/values", "propertyName=tier"),
    ("session-data/values", "propertyName=score&dataType=2"),
    ("session-data/values", "propertyName=langs&dataType=5"),
    ("session-data/values", "propertyName=signup"),
  ] {
    let response = compare(&stack, t, &data.path(path, query), false).await;
    assert_eq!(response.status, 200, "{path}?{query}: {response:?}");
    assert!(has_rows(&response.body), "{path}?{query}: {response:?}");
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn data_errors_and_permissions_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let website = data.website;

  for (tokens, path, status) in [
    (&data.tokens, "/api/websites/{w}/event-data?startAt=1".to_owned(), 400),
    (&data.tokens, data.path("event-data", "page=0"), 400),
    (&data.tokens, data.path("event-data-pivot", ""), 400),
    (
      &data.tokens,
      data.path("event-data-pivot", "eventName=x&unit=century"),
      400,
    ),
    (
      &data.tokens,
      data.path(
        "event-data-pivot/numeric-series",
        "eventName=x&propertyName=y&metric=max",
      ),
      400,
    ),
    (&data.tokens, data.path("event-data/values", "dataType=1"), 400),
    (
      &data.tokens,
      data.path("event-data/values", "propertyName=x&dataType=1.5"),
      400,
    ),
    (&data.tokens, data.path("session-data/stats", ""), 400),
    (&data.tokens, data.path("session-data/values", "dataType=x"), 400),
    // Without a property, there are no values.
    (&data.tokens, data.path("session-data/values", ""), 200),
    // Routes without `timezone` or `unit` ignore them.
    (
      &data.tokens,
      data.path("event-data/stats", "timezone=Mars/Base&unit=century"),
      200,
    ),
    (
      &data.tokens,
      data.path("session-data/properties", "timezone=Mars/Base"),
      200,
    ),
    (&data.outsider, data.path("event-data/stats", ""), 401),
    (&data.outsider, data.path("session-data/properties", ""), 401),
    (
      &data.outsider,
      format!("/api/websites/{website}/event-data/{}", Uuid::new_v4()),
      401,
    ),
    (
      &data.tokens,
      format!("/api/websites/{website}/event-data/{}", Uuid::new_v4()),
      200,
    ),
  ] {
    let path = path.replace("{w}", &website.to_string());
    let response = compare(&stack, tokens, &path, false).await;
    assert_eq!(response.status, status, "{path}: {response:?}");
  }

  // Upstream fails with a 500 on a malformed event ID.
  let path = format!("/api/websites/{website}/event-data/event");
  let (u, r) = stack.call_as(&data.tokens, Method::GET, &path, None).await;
  assert_eq!((u.status, r.status), (500, 404), "{path}: {u:?} {r:?}");

  // Share links see event data if they enable events, session data if they
  // enable sessions.
  for (parameters, path, status) in [
    (json!({ "events": true }), "event-data/stats", 200),
    (json!({ "overview": true }), "event-data/stats", 401),
    (json!({ "sessions": true }), "session-data/properties", 200),
    (json!({ "events": true }), "session-data/properties", 401),
  ] {
    let mut claims = json!({ "websiteId": website, "parameters": parameters });
    claims["shareId"] = json!(Uuid::new_v4());
    let token = share_token(claims);
    let path = data.path(path, "");
    let (u, r) = stack
      .call_both(
        Method::GET,
        &path,
        &[("x-umami-share-token", &token), ("x-umami-share-context", "1")],
        None,
      )
      .await;
    assert_eq!(u.status, status, "{path} {parameters}: {u:?}");
    let (mut u, mut r) = (u, r);
    as_sets(&mut u.body);
    as_sets(&mut r.body);
    assert_same(&u, &r, &[], &path);
  }
}
