//! Report parity: saved funnels and goals (with their stats), journeys,
//! retention, breakdowns, UTM, attribution, performance and revenue, over the
//! analytics dataset.
//!
//! Upstream breaks ties in its lists arbitrarily (umami-rs by name), so lists
//! are compared as sets.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use reqwest::Method;
use serde_json::{
  Value,
  json,
};
use umami_parity::dataset::{
  Dataset,
  as_sets,
  range,
  seed,
};
use umami_parity::fixtures::{
  Entity,
  seed_entity,
  seed_team,
  share_token,
  timestamps,
};
use umami_parity::{
  Response,
  Stack,
  assert_same,
  assert_same_by,
  mask,
};
use uuid::Uuid;

/// Requests `path` from both servers and compares the responses, lists as
/// sets.
async fn compare(stack: &Stack, tokens: &[String; 2], path: &str) -> Response {
  let (u, r) = stack.call_as(tokens, Method::GET, path, None).await;
  let (mut u_sets, mut r_sets) = (u.clone(), r.clone());
  as_sets(&mut u_sets.body);
  as_sets(&mut r_sets.body);
  assert_same(&u_sets, &r_sets, &[], path);
  u
}

/// Percent-encodes a query parameter value.
fn encode(value: &str) -> String {
  value
    .bytes()
    .map(|b| {
      if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
        (b as char).to_string()
      } else {
        format!("%{b:02X}")
      }
    })
    .collect()
}

/// Whether a response holds results: a non-empty list, or an object with one.
fn has_results(body: &Value) -> bool {
  match body {
    Value::Array(items) => !items.is_empty(),
    Value::Object(object) => {
      object.values().any(|value| {
        match value {
          Value::Array(items) => !items.is_empty(),
          Value::Number(n) => n.as_f64() != Some(0.0),
          Value::String(_) | Value::Object(_) => true,
          _ => false,
        }
      })
    }
    _ => false,
  }
}

/// `path` under the dataset's website, over its last 30 days, with `query`.
fn report(data: &Dataset, path: &str, query: &str) -> String {
  format!("/api/websites/{}/{path}?{}&{query}", data.website, range(data, 30))
}

/// Creates the same saved report in both databases.
async fn seed_report(stack: &Stack, kind: &str, name: &str, website: Uuid, user: Uuid, parameters: &Value) -> Uuid {
  let id = Uuid::new_v4();
  stack
    .execute(
      &format!(
        "insert into report (report_id, user_id, website_id, type, name, description, parameters, created_at) \
         values ($1[1], $1[2], $1[3], '{kind}', '{name}', '', '{parameters}', '2026-01-02T03:04:05.678Z')"
      ),
      &[id, user, website],
    )
    .await;
  id
}

