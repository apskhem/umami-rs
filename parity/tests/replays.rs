//! Session replay parity: `/recorder.js`, the recorder's settings, recording
//! through `/api/record`, and reading recordings back (`replays`, a session's
//! replays, one replay, saved replays) and `heatmaps`.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use chrono::{
  Duration,
  Utc,
};
use reqwest::Method;
use serde_json::{
  Value,
  json,
};
use umami_parity::fixtures::{
  Entity,
  secret,
  seed_entity,
  seed_team,
  timestamps,
};
use umami_parity::{
  Stack,
  assert_same,
  assert_same_by,
  mask,
};
use umami_server::collect::CacheToken;
use uuid::Uuid;

const CHROME: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) \
                      Chrome/120.0.0.0 Safari/537.36";

/// A website with the recorder enabled as `config` says, and a session on it,
/// in both databases.
struct Recorded {
  website: Uuid,
  session: Uuid,
  visit: Uuid,
  token: String,
}

async fn seed_recorded(stack: &Stack, owner: Uuid, config: &Value) -> Recorded {
  let website = seed_entity(stack, Entity::Website, "Recorded", Some(owner), None).await;
  let (session, visit) = (Uuid::new_v4(), Uuid::new_v4());
  stack
    .execute(
      &format!(
        "update website set recorder_enabled = true, replay_config = '{config}' where website_id = $1[1]; \
         insert into session (session_id, website_id, browser, os, device, country, city) \
         values ($1[2], $1[1], 'chrome', 'Mac OS', 'laptop', 'TH', 'Bangkok'); \
         insert into website_event (event_id, website_id, session_id, visit_id, url_path, event_type, created_at) \
         values (gen_random_uuid(), $1[1], $1[2], $1[3], '/pricing', 1, now())"
      ),
      &[website, session, visit],
    )
    .await;
  let token = CacheToken::new(
    Some(website.to_string()),
    session.to_string(),
    visit.to_string(),
    Utc::now().timestamp(),
    None,
  )
  .sign(&secret());

  Recorded {
    website,
    session,
    visit,
    token,
  }
}

/// Posts `body` to `/api/record` on both servers, as a browser with `token`.
async fn record(
  stack: &Stack,
  token: &str,
  ip: &str,
  body: &Value,
) -> (umami_parity::Response, umami_parity::Response) {
  let headers = [
    ("x-umami-cache", token),
    ("user-agent", CHROME),
    ("x-forwarded-for", ip),
  ];
  stack.call_both(Method::POST, "/api/record", &headers, Some(body)).await
}

fn replay(website: Uuid, timestamp: i64, events: Value) -> Value {
  json!({ "type": "record", "payload": { "website": website, "timestamp": timestamp, "events": events } })
}

