//! Website analytics parity: stats, pageviews, metrics (and expanded), active
//! visitors, date ranges and filter values, over a seeded dataset that both
//! databases hold identically.
//!
//! Upstream breaks ties in breakdowns arbitrarily (umami-rs by value), so lists
//! are compared as sets; their order is checked separately.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use std::collections::HashSet;
use std::fmt::Write as _;

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
use umami_parity::fixtures::share_token;
use umami_parity::{
  Response,
  Stack,
  assert_same,
  bearer,
};
use uuid::Uuid;

/// A tiny deterministic generator, so the dataset is varied but repeatable.
struct Lcg(u64);

impl Lcg {
  fn next(&mut self, n: usize) -> usize {
    self.0 = self
      .0
      .wrapping_mul(6364136223846793005)
      .wrapping_add(1442695040888963407);
    ((self.0 >> 33) % n as u64) as usize
  }

  fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
    items[self.next(items.len())]
  }
}

fn sql_text(value: Option<&str>) -> String {
  value.map_or("null".to_owned(), |v| format!("'{}'", v.replace('\'', "''")))
}

fn sql_time(at: DateTime<Utc>) -> String {
  format!("'{}'", at.to_rfc3339())
}

struct Dataset {
  website: Uuid,
  segment: Uuid,
  cohort: Uuid,
  now: DateTime<Utc>,
  /// The sessions, in order: 1 and 2 share an identity, 3 has two, and
  /// 40-42 are active now.
  sessions: Vec<Uuid>,
}

