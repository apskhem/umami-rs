//! `/api/send` and `/api/batch` parity: identical requests to upstream and umami-rs
//! must produce equivalent responses and identical rows.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.
//! Requests carry a `timestamp` so `created_at` and the session salts are the same on
//! both servers.

use serde::Deserialize;
use serde_json::{
  Value,
  json,
};
use umami_parity::{
  Stack,
  Table,
};
use uuid::Uuid;

const CHROME_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const SAFARI_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

/// A fixed time an hour ago, so both servers derive the same salts and timestamps.
fn timestamp() -> i64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs() as i64
    / 3600
    * 3600
    - 3600
}

fn send_body(kind: &str, website: Uuid, payload: Value) -> Value {
  let mut payload = payload;
  payload["website"] = json!(website);
  payload["timestamp"] = json!(timestamp());

  json!({ "type": kind, "payload": payload })
}

/// Default headers: a desktop browser at a public IP.
fn headers(ip: &str) -> [(&'static str, String); 2] {
  [
    ("user-agent", CHROME_MAC.to_owned()),
    ("x-forwarded-for", ip.to_owned()),
  ]
}

fn as_refs<'a>(headers: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
  headers.iter().map(|(name, value)| (*name, value.as_str())).collect()
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn pageviews() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let headers = headers("81.2.69.142");

  for payload in [
    json!({ "hostname": "example.com", "url": "/", "screen": "1440x900", "language": "en-US", "title": "Home", "referrer": "https://www.google.com/search?q=umami" }),
    json!({ "hostname": "www.example.com", "url": "/docs/getting-started?utm_source=newsletter&utm_medium=email&utm_campaign=launch%20week&gclid=abc", "screen": "2560x1440", "referrer": "https://news.ycombinator.com/item?id=1" }),
    json!({ "hostname": "example.com", "url": "https://example.com/caf%C3%A9/%2F#section", "title": "Caf%C3%A9%20%E2%82%AC", "referrer": "/internal" }),
    json!({ "hostname": "example.com:8080", "url": "/p?x=1&x=2&fbclid=f&msclkid=m&ttclid=t&li_fat_id=l&twclid=w", "referrer": "android-app://com.google.android.gm/" }),
    json!({ "hostname": "example.com", "url": "/tagged", "tag": "variant-b", "screen": "390x844" }),
    json!({ "hostname": "", "url": "/no-hostname", "referrer": "" }),
    json!({ "url": "/%E0%A4%A", "title": "%E0%A4%A" }),
    json!({ "hostname": "example.com", "url": format!("/{}", "long/".repeat(150)), "title": "t".repeat(600) }),
  ] {
    stack
      .send("/api/send", &as_refs(&headers), &send_body("event", website, payload))
      .await;
  }

  stack.assert_same_rows(website).await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn custom_events_with_data_and_revenue() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let headers = headers("81.2.69.160");

  for payload in [
    json!({ "url": "/checkout", "name": "purchase", "data": { "revenue": 19.99, "currency": "USD", "plan": { "tier": "pro", "seats": 3 } } }),
    json!({ "url": "/checkout", "name": "purchase", "data": { "revenue": "5", "currency": "EUR" } }),
    json!({ "url": "/checkout", "name": "refund", "data": { "revenue": 0, "currency": "USD" } }),
    json!({ "url": "/", "name": "types", "data": {
            "str": "hello", "empty": "", "int": 7, "float": 42.5, "tie": 0.03125, "neg": -0.03125,
            "big": 123456789012.34567, "tiny": 1e-7, "bool": true, "nil": null,
            "arr": [1, "two", { "three": 3 }], "longarr": vec!["abc"; 200], "obj": {},
            "date": "2024-01-02T03:04:05Z", "dateoffset": "2024-01-02T03:04:05.678+02:00",
            "datelocal": "2024-01-02T03:04:05", "long": "x".repeat(600),
        } }),
    json!({ "url": "/", "name": "n".repeat(80), "data": { "k".repeat(600): "v" } }),
    json!({ "url": "/", "name": "" }),
  ] {
    stack
      .send("/api/send", &as_refs(&headers), &send_body("event", website, payload))
      .await;
  }

  stack.assert_same_rows(website).await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn identify() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let headers = headers("81.2.69.161");

  for payload in [
    json!({ "url": "/" }),
    json!({ "id": "user-42", "data": { "plan": "free", "age": 30 } }),
    json!({ "id": "user-42", "data": { "plan": "pro", "beta": true } }),
    json!({ "id": "user-43" }),
    json!({ "data": { "anonymous": "yes" } }),
  ] {
    let kind = if payload.get("url").is_some() {
      "event"
    } else {
      "identify"
    };
    stack
      .send("/api/send", &as_refs(&headers), &send_body(kind, website, payload))
      .await;
  }

  stack.assert_same_rows(website).await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn performance() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let headers = headers("81.2.69.162");

  stack
        .send(
            "/api/send",
            &as_refs(&headers),
            &send_body(
                "performance",
                website,
                json!({ "url": "/a%20b?q=1#x", "title": "Perf", "lcp": 1234.56, "inp": 80, "cls": 0.123456, "fcp": 900.04, "ttfb": 0, "duration": 5000 }),
            ),
        )
        .await;

  stack.assert_same_rows(website).await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn links_and_pixels() {
  let stack = Stack::connect().await;
  let headers = headers("81.2.69.163");

  // Upstream does not check that links and pixels exist; their events are stored
  // under the link/pixel ID.
  for source in ["link", "pixel"] {
    let id = Uuid::new_v4();
    let body = json!({ "type": "event", "payload": { source: id, "url": "/x", "timestamp": timestamp() } });

    stack.send("/api/send", &as_refs(&headers), &body).await;
    stack.assert_same_rows(id).await;
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn geolocation() {
  let stack = Stack::connect().await;
  let website = stack.website().await;

  let cases: Vec<Vec<(&'static str, String)>> = vec![
    // MaxMind lookups (no CDN headers).
    vec![
      ("user-agent", CHROME_MAC.into()),
      ("x-forwarded-for", "81.2.69.142".into()),
    ],
    vec![
      ("user-agent", CHROME_MAC.into()),
      ("x-forwarded-for", "2a02:dd40::1".into()),
    ],
    vec![("user-agent", CHROME_MAC.into()), ("x-real-ip", "8.8.8.8".into())],
    // Local addresses are not located.
    vec![
      ("user-agent", CHROME_MAC.into()),
      ("x-forwarded-for", "192.168.1.20".into()),
    ],
    // CDN headers win over the database.
    vec![
      ("user-agent", CHROME_MAC.into()),
      ("cf-connecting-ip", "81.2.69.143".into()),
      ("cf-ipcountry", "US".into()),
      ("cf-region-code", "CA".into()),
      ("cf-ipcity", "San Jos%C3%A9".into()),
    ],
    vec![
      ("user-agent", SAFARI_IPHONE.into()),
      ("x-forwarded-for", "81.2.69.144".into()),
      ("x-vercel-ip-country", "DE".into()),
      ("x-vercel-ip-country-region", "BE".into()),
    ],
  ];

  for headers in cases {
    stack
      .send(
        "/api/send",
        &as_refs(&headers),
        &send_body("event", website, json!({ "url": "/" })),
      )
      .await;
  }

  // An explicit payload IP skips CDN headers.
  let headers = [("user-agent", CHROME_MAC.to_owned()), ("cf-ipcountry", "US".to_owned())];
  stack
    .send(
      "/api/send",
      &as_refs(&headers),
      &send_body("event", website, json!({ "url": "/", "ip": "81.2.69.145" })),
    )
    .await;

  stack.assert_same_rows(website).await;

  // Each case created its own session, so every path above was exercised.
  let locations: Vec<(Option<String>, Option<String>, Option<String>)> =
    sqlx::query_as("select country, region, city from session where website_id = $1 order by country, region, city")
      .bind(website)
      .fetch_all(&stack.rs.db)
      .await
      .unwrap();
  let s = |v: &str| Some(v.to_owned());
  assert_eq!(locations.len(), 7, "{locations:?}");
  assert!(
    locations.contains(&(s("US"), s("US-CA"), s("San José"))),
    "{locations:?}"
  );
  assert!(locations.contains(&(s("DE"), s("DE-BE"), None)), "{locations:?}");
  assert!(locations.contains(&(None, None, None)), "{locations:?}");
}

/// Without any IP header, upstream (behind Next.js) and umami-rs must still derive
/// the same client IP, and so the same session.
#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn client_ip_without_proxy_headers() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let headers = [("user-agent", CHROME_MAC.to_owned())];

  stack
    .send(
      "/api/send",
      &as_refs(&headers),
      &send_body("event", website, json!({ "url": "/" })),
    )
    .await;
  stack.assert_same_rows(website).await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn payload_overrides() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let headers = [("user-agent", "ignored".to_owned())];

  stack
        .send(
            "/api/send",
            &as_refs(&headers),
            &send_body(
                "event",
                website,
                json!({ "url": "/", "userAgent": SAFARI_IPHONE, "ip": "81.2.69.142", "browser": "custom-browser", "os": "", "device": "kiosk", "id": "d".repeat(80) }),
            ),
        )
        .await;

  stack.assert_same_rows(website).await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn rejections() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let normal = headers("81.2.69.170");

  // Bots, ignored IPs, unknown websites and invalid bodies.
  let bot = [(
    "user-agent",
    "Googlebot/2.1 (+http://www.google.com/bot.html)".to_owned(),
  )];
  let ignored = headers("198.51.100.66");
  let page = || send_body("event", website, json!({ "url": "/" }));

  stack.send("/api/send", &as_refs(&bot), &page()).await;
  stack.send("/api/send", &as_refs(&ignored), &page()).await;
  stack
    .send(
      "/api/send",
      &as_refs(&normal),
      &send_body("event", Uuid::new_v4(), json!({ "url": "/" })),
    )
    .await;

  for body in [
    json!(null),
    json!([]),
    json!({ "type": "event" }),
    json!({ "type": "event", "payload": {} }),
    json!({ "type": "record", "payload": { "website": website } }),
    json!({ "type": "event", "payload": { "website": "not-a-uuid" } }),
    json!({ "type": "event", "payload": { "website": website, "pixel": website } }),
    json!({ "type": "event", "payload": { "website": website, "name": "=cmd" } }),
    json!({ "type": "event", "payload": { "website": website, "title": 5 } }),
    json!({ "type": "event", "payload": { "website": website, "data": [1] } }),
    json!({ "type": "event", "payload": { "website": website, "timestamp": "soon" } }),
    json!({ "type": "performance", "payload": { "website": website, "lcp": 70000 } }),
  ] {
    stack.send("/api/send", &as_refs(&normal), &body).await;
  }

  stack.assert_same_rows(website).await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn cache_tokens_are_interchangeable() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let headers = headers("81.2.69.171");
  let body = send_body("event", website, json!({ "url": "/first" }));

  let first = stack.send("/api/send", &as_refs(&headers), &body).await;
  let token = first.body["cache"].as_str().unwrap().to_owned();

  // Upstream's token on the follow-up request, to both servers.
  let mut with_token = as_refs(&headers);
  with_token.push(("x-umami-cache", &token));
  stack
    .send(
      "/api/send",
      &with_token,
      &send_body("event", website, json!({ "url": "/second", "id": "user-1" })),
    )
    .await;

  // umami-rs's token is accepted by upstream too.
  let rs_first = stack.post(&stack.rs, "/api/send", &as_refs(&headers), &body).await;
  let rs_token = rs_first.body["cache"].as_str().unwrap().to_owned();
  let mut with_rs_token = as_refs(&headers);
  with_rs_token.push(("x-umami-cache", &rs_token));
  let upstream = stack
    .post(
      &stack.upstream,
      "/api/send",
      &with_rs_token,
      &send_body("event", website, json!({ "url": "/third" })),
    )
    .await;
  assert_eq!(upstream.body["visitId"], rs_first.body["visitId"]);
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn batch() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let headers = headers("81.2.69.172");

  let body = json!([
      send_body("event", website, json!({ "url": "/a" })),
      send_body("event", website, json!({ "url": "/b", "name": "click" })),
      { "type": "event", "payload": {} },
      send_body("identify", website, json!({ "id": "user-9" })),
  ]);
  let upstream = stack.send("/api/batch", &as_refs(&headers), &body).await;
  assert_eq!(upstream.body["processed"], 3);

  stack
    .send("/api/batch", &as_refs(&headers), &json!({ "not": "an array" }))
    .await;
  stack.assert_same_rows(website).await;
}

#[derive(Deserialize)]
struct UaCase {
  ua: String,
  bot: bool,
  device: Option<String>,
  device_upstream: Option<String>,
}

/// A sample of real user agents: browser, OS and device as upstream stores them.
/// Device types are allowed to differ only where ua-parser-js 1.x (ported) and 2.x
/// (upstream) are known to disagree.
#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn user_agents() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let cases: Vec<UaCase> = serde_json::from_str(include_str!("../../crate-ua-rules/tests/fixtures/ua.json")).unwrap();

  let mut drift = Vec::new();

  for (index, case) in cases.iter().enumerate().filter(|(i, _)| i % 4 == 0) {
    let headers = [
      ("user-agent", case.ua.clone()),
      ("x-forwarded-for", "81.2.69.180".to_owned()),
    ];
    let body = send_body("event", website, json!({ "url": format!("/{index}") }));

    if case.bot {
      stack.send("/api/send", &as_refs(&headers), &body).await;
      continue;
    }

    if case.device != case.device_upstream {
      let upstream = stack
        .post(&stack.upstream, "/api/send", &as_refs(&headers), &body)
        .await;
      let rs = stack.post(&stack.rs, "/api/send", &as_refs(&headers), &body).await;
      assert_eq!(upstream.body["sessionId"], rs.body["sessionId"]);
      drift.push(case);
    } else {
      stack.send("/api/send", &as_refs(&headers), &body).await;
    }
  }

  // Rows must match exactly, except the device of known-drift user agents.
  let upstream = stack.upstream.rows(Table::Session, website).await;
  let rs = stack.rs.rows(Table::Session, website).await;
  assert_eq!(upstream.len(), rs.len());

  let device = |kind: &Option<String>| kind.clone().unwrap_or_else(|| "desktop".to_owned());
  let drifted_devices: Vec<(String, String)> = drift
    .iter()
    .map(|case| (device(&case.device_upstream), device(&case.device)))
    .collect();

  let mut mismatches = Vec::new();
  for (u, r) in upstream.iter().zip(&rs) {
    let mut u = u.clone();
    let mut r = r.clone();
    let pair = (
      u["device"].as_str().unwrap_or("").to_owned(),
      r["device"].as_str().unwrap_or("").to_owned(),
    );

    if pair.0 != pair.1 && drifted_devices.contains(&pair) {
      u["device"] = Value::Null;
      r["device"] = Value::Null;
    }
    if u != r {
      mismatches.push(format!("upstream {u}\n      rs {r}"));
    }
  }

  assert!(
    mismatches.is_empty(),
    "{} sessions differ:\n{}",
    mismatches.len(),
    mismatches.join("\n")
  );
  stack.assert_same_rows_except(website, &[Table::Session]).await;
  eprintln!("{} user agents with known device drift", drift.len());
}