fn heatmap(website: Uuid, events: Value) -> Value {
  json!({ "type": "heatmap", "payload": { "website": website, "events": events } })
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn recorder_script_and_settings_match_upstream() {
  let stack = Stack::connect().await;
  let owner = stack.create_user("user").await;

  let u = stack
    .http()
    .get(format!("{}/recorder.js", stack.upstream.url))
    .send()
    .await
    .unwrap();
  let r = stack
    .http()
    .get(format!("{}/recorder.js", stack.rs.url))
    .send()
    .await
    .unwrap();
  assert_eq!(u.status(), r.status());
  for name in ["cache-control", "access-control-allow-origin"] {
    assert_eq!(u.headers().get(name), r.headers().get(name), "{name}");
  }
  let content_type = |response: &reqwest::Response| {
    response.headers()[reqwest::header::CONTENT_TYPE]
      .to_str()
      .unwrap()
      .to_lowercase()
  };
  assert_eq!(content_type(&u), content_type(&r));
  assert_eq!(u.bytes().await.unwrap(), r.bytes().await.unwrap());

  let plain = seed_entity(&stack, Entity::Website, "Plain", Some(owner.id), None).await;
  let defaults = seed_recorded(&stack, owner.id, &json!({ "replayEnabled": true })).await;
  let custom = seed_recorded(
    &stack,
    owner.id,
    &json!({ "heatmapEnabled": true, "sampleRate": 1, "heatmapSampleRate": 0.5, "maskLevel": "strict",
             "maxDuration": 60000, "blockSelector": ".secret", "unknown": 1 }),
  )
  .await;
  for website in [
    plain.to_string(),
    defaults.website.to_string(),
    custom.website.to_string(),
    Uuid::new_v4().to_string(),
    "website".to_owned(),
  ] {
    let path = format!("/api/websites/{website}/recorder");
    let (u, r) = stack.call_both(Method::GET, &path, &[], None).await;
    assert_same(&u, &r, &[], &path);
  }

  // A deleted website: upstream still configures the recorder; umami-rs
  // disables it.
  stack
    .execute(
      "update website set deleted_at = now() where website_id = $1[1]",
      &[defaults.website],
    )
    .await;
  let path = format!("/api/websites/{}/recorder", defaults.website);
  let (u, r) = stack.call_both(Method::GET, &path, &[], None).await;
  assert_eq!(u.body["enabled"], true, "{u:?}");
  assert_eq!((r.status, r.body.clone()), (200, json!({ "enabled": false })));
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn recording_matches_upstream() {
  let stack = Stack::connect().await;
  let (owner, tokens) = stack.session("user").await;
  let both = seed_recorded(
    &stack,
    owner.id,
    &json!({ "replayEnabled": true, "heatmapEnabled": true }),
  )
  .await;
  let replay_only = seed_recorded(&stack, owner.id, &json!({ "replayEnabled": true })).await;
  let heatmap_only = seed_recorded(&stack, owner.id, &json!({ "heatmapEnabled": true })).await;
  let disabled = seed_entity(&stack, Entity::Website, "Off", Some(owner.id), None).await;
  let ip = "203.0.113.20";
  let now = Utc::now();
  let ms = |seconds: i64| (now - Duration::seconds(seconds)).timestamp_millis();
  let w = both.website;

  // A full snapshot and meta first, then incremental events, one sent in two
  // fragments, and one sent out of order in a later chunk.
  let first = json!([
      { "type": 4, "timestamp": ms(60), "data": { "href": "https://example.com/", "width": 1280, "height": 800 } },
      { "type": 2, "timestamp": ms(60), "data": { "node": { "type": 0, "childNodes": [] }, "initialOffset": { "top": 0.0, "left": 1.5 } } },
      { "type": 3, "timestamp": ms(55), "data": { "source": 1, "positions": [{ "x": 10, "y": 20.25, "id": 1, "timeOffset": -12 }] } },
      { "type": "umami:rrweb-event-fragment", "data": { "id": "f1", "index": 0, "total": 2, "value": "{\"type\":3,\"timestamp\":" } },
      { "type": "umami:rrweb-event-fragment", "data": { "id": "f1", "index": 1, "total": 2, "value": format!("{},\"data\":{{\"source\":2}}}}", ms(50)) } },
  ]);
  let second = json!([
      { "type": 3, "timestamp": ms(40), "data": { "source": 3, "y": 400 } },
      { "type": 3, "timestamp": ms(52), "data": { "source": 2, "late": true } },
      { "type": 5, "data": { "tag": "no timestamp" } },
  ]);
  let chunk_time = |seconds: i64| (now - Duration::seconds(seconds)).timestamp();
  // Tokens for the same session on other websites.
  let token_for = |website: Uuid| {
    CacheToken::new(
      Some(website.to_string()),
      both.session.to_string(),
      both.visit.to_string(),
      now.timestamp(),
      None,
    )
    .sign(&secret())
  };
  let missing = Uuid::new_v4();
  let (disabled_token, missing_token) = (token_for(disabled), token_for(missing));

  for (token, body, status) in [
    (both.token.as_str(), json!({ "type": "replay", "payload": {} }), 400),
    (
      both.token.as_str(),
      json!({ "type": "record", "payload": { "website": "x", "events": [] } }),
      400,
    ),
    (
      both.token.as_str(),
      replay(w, 1, Value::Array(vec![json!({}); 201])),
      400,
    ),
    (both.token.as_str(), replay(w, 1, json!([])), 200),
    ("", replay(w, 1, json!([{ "type": 3 }])), 400),
    ("not-a-token", replay(w, 1, json!([{ "type": 3 }])), 400),
    (
      disabled_token.as_str(),
      replay(disabled, 1, json!([{ "type": 3 }])),
      200,
    ),
    (missing_token.as_str(), replay(missing, 1, json!([{ "type": 3 }])), 400),
    (
      heatmap_only.token.as_str(),
      replay(heatmap_only.website, 1, json!([{ "type": 3 }])),
      200,
    ),
    (
      replay_only.token.as_str(),
      heatmap(replay_only.website, json!([{ "type": "click", "url": "/" }])),
      200,
    ),
    (
      both.token.as_str(),
      heatmap(w, json!([{ "type": "hover", "url": "/" }])),
      400,
    ),
    (
      both.token.as_str(),
      heatmap(w, json!([{ "type": "click", "url": "/", "x": "x" }])),
      400,
    ),
    (both.token.as_str(), replay(w, chunk_time(60), first.clone()), 200),
    (both.token.as_str(), replay(w, chunk_time(30), second.clone()), 200),
    (
      both.token.as_str(),
      heatmap(
        w,
        json!([
            { "type": "click", "url": "https://example.com/pricing?plan=pro#top", "x": 100.4, "y": "250.5", "pageX": 100, "pageY": 1250,
              "pageW": 1280, "pageH": 3000, "viewportW": 1280, "viewportH": 800, "timestamp": ms(20) },
            { "type": "click", "url": "/pricing", "x": 100, "y": 250, "pageX": 100, "pageY": 1250,
              "pageW": 1280, "pageH": 3000, "viewportW": 1280, "viewportH": 800, "timestamp": ms(19), "scrollPct": 50 },
            { "type": "click", "url": "/about", "x": 5, "y": 6, "pageX": 5, "pageY": 6,
              "pageW": 390, "pageH": 2000, "viewportW": 390, "viewportH": 844, "timestamp": ms(18) },
            { "type": "click", "url": "/pricing", "x": 7, "timestamp": ms(17) },
            { "type": "scroll", "url": "/pricing", "scrollPct": 64.6, "pageW": 1280, "pageH": 3000,
              "viewportW": 1280, "viewportH": 800, "timestamp": ms(16), "x": 9 },
            { "type": "scroll", "url": "/pricing", "scrollPct": 140, "pageW": 1280, "pageH": 3000,
              "viewportW": 1280, "viewportH": 800, "timestamp": ms(15) },
            { "type": "scroll", "url": "relative", "scrollPct": -3, "pageW": 390, "pageH": 2000,
              "viewportW": 390, "viewportH": 844, "timestamp": ms(14) },
        ]),
      ),
      200,
    ),
  ] {
    let (u, r) = record(&stack, token, ip, &body).await;
    assert_eq!(u.status, status, "{body}: {u:?}");
    assert_same(&u, &r, &[], &format!("record {body}"));
  }

  // A token from another website: upstream records under its session anyway;
  // umami-rs refuses it.
  let (u, r) = record(&stack, &disabled_token, ip, &replay(w, 1, json!([{ "type": 3 }]))).await;
  assert_eq!((u.status, r.status), (200, 400), "{u:?} {r:?}");
  stack
    .execute(
      "delete from session_replay where website_id = $1[1] and chunk_index = 1",
      &[w],
    )
    .await;

  // Bots are answered without recording; ignored IPs are refused.
  let bot = [("x-umami-cache", both.token.as_str()), ("user-agent", "Googlebot/2.1")];
  let (u, r) = stack
    .call_both(
      Method::POST,
      "/api/record",
      &bot,
      Some(&replay(w, 1, json!([{ "type": 3 }]))),
    )
    .await;
  assert_same(&u, &r, &[], "bot");
  let (u, r) = record(
    &stack,
    &both.token,
    "198.51.100.66",
    &replay(w, 1, json!([{ "type": 3 }])),
  )
  .await;
  assert_eq!(u.status, 403, "{u:?}");
  assert_same(&u, &r, &[], "ignored IP");

  // Oversized requests.
  let big = replay(w, 1, json!([{ "type": 3, "data": "x".repeat(1_100_000) }]));
  let (u, r) = record(&stack, &both.token, ip, &big).await;
  assert_eq!(u.status, 413, "{u:?}");
  assert_same(&u, &r, &[], "too large");

  stack
    .assert_same_snapshot(
      "select website_id, session_id, visit_id, chunk_index, event_count, started_at, ended_at \
       from session_replay where website_id = any($1) order by chunk_index",
      &[w, heatmap_only.website],
      "recordings",
    )
    .await;
  stack
    .assert_same_snapshot(
      "select website_id, session_id, visit_id, url_path, event_type, x, y, page_x, page_y, page_w, \
       viewport_w, viewport_h, page_h, scroll_pct, created_at from heatmap_event \
       where website_id = any($1) order by created_at",
      &[w, replay_only.website],
      "heatmap events",
    )
    .await;

  // Reading the recording back, whole and up to a point.
  let replay_path = format!("/api/websites/{w}/replays/{}", both.visit);
  for query in [
    String::new(),
    format!("?until={}", ms(53)),
    format!("?chunkIndex={}&eventIndex=1", chunk_time(60)),
    "?chunkIndex=x&eventIndex=".to_owned(),
  ] {
    let path = format!("{replay_path}{query}");
    let (u, r) = stack.call_as(&tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, 200, "{path}: {u:?}");
    assert_same(&u, &r, &[], &path);
  }
  let (u, _) = stack.call_as(&tokens, Method::GET, &replay_path, None).await;
  assert_eq!(u.body["eventCount"], 7, "{u:?}");

  // A deleted website records nothing on umami-rs.
  let deleted = seed_recorded(&stack, owner.id, &json!({ "replayEnabled": true })).await;
  stack
    .execute(
      "update website set deleted_at = now() where website_id = $1[1]",
      &[deleted.website],
    )
    .await;
  let (u, r) = record(
    &stack,
    &deleted.token,
    ip,
    &replay(deleted.website, 1, json!([{ "type": 3 }])),
  )
  .await;
  assert_eq!((u.status, r.status), (200, 400), "{u:?} {r:?}");
  let recordings = "select count(*) as count from session_replay where website_id = any($1)";
  assert_eq!(
    stack.rs.snapshot(recordings, &[deleted.website]).await,
    json!([{ "count": 0 }])
  );

  // A heatmap batch with a URL path longer than its column and a coordinate
  // too large for one: upstream drops the batch (still answering 200);
  // umami-rs truncates the path and leaves the coordinate empty.
  let edge = seed_recorded(&stack, owner.id, &json!({ "heatmapEnabled": true })).await;
  let long_url = format!("/{}", "a".repeat(600));
  let events = json!([
      { "type": "click", "url": long_url, "x": 1, "y": 2, "timestamp": ms(10) },
      { "type": "click", "url": "/", "x": 1e12, "y": 3, "timestamp": ms(9) },
  ]);
  let (u, r) = record(&stack, &edge.token, ip, &heatmap(edge.website, events)).await;
  assert_same(&u, &r, &[], "oversized heatmap values");
  let rows =
    "select length(url_path) as length, x, y from heatmap_event where website_id = any($1) order by created_at";
  assert_eq!(stack.upstream.snapshot(rows, &[edge.website]).await, json!([]));
  assert_eq!(
    stack.rs.snapshot(rows, &[edge.website]).await,
    json!([{ "length": 500, "x": 1, "y": 2 }, { "length": 1, "x": null, "y": 3 }])
  );
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn replays_and_heatmaps_match_upstream() {
  let stack = Stack::connect().await;
  let (_, admin) = stack.session("admin").await;
  let (owner, tokens) = stack.session("user").await;
  let (viewer, viewer_tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&viewer, "team-view-only")]).await;
  let rec = seed_recorded(
    &stack,
    owner.id,
    &json!({ "replayEnabled": true, "heatmapEnabled": true }),
  )
  .await;
  stack
    .execute(
      "update website set team_id = $1[1], user_id = null where website_id = $1[2]",
      &[team, rec.website],
    )
    .await;
  let w = rec.website;
  let now = Utc::now();
  let ms = |seconds: i64| (now - Duration::seconds(seconds)).timestamp_millis();

  // The same recording and heatmap events in both databases.
  for (seconds, chunk) in [(300, 1), (200, 2)] {
    let events = json!([
        { "type": 4, "timestamp": ms(seconds), "data": { "width": 1280, "height": 800 } },
        { "type": 3, "timestamp": ms(seconds - 30) },
    ]);
    stack
      .execute(
        &format!(
          "insert into session_replay (replay_id, website_id, session_id, visit_id, chunk_index, events, event_count, started_at, ended_at, created_at) \
           values (gen_random_uuid(), $1[1], $1[2], $1[3], {chunk}, \
             decode('{}', 'hex'), 2, to_timestamp({}), to_timestamp({}), now() - interval '{seconds} seconds')",
          gzip_hex(&events.to_string()),
          ms(seconds) as f64 / 1000.0,
          ms(seconds - 30) as f64 / 1000.0,
        ),
        &[w, rec.session, rec.visit],
      )
      .await;
  }
  for (path, kind, x, scroll, viewport) in [
    ("/pricing", 1, 10, None, (1280, 800)),
    ("/pricing", 1, 10, None, (1280, 800)),
    ("/pricing", 1, 20, None, (390, 844)),
    ("/", 1, 5, None, (1280, 800)),
    ("/pricing", 2, 0, Some(45), (1280, 800)),
    ("/pricing", 2, 0, Some(95), (390, 844)),
  ] {
    let (x, y) = if kind == 1 {
      (format!("{x}"), "30".to_owned())
    } else {
      ("null".to_owned(), "null".to_owned())
    };
    let scroll = scroll.map_or("null".to_owned(), |s: i32| s.to_string());
    stack
      .execute(
        &format!(
          "insert into heatmap_event (heatmap_event_id, website_id, session_id, visit_id, url_path, event_type, \
             x, y, page_x, page_y, page_w, page_h, viewport_w, viewport_h, scroll_pct, created_at) \
           values (gen_random_uuid(), $1[1], $1[2], $1[3], '{path}', {kind}, {x}, {y}, {x}, {y}, {w}, 3000, {w}, {h}, {scroll}, now())",
          w = viewport.0,
          h = viewport.1,
        ),
        &[w, rec.session, rec.visit],
      )
      .await;
  }

  let range = format!(
    "startAt={}&endAt={}",
    (now - Duration::days(1)).timestamp_millis(),
    (now + Duration::hours(1)).timestamp_millis()
  );
  let generated = |v: &mut Value| mask(v, &["createdAt"]);
  for (tokens, path, status) in [
    (&tokens, format!("/api/websites/{w}/replays?{range}"), 401),
    (&viewer_tokens, format!("/api/websites/{w}/replays?{range}"), 200),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/replays?{range}&minDuration=40"),
      200,
    ),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/replays?{range}&minDuration=100"),
      200,
    ),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/replays?{range}&search=bang&browser=chrome"),
      200,
    ),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/replays?{range}&path=/nowhere"),
      200,
    ),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/replays?{range}&minDuration=-1"),
      400,
    ),
    (&viewer_tokens, format!("/api/websites/{w}/replays"), 400),
    (
      &admin,
      format!("/api/websites/{w}/sessions/{}/replays?{range}", rec.session),
      200,
    ),
    (
      &admin,
      format!("/api/websites/{w}/sessions/{}/replays?{range}", Uuid::new_v4()),
      200,
    ),
    (&outsider, format!("/api/websites/{w}/replays/{}", rec.visit), 401),
    (&viewer_tokens, format!("/api/websites/{w}/replays/{}", rec.visit), 200),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/replays/{}", Uuid::new_v4()),
      200,
    ),
    (&viewer_tokens, format!("/api/websites/{w}/heatmaps?{range}"), 200),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/heatmaps?{range}&urlPath=/pricing"),
      200,
    ),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/heatmaps?{range}&urlPath=/pricing&mode=scroll"),
      200,
    ),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/heatmaps?{range}&path=c.pric"),
      200,
    ),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/heatmaps?{range}&urlPath=/pricing&browser=firefox"),
      200,
    ),
    (
      &viewer_tokens,
      format!("/api/websites/{w}/heatmaps?{range}&mode=hover"),
      400,
    ),
    (&outsider, format!("/api/websites/{w}/heatmaps?{range}"), 401),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "GET {path}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("GET {path}"));
  }

  // Saving a replay: upstream fails with a 500 when saving it a second time;
  // umami-rs renames it.
  let saved = format!("/api/websites/{w}/replays/saved/{}", rec.visit);
  let generated = |v: &mut Value| mask(v, &["id", "createdAt", "updatedAt"]);
  for (tokens, method, path, body, status) in [
    (&viewer_tokens, Method::GET, saved.clone(), None, 200),
    (
      &viewer_tokens,
      Method::POST,
      saved.clone(),
      Some(json!({ "isSaved": true })),
      401,
    ),
    (
      &admin,
      Method::POST,
      saved.clone(),
      Some(json!({ "isSaved": "yes" })),
      400,
    ),
    (
      &admin,
      Method::POST,
      saved.clone(),
      Some(json!({ "isSaved": true, "name": "n".repeat(101) })),
      400,
    ),
    (
      &admin,
      Method::POST,
      saved.clone(),
      Some(json!({ "isSaved": true, "name": "Checkout" })),
      200,
    ),
    (&viewer_tokens, Method::GET, saved.clone(), None, 200),
    (
      &viewer_tokens,
      Method::GET,
      format!("/api/websites/{w}/replays/saved"),
      None,
      200,
    ),
    (
      &viewer_tokens,
      Method::GET,
      format!("/api/websites/{w}/replays/saved?search=CHECK"),
      None,
      200,
    ),
    (
      &outsider,
      Method::GET,
      format!("/api/websites/{w}/replays/saved"),
      None,
      401,
    ),
  ] {
    let (u, r) = stack.call_as(tokens, method.clone(), &path, body.as_ref()).await;
    assert_eq!(u.status, status, "{method} {path} {body:?}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("{method} {path}"));
  }
  let rename = json!({ "isSaved": true, "name": "Renamed" });
  let (u, r) = stack.call_as(&admin, Method::POST, &saved, Some(&rename)).await;
  assert_eq!((u.status, r.status), (500, 200), "{u:?} {r:?}");

  let unsave = json!({ "isSaved": false });
  let (u, r) = stack.call_as(&admin, Method::POST, &saved, Some(&unsave)).await;
  assert_same_by(&u, &r, timestamps, "unsave");
  let (u, r) = stack.call_as(&viewer_tokens, Method::GET, &saved, None).await;
  assert_eq!(u.body, json!({ "isSaved": false }));
  assert_same(&u, &r, &[], "after unsaving");

  // Malformed IDs: upstream fails with a 500; umami-rs answers 404.
  for (method, path, body) in [
    (Method::GET, format!("/api/websites/{w}/replays/visit"), None),
    (
      Method::GET,
      format!("/api/websites/{w}/sessions/session/replays?{range}"),
      None,
    ),
    (Method::GET, format!("/api/websites/{w}/replays/saved/visit"), None),
    (
      Method::POST,
      format!("/api/websites/{w}/replays/saved/visit"),
      Some(json!({ "isSaved": true })),
    ),
  ] {
    let (u, r) = stack.call_as(&admin, method.clone(), &path, body.as_ref()).await;
    assert_eq!((u.status, r.status), (500, 404), "{method} {path}: {u:?} {r:?}");
  }
}

/// Gzips `text` and returns it as hex, as upstream stores recordings.
fn gzip_hex(text: &str) -> String {
  use std::io::Write as _;

  let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
  encoder.write_all(text.as_bytes()).unwrap();
  hex::encode(encoder.finish().unwrap())
}