/// Seeds a website with 40 sessions over 45 days (a few active right now), with
/// pageviews, custom events with data, performance events, session data, a
/// segment and a cohort.
async fn seed(stack: &Stack) -> Dataset {
  let now = Utc::now().duration_trunc(Duration::minutes(1)).unwrap();
  let website = Uuid::new_v4();
  let (segment, cohort) = (Uuid::new_v4(), Uuid::new_v4());
  let mut rng = Lcg(7);
  let mut sql = String::new();

  writeln!(
        sql,
        "insert into website (website_id, name, domain, created_at) values ('{website}', 'Analytics', 'example.com', now());"
    )
    .unwrap();
  writeln!(
        sql,
        r#"insert into segment (segment_id, website_id, type, name, parameters) values
           ('{segment}', '{website}', 'segment', 'Chrome', '{{"filters":[{{"name":"browser","operator":"eq","value":"chrome"}}],"match":"all"}}'),
           ('{cohort}', '{website}', 'cohort', 'Signups', '{{"dateRange":"30day","filters":[{{"name":"path","operator":"c","value":"pricing"}}],"action":{{"type":"event","value":"signup"}},"match":"all"}}');"#
    )
    .unwrap();

  let paths = ["/", "/pricing", "/blog/rust", "/blog/postgres", "/about", "/docs/start"];
  let titles = ["Home", "Pricing", "Blog", "About", "Docs"];
  let referrers: [(&str, &str, &str); 7] = [
    ("", "", ""),
    ("google.com", "/search", ""),
    ("facebook.com", "/", ""),
    ("example.com", "/", ""),
    ("chatgpt.com", "/", ""),
    ("news.ycombinator.com", "/item", ""),
    ("", "", "gclid=abc&utm_source=google&utm_medium=cpc"),
  ];
  let browsers = ["chrome", "firefox", "safari", "edge-chromium"];
  let oses = ["Mac OS", "Windows 10", "iOS", "Android OS", "Linux"];
  let devices = ["desktop", "laptop", "mobile", "tablet"];
  let places = [
    ("US", "US-CA", "San Francisco"),
    ("US", "US-NY", "New York"),
    ("DE", "DE-BE", "Berlin"),
    ("TH", "TH-10", "Bangkok"),
    ("JP", "JP-13", "Tokyo"),
  ];
  let languages = ["en-US", "de-DE", "th-TH", "ja-JP", "en-GB"];

  let mut first_visits = Vec::new();
  let mut sessions = Vec::new();
  // Every event gets its own instant, so time-ordered lists have no ties
  // (which upstream orders arbitrarily).
  let mut used = HashSet::new();
  let mut unique = |mut at: DateTime<Utc>| {
    while !used.insert(at) {
      at += Duration::seconds(1);
    }
    at
  };
  for s in 0..43 {
    let session = Uuid::new_v4();
    sessions.push(session);
    // Sessions 40-42 are active right now.
    let start = if s >= 40 {
      now - Duration::minutes(2 + s as i64 - 40)
    } else {
      now - Duration::hours(s as i64 * 26 + 1) - Duration::minutes(rng.next(50) as i64)
    };
    let (country, region, city) = places[rng.next(places.len())];
    let distinct = (s % 6 == 0).then(|| format!("user-{s}"));
    let hostname = if rng.next(3) == 0 {
      "www.example.com"
    } else {
      "example.com"
    };
    // Identities: each identified session's own, one shared by sessions 1
    // and 2, and two for session 3.
    let links: Vec<String> = match s {
      1 | 2 => vec!["shared".into()],
      3 => vec!["first".into(), "second".into()],
      _ => distinct.iter().cloned().collect(),
    };
    for (n, link) in links.iter().enumerate() {
      writeln!(
        sql,
        "insert into session_link (website_id, session_id, distinct_id, created_at) \
                 values ('{website}', '{session}', '{link}', {});",
        sql_time(start + Duration::minutes(n as i64)),
      )
      .unwrap();
    }
    writeln!(
            sql,
            "insert into session (session_id, website_id, browser, os, device, screen, language, country, region, city, distinct_id, created_at) \
             values ('{session}', '{website}', {}, {}, {}, '1920x1080', {}, '{country}', '{region}', '{city}', {}, {});",
            sql_text(Some(rng.pick(&browsers))),
            sql_text(Some(rng.pick(&oses))),
            sql_text(Some(rng.pick(&devices))),
            sql_text(Some(rng.pick(&languages))),
            sql_text(distinct.as_deref()),
            sql_time(start),
        )
        .unwrap();
    if s % 2 == 0 {
      writeln!(
                sql,
                "insert into session_data (session_data_id, website_id, session_id, data_key, string_value, data_type, created_at) \
                 values (gen_random_uuid(), '{website}', '{session}', 'tier', '{}', 1, {});",
                if s % 4 == 0 { "gold" } else { "silver" },
                sql_time(start),
            )
            .unwrap();
    }

    for v in 0..(1 + s % 3) {
      let visit = Uuid::new_v4();
      let mut at = start + Duration::minutes(v as i64 * 95);
      if s >= 40 {
        at = start;
      }
      if v == 0 {
        first_visits.push((session, visit, at, hostname));
      }
      let (domain, referrer_path, query) = referrers[rng.next(referrers.len())];

      for e in 0..(1 + (s + v) % 4) {
        let path = paths[rng.next(paths.len())];
        let (domain, referrer_path, query) = if e == 0 {
          (domain, referrer_path, query)
        } else {
          ("", "", "")
        };
        let utm = |key: &str| query.split('&').find_map(|pair| pair.strip_prefix(&format!("{key}=")));
        writeln!(
                    sql,
                    "insert into website_event (event_id, website_id, session_id, visit_id, created_at, url_path, url_query, referrer_path, referrer_query, referrer_domain, page_title, event_type, hostname, utm_source, utm_medium, gclid) \
                     values ('{}', '{website}', '{session}', '{visit}', {}, '{path}', '{query}', '{referrer_path}', '', '{domain}', {}, 1, '{hostname}', {}, {}, {});",
                    Uuid::new_v4(),
                    sql_time(unique(at)),
                    sql_text(Some(rng.pick(&titles))),
                    sql_text(utm("utm_source")),
                    sql_text(utm("utm_medium")),
                    sql_text(utm("gclid")),
                )
                .unwrap();
        at += Duration::seconds(37 + rng.next(300) as i64);
      }

      if (s + v) % 3 == 0 {
        let event = Uuid::new_v4();
        let name = if (s + v) % 2 == 0 { "signup" } else { "purchase" };
        writeln!(
                    sql,
                    "insert into website_event (event_id, website_id, session_id, visit_id, created_at, url_path, url_query, referrer_domain, event_type, event_name, tag, hostname) \
                     values ('{event}', '{website}', '{session}', '{visit}', {}, '/pricing', '', '', 2, '{name}', {}, '{hostname}');",
                    sql_time(unique(at)),
                    sql_text((s % 2 == 0).then_some("campaign")),
                )
                .unwrap();
        writeln!(
                    sql,
                    "insert into event_data (event_data_id, website_id, website_event_id, data_key, string_value, number_value, date_value, data_type, created_at) values \
                     (gen_random_uuid(), '{website}', '{event}', 'plan', '{}', null, null, 1, {t}), \
                     (gen_random_uuid(), '{website}', '{event}', 'amount', null, {}, null, 2, {t}), \
                     (gen_random_uuid(), '{website}', '{event}', 'tags', '{}', null, null, 5, {t}), \
                     (gen_random_uuid(), '{website}', '{event}', 'when', null, null, '2026-0{}-15T10:00:00Z', 4, {t});",
                    if s % 2 == 0 { "pro" } else { "free" },
                    10 * (s % 5),
                    if s % 3 == 0 { r#"["a","b"]"# } else { r#"["c"]"# },
                    1 + s % 9,
                    t = sql_time(at),
                )
                .unwrap();
      }

      if (s + v) % 4 == 1 {
        writeln!(
                    sql,
                    "insert into website_event (event_id, website_id, session_id, visit_id, created_at, url_path, url_query, event_type, hostname, lcp, fcp) \
                     values ('{}', '{website}', '{session}', '{visit}', {}, '/', '', 5, '{hostname}', 1234.5, 456);",
                    Uuid::new_v4(),
                    sql_time(unique(at)),
                )
                .unwrap();
      }
    }
  }

  // `/top{k}` is viewed by k sessions, so its visitor counts are distinct.
  for k in 1..=5 {
    for (session, visit, at, hostname) in first_visits.iter().take(k) {
      writeln!(
                sql,
                "insert into website_event (event_id, website_id, session_id, visit_id, created_at, url_path, url_query, referrer_domain, event_type, hostname) \
                 values ('{}', '{website}', '{session}', '{visit}', {}, '/top{k}', '', '', 1, '{hostname}');",
                Uuid::new_v4(),
                sql_time(unique(*at + Duration::seconds(k as i64))),
            )
            .unwrap();
    }
  }

  for target in stack.targets() {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.clone()))
      .execute(&target.db)
      .await
      .unwrap_or_else(|e| panic!("{}: seeding: {e}", target.name));
  }

  Dataset {
    website,
    segment,
    cohort,
    now,
    sessions,
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

/// Asserts a breakdown is ordered by its count, descending.
fn assert_ordered(response: &Response, context: &str) {
  let Some(rows) = response.body.as_array() else {
    return;
  };
  let count = |row: &Value| {
    let field = row
      .get("y")
      .or_else(|| row.get("visitors"))
      .or_else(|| row.get("count"));
    field.and_then(Value::as_i64).unwrap_or_default()
  };
  assert!(
    rows.windows(2).all(|pair| count(&pair[0]) >= count(&pair[1])),
    "{context}: not ordered by count: {}",
    response.body
  );
}

async fn compare(stack: &Stack, tokens: &[String; 2], path: &str) -> Response {
  let (u, r) = stack.call_as(tokens, Method::GET, path, None).await;
  assert_ordered(&r, path);

  let (mut u_sets, mut r_sets) = (u.clone(), r.clone());
  as_sets(&mut u_sets.body);
  as_sets(&mut r_sets.body);
  assert_same(&u_sets, &r_sets, &[], path);

  u
}

/// `startAt`/`endAt` for the last `days` days.
fn range(data: &Dataset, days: i64) -> String {
  let end = data.now + Duration::minutes(1);
  let start = end - Duration::days(days);
  format!("startAt={}&endAt={}", start.timestamp_millis(), end.timestamp_millis())
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn stats_and_series_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let base = format!("/api/websites/{}", data.website);

  let ranges = [
    range(&data, 1),
    format!("{}&unit=day&timezone=America/New_York", range(&data, 7)),
    format!("{}&unit=day&timezone=Asia/Calcutta", range(&data, 30)),
    format!("{}&unit=month&timezone=Asia/Bangkok", range(&data, 45)),
    format!("{}&unit=minute&timezone=utc", range(&data, 45)),
  ];
  let filters = [
    String::new(),
    "browser=chrome".into(),
    "browser=neq.chrome,safari".into(),
    "path=c./blog".into(),
    "path=/pricing&path1=/".into(),
    "referrer=google.com".into(),
    "country=US,DE".into(),
    "os=re.^(mac|ios)".into(),
    "browser=chrome&path=/pricing&match=any".into(),
    "eventType=1".into(),
    "excludeBounce=true".into(),
    "hostname=www.example.com".into(),
    "title=dnc.About".into(),
    "event=signup".into(),
    "query=c.utm".into(),
    "distinctId=user-6".into(),
    "epf0=1.eq.plan.pro".into(),
    "epf0=2.gte.amount.20".into(),
    "epf0=5.c.tags.a".into(),
    "epf0=4.af.when.2026-03-01".into(),
    "spf0=1.eq.tier.gold".into(),
    format!("segment={}", data.segment),
    format!("cohort={}", data.cohort),
  ];

  for range in &ranges {
    for filter in &filters {
      let query = format!("{range}&{filter}");
      for path in [
        format!("{base}/stats?{query}"),
        format!("{base}/stats?{query}&compare=yoy"),
        format!("{base}/pageviews?{query}"),
        format!("{base}/pageviews?{query}&compare=prev"),
      ] {
        let response = compare(&stack, &admin, &path).await;
        assert_eq!(response.status, 200, "{path}: {response:?}");
      }
    }
  }

  // The dataset is spread out, and the filters select real subsets.
  let all = range(&data, 45);
  let stats = compare(&stack, &admin, &format!("{base}/stats?{all}")).await;
  let visitors = stats.body["visitors"].as_i64().unwrap();
  assert!(visitors > 40, "{stats:?}");
  assert!(stats.body["bounces"].as_i64().unwrap() > 0, "{stats:?}");
  for filter in [
    "spf0=1.eq.tier.gold".to_owned(),
    "browser=chrome&path=/pricing&match=any".to_owned(),
    format!("segment={}", data.segment),
    format!("cohort={}", data.cohort),
  ] {
    let path = format!("{base}/stats?{all}&{filter}");
    let filtered = compare(&stack, &admin, &path).await.body["visitors"].as_i64().unwrap();
    assert!((1..visitors).contains(&filtered), "{path}: {filtered} of {visitors}");
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn metrics_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let base = format!("/api/websites/{}", data.website);

  let types = [
    "path",
    "entry",
    "exit",
    "fullPath",
    "referrer",
    "domain",
    "title",
    "query",
    "event",
    "tag",
    "hostname",
    "utmSource",
    "utmMedium",
    "browser",
    "os",
    "device",
    "screen",
    "language",
    "country",
    "region",
    "city",
    "distinctId",
    "channel",
  ];
  let filters = [
    String::new(),
    "browser=chrome".into(),
    "path=c./blog&match=any&country=DE".into(),
    "excludeBounce=true".into(),
    "epf0=1.eq.plan.free".into(),
    "spf0=1.eq.tier.silver".into(),
    format!("segment={}", data.segment),
    format!("cohort={}", data.cohort),
    "search=o".into(),
  ];

  let (mut compared, mut empty) = (0, 0);
  for range in [range(&data, 7), format!("{}&timezone=Asia/Tokyo", range(&data, 45))] {
    for filter in &filters {
      for kind in types {
        for endpoint in ["metrics", "metrics/expanded"] {
          let path = format!("{base}/{endpoint}?type={kind}&{range}&{filter}");
          let response = compare(&stack, &admin, &path).await;
          assert_eq!(response.status, 200, "{path}: {response:?}");
          compared += 1;
          empty += usize::from(response.body == json!([]));
        }
      }
    }
  }
  // Most breakdowns have data (tags, UTM and distinct IDs are sparse).
  assert!(empty * 4 < compared, "{empty} of {compared} breakdowns are empty");

  let event_data = format!("{base}/metrics?type=event&epf0=2.gte.amount.20&{}", range(&data, 45));
  let events = compare(&stack, &admin, &event_data).await;
  assert_ne!(events.body, json!([]), "{event_data}");

  // Paging, on a dimension whose counts are distinct.
  let top = format!("{base}/metrics?type=path&search=top&{}", range(&data, 45));
  let all = compare(&stack, &admin, &top).await;
  assert_eq!(all.body.as_array().unwrap().len(), 5, "{all:?}");
  for page in ["limit=2", "limit=2&offset=2", "limit=1.5", "offset=4"] {
    let path = format!("{top}&{page}");
    let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
    assert_same(&u, &r, &[], &path);
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn values_active_and_date_range_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let base = format!("/api/websites/{}", data.website);
  let range = range(&data, 45);

  for kind in [
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
  ] {
    for search in ["", "&search=o", "&search=chrome,safari", "&search=%2Fblog%2Crust"] {
      let path = format!("{base}/values?type={kind}&{range}{search}");
      let response = compare(&stack, &admin, &path).await;
      assert_eq!(response.status, 200, "{path}: {response:?}");
    }
  }

  for path in [format!("{base}/active"), format!("{base}/daterange")] {
    let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
    assert_eq!(u.status, 200, "{path}: {u:?}");
    assert_same(&u, &r, &[], &path);
  }

  let (u, _) = stack
    .call_as(&admin, Method::GET, &format!("{base}/active"), None)
    .await;
  assert_eq!(u.body, json!({ "visitors": 3 }));
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn errors_and_permissions_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let (_, outsider) = stack.session("user").await;
  let base = format!("/api/websites/{}", data.website);
  let range = range(&data, 7);

  for (tokens, path, status) in [
    (&admin, format!("{base}/stats"), 400),
    (&admin, format!("{base}/stats?startAt=1"), 400),
    (&admin, format!("{base}/stats?{range}&timezone=Nowhere/City"), 400),
    (&admin, format!("{base}/stats?{range}&unit=week"), 400),
    (&admin, format!("{base}/stats?{range}&compare=never"), 400),
    (&admin, format!("{base}/stats?{range}&segment=nope"), 400),
    (&admin, format!("{base}/stats?{range}&eventType=0"), 400),
    (&admin, format!("{base}/stats?{range}&match=some"), 400),
    (&admin, format!("{base}/metrics?{range}"), 400),
    (&admin, format!("{base}/metrics?type=nope&{range}"), 400),
    (&admin, format!("{base}/metrics?type=path&limit=x&{range}"), 400),
    (&admin, format!("{base}/values?type=segment&{range}"), 400),
    (&outsider, format!("{base}/stats?{range}"), 401),
    (&outsider, format!("{base}/metrics?type=path&{range}"), 401),
    (&outsider, format!("{base}/active"), 401),
    (&outsider, format!("{base}/daterange"), 401),
    // An unknown website reads as empty.
    (&admin, format!("/api/websites/{}/stats?{range}", Uuid::new_v4()), 200),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "{path}: {u:?}");
    assert_same(&u, &r, &[], &path);
  }

  // Parameters are checked before the session.
  let path = format!("{base}/stats?{range}&unit=week");
  let (u, r) = stack.call_both(Method::GET, &path, &[], None).await;
  assert_eq!(u.status, 400);
  assert_same(&u, &r, &[], "invalid parameters without a session");

  // Share links see the sections they enable.
  let share = |parameters: Value| share_token(json!({ "websiteId": data.website, "parameters": parameters }));
  for (token, path, status) in [
    (share(json!({})), format!("{base}/stats?{range}"), 200),
    (
      share(json!({ "overview": true })),
      format!("{base}/metrics?type=path&{range}"),
      200,
    ),
    (share(json!({ "events": true })), format!("{base}/stats?{range}"), 401),
    (
      share(json!({ "events": true })),
      format!("{base}/metrics?type=event&{range}"),
      200,
    ),
    (share(json!({ "overview": false })), format!("{base}/active"), 401),
    (share(json!({ "realtime": true })), format!("{base}/active"), 200),
    (share(json!({ "events": true })), format!("{base}/daterange"), 200),
  ] {
    let headers = [("x-umami-share-token", token.as_str()), ("x-umami-share-context", "1")];
    let (u, r) = stack.call_both(Method::GET, &path, &headers, None).await;
    assert_eq!(u.status, status, "{path}: {u:?}");
    let (mut u, mut r) = (u, r);
    as_sets(&mut u.body);
    as_sets(&mut r.body);
    assert_same(&u, &r, &[], &format!("share link on {path}"));
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn upstream_server_errors() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let base = format!("/api/websites/{}", data.website);
  let range = range(&data, 7);

  // Upstream fails with a 500 for these; umami-rs answers them.
  let dates = format!(
    "startDate={}&endDate={}",
    (data.now - Duration::days(7)).format("%Y-%m-%d"),
    (data.now + Duration::days(1)).format("%Y-%m-%d"),
  );
  for (path, status) in [
    (format!("{base}/stats?{dates}"), 200),
    (format!("{base}/metrics?type=path&limit=-1&{range}"), 400),
    (format!("{base}/stats?{range}&segment={}", Uuid::new_v4()), 400),
    (format!("{base}/stats?{range}&browser=s.chrome"), 400),
    (format!("{base}/stats?{range}&path=re.("), 400),
    (format!("/api/websites/not-a-uuid/stats?{range}"), 404),
  ] {
    let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
    assert_eq!((u.status, r.status), (500, status), "{path}: {u:?} {r:?}");
  }

  // The date form reports the same as the equivalent timestamps.
  let start = (data.now - Duration::days(7))
    .duration_trunc(Duration::days(1))
    .unwrap();
  let end = (data.now + Duration::days(1))
    .duration_trunc(Duration::days(1))
    .unwrap();
  let millis = format!("startAt={}&endAt={}", start.timestamp_millis(), end.timestamp_millis());
  let rs = |path: String| {
    let auth = bearer(&admin[1]);
    let stack = &stack;
    async move {
      stack
        .call(&stack.rs, Method::GET, &path, &[("authorization", &auth)], None)
        .await
    }
  };
  let by_date = rs(format!("{base}/stats?{dates}")).await;
  let by_millis = rs(format!("{base}/stats?{millis}")).await;
  assert_eq!(by_date.body, by_millis.body);

  // `excludeBounce=false` excludes bounces upstream; here it does not.
  let path = format!("{base}/stats?{range}&excludeBounce=false");
  let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
  assert_eq!(u.body["bounces"], 0, "{u:?}");
  let plain = rs(format!("{base}/stats?{range}")).await;
  assert_eq!(r.body, plain.body);
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn events_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let base = format!("/api/websites/{}", data.website);
  let all = range(&data, 45);
  let week = range(&data, 7);

  for query in [
    all.clone(),
    format!("{all}&page=2&pageSize=7"),
    format!("{all}&pageSize=500"),
    format!("{all}&maxResults=10"),
    format!("{all}&search=pricing"),
    format!("{all}&search=SIGN"),
    format!("{week}&browser=chrome&path=c./blog"),
    format!("{all}&eventType=2"),
    format!("{all}&epf0=1.eq.plan.pro"),
    format!("{all}&cohort={}", data.cohort),
  ] {
    for path in [
      format!("{base}/events?{query}"),
      format!("{base}/events/stats?{query}"),
      format!("{base}/events/stats?{query}&compare=yoy"),
    ] {
      let response = compare(&stack, &admin, &path).await;
      assert_eq!(response.status, 200, "{path}: {response:?}");
    }
  }

  // The list's own order matters: pages must line up exactly.
  let path = format!("{base}/events?{all}&page=3&pageSize=10");
  let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
  assert_eq!(u.body["data"].as_array().unwrap().len(), 10, "{u:?}");
  assert_same(&u, &r, &[], &path);

  for query in [
    format!("{all}&timezone=UTC"),
    format!("{all}&timezone=Asia/Tokyo&unit=day"),
    format!("{week}&timezone=America/New_York&unit=hour&limit=1"),
    format!("{all}&timezone=UTC&limit=0"),
    format!("{all}&timezone=UTC&browser=firefox"),
  ] {
    let path = format!("{base}/events/series?{query}");
    let response = compare(&stack, &admin, &path).await;
    assert_eq!(response.status, 200, "{path}: {response:?}");
  }

  for path in [
    // The series needs integer timestamps and a time zone.
    format!("{base}/events/series?{all}"),
    format!("{base}/events/series?startAt=1.5&endAt=2&timezone=UTC"),
    format!("{base}/events/series?startDate=2026-01-01&endDate=2026-02-01&timezone=UTC"),
    format!("{base}/events?{all}&page=0"),
  ] {
    let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
    assert_eq!(u.status, 400, "{path}: {u:?}");
    assert_same(&u, &r, &[], &path);
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn sessions_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let base = format!("/api/websites/{}", data.website);
  let all = range(&data, 45);

  for query in [
    all.clone(),
    format!("{all}&page=2&pageSize=10"),
    format!("{all}&maxResults=5"),
    format!("{all}&search=user"),
    format!("{all}&search=CHROME"),
    format!("{all}&country=DE"),
    format!("{all}&path=/pricing&match=any&browser=safari"),
    format!("{all}&segment={}", data.segment),
  ] {
    for path in [
      format!("{base}/sessions?{query}"),
      format!("{base}/sessions/stats?{query}"),
      format!("{base}/sessions/weekly?{query}&timezone=Asia/Bangkok"),
    ] {
      let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
      assert_eq!(u.status, 200, "{path}: {u:?}");
      assert_same(&u, &r, &[], &path);
    }
  }

  let (u, _) = stack
    .call_as(&admin, Method::GET, &format!("{base}/sessions?{all}"), None)
    .await;
  assert_eq!(u.body["count"], 43, "{u:?}");

  // Session details: plain, identified, sharing an identity, with two.
  let sessions = &data.sessions;
  for session in [sessions[5], sessions[6], sessions[1], sessions[3], sessions[40]] {
    let session_base = format!("{base}/sessions/{session}");
    for path in [
      session_base.clone(),
      format!("{session_base}/properties"),
      format!("{session_base}/activity?{all}"),
      format!("{session_base}/activity?{}", range(&data, 1)),
      format!("{session_base}/activity?{all}&distinctId=shared"),
    ] {
      let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
      assert_eq!(u.status, 200, "{path}: {u:?}");
      assert_same(&u, &r, &[], &path);
    }
  }

  let stitched = |session: Uuid| {
    let path = format!("{base}/sessions/{session}");
    let admin = admin.clone();
    let stack = &stack;
    async move { stack.call_as(&admin, Method::GET, &path, None).await.0.body }
  };
  let shared = stitched(sessions[1]).await;
  assert_eq!(shared["stitchedSessionCount"], 2, "{shared}");
  assert_eq!(shared["distinctId"], "shared");
  let collided = stitched(sessions[3]).await;
  assert!(collided.get("distinctId").is_none(), "{collided}");
  assert_eq!(collided["distinctIds"], json!(["first", "second"]));

  for (path, status) in [
    (format!("{base}/sessions/{}", Uuid::new_v4()), 404),
    (format!("{base}/sessions/stats?{all}&startAt=1.5"), 400),
    (format!("{base}/sessions/weekly?{all}"), 400),
    (format!("{base}/sessions/{}/activity", sessions[0]), 400),
    (
      format!(
        "{base}/sessions/{}/activity?{all}&distinctId={}",
        sessions[0],
        "x".repeat(51)
      ),
      400,
    ),
  ] {
    let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "{path}: {u:?}");
    assert_same(&u, &r, &[], &path);
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn realtime_matches_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;

  for query in ["", "?timezone=Asia/Tokyo", "?unit=hour", "?browser=chrome"] {
    let path = format!("/api/realtime/{}{query}", data.website);
    let (mut u, mut r) = stack.call_as(&admin, Method::GET, &path, None).await;
    assert_eq!(u.status, 200, "{path}: {u:?}");
    for response in [&mut u, &mut r] {
      response.body["timestamp"] = json!(0);
    }
    assert_same(&u, &r, &[], &path);
  }

  let path = format!("/api/realtime/{}", data.website);
  let (u, _) = stack.call_as(&admin, Method::GET, &path, None).await;
  // The three sessions active now, one per country.
  assert_eq!(u.body["totals"]["countries"], 3, "{u:?}");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn deleting_sessions_matches_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let (_, outsider) = stack.session("user").await;
  let session = data.sessions[0];
  let path = format!("/api/websites/{}/sessions/{session}", data.website);

  for (tokens, status) in [(&outsider, 401), (&admin, 200), (&admin, 404)] {
    let (u, r) = stack.call_as(tokens, Method::DELETE, &path, None).await;
    assert_eq!(u.status, status, "{u:?}");
    assert_same(&u, &r, &[], "delete session");
  }

  for sql in [
    "select count(*) from session where session_id = any($1)",
    "select count(*) from website_event where session_id = any($1)",
    "select count(*) from session_data where session_id = any($1)",
    "select count(*) from session_link where session_id = any($1)",
    "select count(*) from event_data where website_event_id in \
           (select event_id from website_event where session_id = any($1))",
  ] {
    let rows = stack
      .assert_same_snapshot(sql, &[session], "after deleting a session")
      .await;
    assert_eq!(rows, json!([{ "count": 0 }]), "{sql}");
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn upstream_session_and_event_quirks() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let base = format!("/api/websites/{}", data.website);
  let all = range(&data, 45);

  // A session seen on two hostnames: listed twice upstream, once here.
  let session = data.sessions[7];
  stack
        .execute(
            "insert into website_event (event_id, website_id, session_id, visit_id, created_at, url_path, url_query, event_type, hostname) \
             select gen_random_uuid(), website_id, session_id, visit_id, created_at + interval '1 second', '/other', '', 1, 'other.example.com' \
             from website_event where session_id = $1[1] order by created_at limit 1",
            &[session],
        )
        .await;
  let path = format!("{base}/sessions?{all}&pageSize=100");
  let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
  let listed = |body: &Value| {
    body["data"]
      .as_array()
      .unwrap()
      .iter()
      .filter(|row| row["id"] == json!(session))
      .count()
  };
  assert_eq!((listed(&u.body), listed(&r.body)), (2, 1));
  assert_eq!(
    (u.body["count"].as_i64(), r.body["count"].as_i64()),
    (Some(44), Some(43))
  );

  // A negative series limit: a 500 upstream, a 400 here.
  let path = format!("{base}/events/series?{all}&timezone=UTC&limit=-1");
  let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
  assert_eq!((u.status, r.status), (500, 400), "{u:?} {r:?}");
}
