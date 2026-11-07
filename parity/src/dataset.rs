//! The analytics parity tests' dataset: a website with 43 sessions over 45
//! days, seeded identically into both databases.

use std::collections::HashSet;
use std::fmt::Write as _;

use chrono::{
  DateTime,
  Duration,
  DurationRound,
  Utc,
};
use serde_json::Value;
use uuid::Uuid;

use crate::Stack;

/// A tiny deterministic generator, so the dataset is varied but repeatable.
pub struct Lcg(pub u64);

impl Lcg {
  pub fn next(&mut self, n: usize) -> usize {
    self.0 = self
      .0
      .wrapping_mul(6364136223846793005)
      .wrapping_add(1442695040888963407);
    ((self.0 >> 33) % n as u64) as usize
  }

  pub fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
    items[self.next(items.len())]
  }
}

pub fn sql_text(value: Option<&str>) -> String {
  value.map_or("null".to_owned(), |v| format!("'{}'", v.replace('\'', "''")))
}

pub fn sql_time(at: DateTime<Utc>) -> String {
  format!("'{}'", at.to_rfc3339())
}

pub struct Dataset {
  pub website: Uuid,
  pub segment: Uuid,
  pub cohort: Uuid,
  pub now: DateTime<Utc>,
  /// The sessions, in order: 1 and 2 share an identity, 3 has two, and
  /// 40-42 are active now.
  pub sessions: Vec<Uuid>,
}

/// Seeds a website with 40 sessions over 45 days (a few active right now), with
/// pageviews, custom events with data, performance events, session data, a
/// segment and a cohort.
pub async fn seed(stack: &Stack) -> Dataset {
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
        if name == "purchase" {
          // Revenue, mostly in USD (recorded in either case).
          let currency = ["USD", "usd", "EUR"][s % 3];
          writeln!(
                    sql,
                    "insert into revenue (revenue_id, website_id, session_id, event_id, event_name, currency, revenue, created_at) \
                     values (gen_random_uuid(), '{website}', '{session}', '{event}', '{name}', '{currency}', {}, {});",
                    5 + (s * 7) % 40,
                    sql_time(at),
                )
                .unwrap();
        }
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
pub fn as_sets(value: &mut Value) {
  match value {
    Value::Array(items) => {
      items.iter_mut().for_each(as_sets);
      items.sort_by_key(Value::to_string);
    }
    Value::Object(object) => object.values_mut().for_each(as_sets),
    _ => {}
  }
}

/// `startAt`/`endAt` for the last `days` days.
pub fn range(data: &Dataset, days: i64) -> String {
  let end = data.now + Duration::minutes(1);
  let start = end - Duration::days(days);
  format!("startAt={}&endAt={}", start.timestamp_millis(), end.timestamp_millis())
}