fn funnel_steps() -> Value {
  json!([
      { "type": "path", "value": "/" },
      { "type": "path", "value": "/pricing" },
      { "type": "event", "value": "signup" },
  ])
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn report_stats_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let steps = |steps: Value| encode(&steps.to_string());

  for (path, query) in [
    ("funnels/stats", format!("window=60&steps={}", steps(funnel_steps()))),
    (
      "funnels/stats",
      format!(
        "window=1440&steps={}&browser=chrome",
        steps(json!([
            { "type": "path", "value": "/blog/*" },
            { "type": "event", "value": "purchase", "filters": [{ "property": "plan", "operator": "eq", "value": "pro" }] },
        ]))
      ),
    ),
    (
      "funnels/stats",
      format!(
        "window=0.5&steps={}",
        steps(json!([
            { "type": "path", "value": "*" },
            { "type": "event", "value": "*", "filters": [{ "property": "plan", "operator": "c", "value": "r" }] },
            { "type": "path", "value": "/nowhere" },
        ]))
      ),
    ),
    ("goals/stats", "type=path&value=/pricing".to_owned()),
    ("goals/stats", "type=event&value=signup&os=Mac OS".to_owned()),
    ("goals/stats", "type=path&value=/blog/*&eventType=2".to_owned()),
    ("journeys", "steps=3".to_owned()),
    ("journeys", "steps=5&startStep=/".to_owned()),
    ("journeys", "steps=4&endStep=/pricing&device=mobile".to_owned()),
    ("retention", "".to_owned()),
    ("retention", "timezone=Asia/Bangkok".to_owned()),
    ("breakdown", "fields=[\"path\"]".to_owned()),
    ("breakdown", "fields=[\"browser\",\"country\"]".to_owned()),
    ("breakdown", "fields=[\"event\",\"path\"]&excludeBounce=true".to_owned()),
    ("utm/metrics", "type=utm_source".to_owned()),
    ("utm/metrics", "type=utm_medium&path=/".to_owned()),
    ("attribution", "model=first-click&type=path&step=/pricing".to_owned()),
    ("attribution", "model=last-click&type=event&step=signup".to_owned()),
    ("performance/chart", "".to_owned()),
    (
      "performance/chart",
      "metric=fcp&unit=hour&timezone=Asia/Tokyo".to_owned(),
    ),
    ("performance/metrics", "type=path".to_owned()),
    ("performance/metrics", "type=device&metric=fcp".to_owned()),
    ("performance/metrics", "type=browser&limit=2".to_owned()),
    ("performance/stats", "".to_owned()),
    ("revenue/chart", "currency=USD".to_owned()),
    (
      "revenue/chart",
      "currency=EUR&unit=day&timezone=America/New_York".to_owned(),
    ),
    ("revenue/stats", "currency=USD".to_owned()),
    ("revenue/stats", "currency=USD&compare=yoy&browser=chrome".to_owned()),
    ("revenue/metrics", "currency=USD&type=country".to_owned()),
    ("revenue/metrics", "currency=USD&type=region".to_owned()),
    ("revenue/metrics", "currency=USD&type=referrer".to_owned()),
    ("revenue/metrics", "currency=USD&type=channel".to_owned()),
    ("revenue/sessions", "currency=USD".to_owned()),
    ("revenue/sessions", "currency=USD&pageSize=2&page=2&search=a".to_owned()),
  ] {
    let path = report(&data, path, &query);
    let response = compare(&stack, &admin, &path).await;
    assert_eq!(response.status, 200, "{path}: {response:?}");
    assert!(has_results(&response.body), "{path}: {response:?}");
  }

  // Segments, cohorts and the explicit date range work on reports too.
  for query in [format!("segment={}", data.segment), format!("cohort={}", data.cohort)] {
    for path in ["journeys?steps=3", "retention?", "breakdown?fields=[\"path\"]"] {
      let (path, extra) = path.split_once('?').unwrap();
      let path = report(&data, path, &format!("{extra}&{query}"));
      let response = compare(&stack, &admin, &path).await;
      assert_eq!(response.status, 200, "{path}: {response:?}");
    }
  }
  // Upstream fails with a 500 on a field named twice; umami-rs groups by it once.
  let path = report(&data, "breakdown", "fields=[\"path\",\"path\"]");
  let (u, r) = stack.call_as(&admin, Method::GET, &path, None).await;
  assert_eq!((u.status, r.status), (500, 200), "{path}: {u:?} {r:?}");

  // Upstream fails with a 500 on `startDate`/`endDate`, which umami-rs accepts.
  let dates = format!(
    "/api/websites/{}/revenue/stats?currency=USD&startDate={}&endDate={}",
    data.website,
    (data.now - chrono::Duration::days(20)).format("%Y-%m-%d"),
    data.now.format("%Y-%m-%dT%H:%M:%SZ"),
  );
  let (u, r) = stack.call_as(&admin, Method::GET, &dates, None).await;
  assert_eq!((u.status, r.status), (500, 200), "{dates}: {u:?} {r:?}");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn saved_funnels_and_goals_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (admin_user, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (member, member_tokens) = stack.session("user").await;
  let (viewer, viewer_tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-member"), (&viewer, "team-view-only")]).await;
  let own = seed_entity(&stack, Entity::Website, "Own", Some(owner.id), None).await;
  let team_site = seed_entity(&stack, Entity::Website, "Team", None, Some(team)).await;
  let generated = |v: &mut Value| mask(v, &["id", "createdAt", "updatedAt"]);
  let funnel = json!({ "window": "30", "steps": funnel_steps() });

  for (kind, parameters) in [
    ("funnels", funnel.clone()),
    ("goals", json!({ "type": "path", "value": "/" })),
  ] {
    let base = |site: Uuid| format!("/api/websites/{site}/{kind}");

    for body in [
      json!({ "parameters": parameters }),
      json!({ "name": "", "parameters": parameters }),
      json!({ "name": "n".repeat(201), "parameters": parameters }),
      json!({ "name": "N", "description": "d".repeat(501), "parameters": parameters }),
      json!({ "name": "N" }),
      json!({ "name": "N", "parameters": { "window": 0, "steps": funnel_steps(), "type": 1 } }),
    ] {
      let (u, r) = stack
        .call_as(&owner_tokens, Method::POST, &base(own), Some(&body))
        .await;
      assert_eq!(u.status, 400, "{kind} {body}: {u:?}");
      assert_same(&u, &r, &[], &format!("create {kind} {body}"));
    }

    for (n, (tokens, site, status)) in [
      (&owner_tokens, own, 200),
      (&outsider, own, 401),
      (&viewer_tokens, team_site, 401),
      (&member_tokens, team_site, 200),
      (&admin, own, 200),
    ]
    .into_iter()
    .enumerate()
    {
      let body = json!({ "name": format!("Saved {n}"), "description": "About", "parameters": parameters, "extra": 1 });
      let (u, r) = stack.call_as(tokens, Method::POST, &base(site), Some(&body)).await;
      assert_eq!(u.status, status, "create {kind} on {site}: {u:?}");
      assert_same_by(&u, &r, generated, &format!("create {kind}"));
    }

    // Upstream saves a report of a website that does not exist.
    let body = json!({ "name": "Nowhere", "parameters": parameters });
    let (u, r) = stack
      .call_as(&admin, Method::POST, &base(Uuid::new_v4()), Some(&body))
      .await;
    assert_eq!((u.status, r.status), (200, 404), "{u:?} {r:?}");

    for (tokens, path, status) in [
      (&owner_tokens, base(own), 200),
      (
        &owner_tokens,
        format!("{}?search=SAV&pageSize=1&page=2", base(own)),
        200,
      ),
      (&owner_tokens, format!("{}?page=0", base(own)), 400),
      (&viewer_tokens, base(team_site), 200),
      (&outsider, base(own), 401),
    ] {
      let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
      assert_eq!(u.status, status, "GET {path}: {u:?}");
      assert_same_by(&u, &r, generated, &format!("GET {path}"));
    }
  }

  // Saved reports with the same IDs in both databases.
  let saved_funnel = seed_report(&stack, "funnel", "Funnel", data.website, admin_user.id, &funnel).await;
  let saved_goal = seed_report(
    &stack,
    "goal",
    "Goal",
    data.website,
    admin_user.id,
    &json!({ "type": "event", "value": "signup" }),
  )
  .await;
  let broken = seed_report(
    &stack,
    "funnel",
    "Broken",
    data.website,
    admin_user.id,
    &json!({ "window": 1 }),
  )
  .await;
  let own_goal = seed_report(
    &stack,
    "goal",
    "Mine",
    own,
    owner.id,
    &json!({ "type": "path", "value": "/" }),
  )
  .await;
  let site = |id: Uuid| format!("/api/websites/{id}");
  let w = data.website;

  for (tokens, path, status) in [
    (&admin, format!("{}/funnels/{saved_funnel}", site(w)), 200),
    (&admin, format!("{}/goals/{saved_funnel}", site(w)), 404),
    (&admin, format!("{}/funnels/{saved_funnel}", site(own)), 404),
    (&admin, format!("{}/funnels/{}", site(w), Uuid::new_v4()), 404),
    (&outsider, format!("{}/goals/{own_goal}", site(own)), 401),
    (&owner_tokens, format!("{}/goals/{own_goal}", site(own)), 200),
    (
      &admin,
      format!("{}/funnels/{saved_funnel}/stats?{}", site(w), range(&data, 30)),
      200,
    ),
    (
      &admin,
      format!("{}/goals/{saved_goal}/stats?{}", site(w), range(&data, 30)),
      200,
    ),
    (
      &admin,
      format!("{}/funnels/{broken}/stats?{}", site(w), range(&data, 30)),
      400,
    ),
    (
      &admin,
      format!("{}/funnels/{saved_funnel}/stats?startAt=5&endAt=1", site(w)),
      400,
    ),
    (
      &outsider,
      format!("{}/goals/{own_goal}/stats?{}", site(own), range(&data, 30)),
      401,
    ),
  ] {
    let response = compare(&stack, tokens, &path).await;
    assert_eq!(response.status, status, "GET {path}: {response:?}");
  }

  for (tokens, method, body, status) in [
    (
      &owner_tokens,
      Method::POST,
      Some(json!({ "parameters": { "type": "x", "value": "y" } })),
      400,
    ),
    (
      &outsider,
      Method::POST,
      Some(json!({ "name": "Renamed", "parameters": { "type": "x", "value": "y" } })),
      401,
    ),
    (
      &owner_tokens,
      Method::POST,
      Some(json!({ "name": "Renamed", "parameters": { "type": "event", "value": "signup" } })),
      200,
    ),
    (&outsider, Method::DELETE, None, 401),
    (&owner_tokens, Method::DELETE, None, 200),
    (&owner_tokens, Method::DELETE, None, 404),
  ] {
    let path = format!("{}/goals/{own_goal}", site(own));
    let (u, r) = stack.call_as(tokens, method.clone(), &path, body.as_ref()).await;
    assert_eq!(u.status, status, "{method} {path} {body:?}: {u:?}");
    assert_same_by(&u, &r, timestamps, &format!("{method} {path}"));
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn report_errors_and_permissions_match_upstream() {
  let stack = Stack::connect().await;
  let data = seed(&stack).await;
  let (_, admin) = stack.session("admin").await;
  let (_, outsider) = stack.session("user").await;
  let w = data.website;

  for (path, query) in [
    ("journeys", "steps=1"),
    ("journeys", "steps=8"),
    ("journeys", ""),
    ("breakdown", "fields=path"),
    ("breakdown", "fields=[\"pages\"]"),
    ("utm/metrics", "type=utm_id"),
    ("attribution", "model=linear&type=path&step=/"),
    ("attribution", "model=first-click&type=path"),
    ("performance/chart", "metric=fid"),
    ("performance/metrics", "type=path&limit=0"),
    ("performance/metrics", "type=os"),
    ("revenue/stats", ""),
    ("revenue/metrics", "currency=USD&type=city"),
    ("funnels/stats", "window=60&steps=[]"),
    (
      "funnels/stats",
      "steps=[{\"type\":\"path\",\"value\":\"/\"},{\"type\":\"path\",\"value\":\"/a\"}]",
    ),
    ("goals/stats", "type=path"),
  ] {
    let path = report(&data, path, query);
    let response = compare(&stack, &admin, &path).await;
    assert_eq!(response.status, 400, "{path}: {response:?}");
  }

  for path in [
    format!("/api/websites/{w}/retention?startAt=-1&endAt=5"),
    format!("/api/websites/{w}/retention?startAt=10&endAt=5"),
    format!("/api/websites/{w}/revenue/chart?currency=USD"),
  ] {
    let response = compare(&stack, &admin, &path).await;
    assert_eq!(response.status, 400, "{path}: {response:?}");
  }

  let response = compare(&stack, &outsider, &report(&data, "retention", "")).await;
  assert_eq!(response.status, 401, "{response:?}");

  // Share links see the reports their sections enable.
  for (sections, path, status) in [
    (json!({ "retention": true }), "retention", 200),
    (json!({ "overview": true }), "retention", 401),
    (json!({ "revenue": true }), "revenue/stats?currency=USD", 200),
    (json!({ "funnels": true }), "goals/stats?type=path&value=/", 401),
  ] {
    let token = share_token(json!({ "websiteId": w, "parameters": sections }));
    let (path, extra) = path.split_once('?').unwrap_or((path, ""));
    let path = report(&data, path, extra);
    let (u, r) = stack
      .call_both(
        Method::GET,
        &path,
        &[("x-umami-share-token", &token), ("x-umami-share-context", "1")],
        None,
      )
      .await;
    assert_eq!(u.status, status, "{path} {sections}: {u:?}");
    let (mut u, mut r) = (u, r);
    as_sets(&mut u.body);
    as_sets(&mut r.body);
    assert_same(&u, &r, &[], &path);
  }
}
