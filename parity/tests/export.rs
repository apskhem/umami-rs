//! Website export parity: `/api/websites/{id}/export`'s zip of CSV files, over
//! the seeded analytics dataset and over values that need CSV quoting or
//! formula escaping.
//!
//! The archives themselves differ (each records when it was made), so their
//! files are compared: names in order, and each CSV's header and rows. Upstream
//! breaks ties in the breakdowns arbitrarily, so rows are compared as sets.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use std::io::{
  Cursor,
  Read as _,
};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::Method;
use serde_json::json;
use umami_parity::dataset::{
  range,
  seed,
};
use umami_parity::fixtures::{
  seed_team,
  seed_website,
  share_token,
};
use umami_parity::{
  Response,
  Stack,
  assert_same,
};
use uuid::Uuid;

const FILES: [&str; 7] = [
  "events.csv",
  "pages.csv",
  "referrers.csv",
  "browsers.csv",
  "os.csv",
  "devices.csv",
  "countries.csv",
];

/// The files of an export response, each as its header line and sorted rows.
fn files(response: &Response) -> Vec<(String, String, Vec<String>)> {
  let zip = response.body["zip"]
    .as_str()
    .unwrap_or_else(|| panic!("no zip: {response:?}"));
  let mut archive = zip::ZipArchive::new(Cursor::new(BASE64.decode(zip).unwrap())).unwrap();
  (0..archive.len())
    .map(|index| {
      let mut file = archive.by_index(index).unwrap();
      let mut text = String::new();
      file.read_to_string(&mut text).unwrap();
      // Rows split on CRLF only: quoted values may hold a bare line break.
      let mut lines = text.split("\r\n").map(str::to_owned);
      let header = lines.next().unwrap_or_default();
      let mut rows: Vec<String> = lines.collect();
      rows.sort();
      (file.name().unwrap().into_owned(), header, rows)
    })
    .collect()
}

/// Exports `path` from both servers as `tokens` and asserts the same files.
async fn compare(stack: &Stack, tokens: &[String; 2], path: &str) -> Vec<(String, String, Vec<String>)> {
  let (u, r) = stack.call_as(tokens, Method::GET, path, None).await;
  assert_eq!((u.status, r.status), (200, 200), "{path}: {u:?} {r:?}");
  let (u, r) = (files(&u), files(&r));
  assert_eq!(u, r, "{path}");
  assert_eq!(u.iter().map(|(name, ..)| name.as_str()).collect::<Vec<_>>(), FILES);
  u
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn export_matches_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let export = format!("/api/websites/{}/export", data.website);

  for range in [
    range(&data, 7),
    format!("{}&timezone=Asia/Tokyo&unit=day&compare=prev", range(&data, 45)),
  ] {
    let all = compare(&stack, &admin, &format!("{export}?{range}&format=json")).await;
    for (name, header, rows) in &all {
      assert!(name == "events.csv" || !rows.is_empty(), "{name} is empty");
      assert!(rows.is_empty() || header == "x,y", "{name}: {header}");
    }

    // Filters are not part of upstream's schema, so they change nothing.
    let filtered = format!("{export}?{range}&browser=chrome&path=c./blog&segment={}", data.segment);
    assert_eq!(compare(&stack, &admin, &filtered).await, all, "{filtered}");
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn export_escapes_like_upstream() {
  let stack = Stack::connect().await;
  let (owner, tokens) = stack.session("user").await;
  let website = seed_website(&stack, "Escapes", Some(owner.id), None).await;
  let session = Uuid::new_v4();
  stack
    .execute(
      r#"insert into session (session_id, website_id, browser, os, device, country)
           values ($1[2], $1[1], '=cmd|calc', ' padded os', 'say "hi"', '@X');
         insert into website_event (event_id, website_id, session_id, visit_id, url_path, page_title, referrer_domain,
             hostname, event_type, event_name, created_at)
           values
             (gen_random_uuid(), $1[1], $1[2], $1[2], '/a,b', 'A', 'news.example', 'example.com', 1, null, now()),
             (gen_random_uuid(), $1[1], $1[2], $1[2], '/line' || chr(10) || 'break', 'B', '+ref.example', 'example.com', 1, null, now()),
             (gen_random_uuid(), $1[1], $1[2], $1[2], '/', 'C', '', 'example.com', 2, '-1+1', now()),
             (gen_random_uuid(), $1[1], $1[2], $1[2], '/', 'C', '', 'example.com', 2, e'\ttabbed', now()),
             (gen_random_uuid(), $1[1], $1[2], $1[2], '/', 'C', '', 'example.com', 2, 'trailing ', now())"#,
      &[website, session],
    )
    .await;
  let now = chrono::Utc::now().timestamp_millis();
  let path = format!(
    "/api/websites/{website}/export?startAt={}&endAt={}",
    now - 3_600_000,
    now + 3_600_000
  );

  let files = compare(&stack, &tokens, &path).await;
  // Every file has rows, so each kind of escaping was compared.
  assert!(files.iter().all(|(_, _, rows)| !rows.is_empty()), "{files:?}");
  assert!(
    files
      .iter()
      .any(|(_, _, rows)| rows.contains(&"'=cmd|calc,1".to_owned())),
    "{files:?}"
  );
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn export_errors_and_permissions_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (viewer, viewer_tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&viewer, "team-view-only")]).await;
  let owned = seed_website(&stack, "Owned", Some(owner.id), None).await;
  let team_website = seed_website(&stack, "Team", None, Some(team)).await;
  let range = range(&data, 7);
  let export = |website: Uuid| format!("/api/websites/{website}/export");

  for (tokens, path, status) in [
    (&admin, export(data.website), 400),
    (&admin, format!("{}?startAt=1", export(data.website)), 400),
    (&admin, format!("{}?{range}&pageSize=0", export(data.website)), 400),
    (&admin, format!("{}?{range}&unit=week2", export(data.website)), 400),
    (
      &admin,
      format!("{}?{range}&timezone=Nowhere/City", export(data.website)),
      400,
    ),
    (&outsider, format!("{}?{range}", export(data.website)), 401),
    (&outsider, format!("{}?{range}", export(owned)), 401),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "{path}: {u:?}");
    assert_same(&u, &r, &[], &path);
  }

  // Owners, team members (even view-only) and admins may export; an unknown
  // website exports empty files.
  for (tokens, website) in [
    (&owner_tokens, owned),
    (&viewer_tokens, team_website),
    (&admin, Uuid::new_v4()),
  ] {
    compare(&stack, tokens, &format!("{}?{range}", export(website))).await;
  }

  // Without a session, and with a share link (even one showing everything).
  let path = format!("{}?{range}", export(data.website));
  let token = share_token(json!({ "websiteId": data.website, "parameters": {} }));
  for headers in [
    vec![],
    vec![("x-umami-share-token", token.as_str()), ("x-umami-share-context", "1")],
  ] {
    let (u, r) = stack.call_both(Method::GET, &path, &headers, None).await;
    assert_eq!(u.status, 401, "{headers:?}: {u:?}");
    assert_same(&u, &r, &[], &format!("{path} {headers:?}"));
  }

  // A malformed website ID: upstream fails with a 500; umami-rs answers 404.
  let path = format!("/api/websites/website/export?{range}");
  let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
  assert_eq!((u.status, r.status), (500, 404), "{u:?} {r:?}");
}
