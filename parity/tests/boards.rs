//! Board parity: `/api/boards` (with `/{id}`, `clone` and `shares`), the team
//! lists, the user's own dashboard (`/api/dashboard`) and the website list
//! charts (`/api/websites/charts`).
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use chrono::{
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
  seed_share,
  seed_team,
  share_token,
  tag,
  timestamps,
};
use umami_parity::{
  Stack,
  assert_same,
  assert_same_by,
  mask,
};
use uuid::Uuid;

/// Hides the values each server generates itself.
fn generated(value: &mut Value) {
  mask(value, &["id", "createdAt", "updatedAt"]);
}

/// The boards of `owners` (users or teams), without IDs and timestamps.
const BOARDS: &str = "select type, name, description, parameters, user_id, team_id, \
                      updated_at >= created_at as stamped from board \
                      where coalesce(user_id, team_id) = any($1) \
                      order by name, description, type, updated_at is null";

/// Creates the same board in both databases.
async fn seed_board(
  stack: &Stack,
  kind: &str,
  name: &str,
  parameters: &Value,
  owner: (Option<Uuid>, Option<Uuid>),
) -> Uuid {
  let id = Uuid::new_v4();
  let owner_id = |id: Option<Uuid>| id.map_or_else(|| "null".to_owned(), |id| format!("'{id}'"));
  stack
    .execute(
      &format!(
        "insert into board (board_id, type, name, description, parameters, user_id, team_id, created_at) \
         values ($1[1], '{kind}', '{name}', 'Seeded', '{parameters}', {}, {}, '2026-01-02T03:04:05.678Z')",
        owner_id(owner.0),
        owner_id(owner.1)
      ),
      &[id],
    )
    .await;
  id
}

/// Creates the same saved report in both databases.
async fn seed_report(stack: &Stack, kind: &str, website: Uuid, user: Uuid) -> Uuid {
  let id = Uuid::new_v4();
  stack
    .execute(
      &format!(
        "insert into report (report_id, user_id, website_id, type, name, description, parameters) \
         values ($1[1], $1[2], $1[3], '{kind}', 'Report', '', '{{}}')"
      ),
      &[id, user, website],
    )
    .await;
  id
}

/// A board layout of one row with these components.
fn layout(components: &[Value]) -> Value {
  let columns: Vec<Value> = components.iter().map(|c| json!({ "component": c })).collect();
  json!({ "rows": [{ "columns": columns }] })
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn creating_boards() {
  let stack = Stack::connect().await;
  let (owner, user) = stack.session("user").await;
  let (member, member_tokens) = stack.session("user").await;
  let (viewer, viewer_tokens) = stack.session("user").await;
  let (_, view_only) = stack.session("view-only").await;
  let (other, _) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-member"), (&viewer, "team-view-only")]).await;
  let website = seed_entity(&stack, Entity::Website, "Site", Some(owner.id), None).await;
  let pixel = seed_entity(&stack, Entity::Pixel, "Pixel", Some(owner.id), None).await;
  let foreign = seed_entity(&stack, Entity::Website, "Foreign", Some(other.id), None).await;
  let funnel = seed_report(&stack, "funnel", website, owner.id).await;
  let goal = seed_report(&stack, "goal", website, owner.id).await;

  for body in [
    json!(null),
    json!({ "name": "B" }),
    json!({ "type": "dashboard", "name": "B" }),
    json!({ "type": "mixed", "name": "n".repeat(101) }),
    json!({ "type": "mixed", "name": "B", "description": "d".repeat(501) }),
    json!({ "type": "mixed", "name": "B", "teamId": "team" }),
    json!({ "type": "mixed", "name": "B", "parameters": null }),
    json!({ "type": "website", "name": "B", "parameters": { "websiteId": "site" } }),
  ] {
    let (u, r) = stack.call_as(&user, Method::POST, "/api/boards", Some(&body)).await;
    assert_eq!(u.status, 400, "{body}: {u:?}");
    assert_same(&u, &r, &[], &format!("create {body}"));
  }

  let funnel_on = |report: Uuid| json!({ "type": "Funnel", "websiteId": website, "props": { "reportId": report } });
  for (tokens, body, status) in [
    (&user, json!({ "type": "open", "name": "Open" }), 200),
    (
      &user,
      json!({ "type": "website", "name": "Site board", "description": "Mine", "userId": other.id,
              "parameters": { "websiteId": website, "extra": [1] } }),
      200,
    ),
    (
      &user,
      json!({ "type": "mixed", "name": "Layout", "parameters": layout(&[
          funnel_on(funnel),
          json!({ "type": "Goal", "entityType": "website", "entityId": website, "props": { "reportId": goal } }),
          json!({ "type": "Funnel", "props": { "reportId": "" } }),
          json!({ "entityType": "pixel", "entityId": pixel }),
      ]) }),
      200,
    ),
    // A report of the wrong type, or not on the component's website.
    (
      &user,
      json!({ "type": "mixed", "name": "Bad", "parameters": layout(&[funnel_on(goal)]) }),
      400,
    ),
    (
      &user,
      json!({ "type": "website", "name": "Bad", "parameters": { "websiteId": website, "rows": [{ "columns": [
          { "component": { "type": "Goal", "props": { "reportId": goal } } },
      ] }] } }),
      200,
    ),
    (
      &user,
      json!({ "type": "mixed", "name": "Bad", "parameters": layout(&[
          json!({ "type": "Goal", "websiteId": website, "props": { "reportId": 5 } }),
      ]) }),
      400,
    ),
    // Entities the user cannot view.
    (
      &user,
      json!({ "type": "website", "name": "Bad", "parameters": { "websiteId": foreign } }),
      400,
    ),
    (
      &user,
      json!({ "type": "mixed", "name": "Bad", "parameters": layout(&[json!({ "websiteId": foreign })]) }),
      400,
    ),
    (&view_only, json!({ "type": "mixed", "name": "Denied" }), 401),
    (
      &member_tokens,
      json!({ "type": "link", "name": "Team board", "teamId": team }),
      200,
    ),
    (
      &viewer_tokens,
      json!({ "type": "mixed", "name": "Team board", "teamId": team }),
      401,
    ),
  ] {
    let (u, r) = stack.call_as(tokens, Method::POST, "/api/boards", Some(&body)).await;
    assert_eq!(u.status, status, "{body}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("create {body}"));
  }

  stack
    .assert_same_snapshot(BOARDS, &[owner.id, team], "created boards")
    .await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn viewing_updating_and_deleting_boards() {
  let stack = Stack::connect().await;
  let (_, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (member, member_tokens) = stack.session("user").await;
  let (viewer, viewer_tokens) = stack.session("user").await;
  let (other, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-member"), (&viewer, "team-view-only")]).await;
  let website = seed_entity(&stack, Entity::Website, "Site", Some(owner.id), None).await;
  let foreign = seed_entity(&stack, Entity::Website, "Foreign", Some(other.id), None).await;
  let goal = seed_report(&stack, "goal", website, owner.id).await;
  let own = seed_board(
    &stack,
    "website",
    "Own",
    &json!({ "websiteId": website }),
    (Some(owner.id), None),
  )
  .await;
  let team_board = seed_board(&stack, "mixed", "Team", &json!({}), (None, Some(team))).await;
  let missing = Uuid::new_v4();
  let item = |id: Uuid| format!("/api/boards/{id}");

  for (tokens, id, status) in [
    (&owner_tokens, own, 200),
    (&outsider, own, 401),
    (&viewer_tokens, team_board, 200),
    (&admin, missing, 200),
    (&owner_tokens, missing, 401),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &item(id), None).await;
    assert_eq!(u.status, status, "GET {id}: {u:?}");
    assert_same(&u, &r, &[], &format!("GET {id}"));
  }
  let token = share_token(json!({ "boardId": own }));
  let (u, r) = stack
    .call_both(
      Method::GET,
      &item(own),
      &[("x-umami-share-token", &token), ("x-umami-share-context", "1")],
      None,
    )
    .await;
  assert_eq!(u.status, 200, "{u:?}");
  assert_same(&u, &r, &[], "GET with a share token");

  for (tokens, id, body, status) in [
    (&owner_tokens, own, json!({ "type": "Mixed" }), 400),
    (&owner_tokens, own, json!({ "name": "n".repeat(201) }), 400),
    (&outsider, own, json!({ "name": "Renamed" }), 401),
    (&viewer_tokens, team_board, json!({ "name": "Renamed" }), 401),
    (&owner_tokens, own, json!({}), 200),
    (
      &owner_tokens,
      own,
      json!({ "name": "Renamed", "description": "Changed" }),
      200,
    ),
    // The current parameters are checked against the new type, and the
    // current type against new parameters.
    (&owner_tokens, own, json!({ "type": "open" }), 200),
    (
      &owner_tokens,
      own,
      json!({ "parameters": { "websiteId": foreign } }),
      400,
    ),
    (
      &owner_tokens,
      own,
      json!({ "parameters": layout(&[json!({ "type": "Funnel", "websiteId": website, "props": { "reportId": goal } })]) }),
      400,
    ),
    (
      &owner_tokens,
      own,
      json!({ "type": "website", "parameters": { "websiteId": website, "rows": [] } }),
      200,
    ),
    (&member_tokens, team_board, json!({ "type": "dashboard" }), 200),
    (&admin, own, json!({ "name": "By admin" }), 200),
    (&admin, missing, json!({ "type": "mixed" }), 401),
  ] {
    let (u, r) = stack.call_as(tokens, Method::POST, &item(id), Some(&body)).await;
    assert_eq!(u.status, status, "POST {id} {body}: {u:?}");
    assert_same_by(&u, &r, timestamps, &format!("POST {id} {body}"));
  }
  stack
    .assert_same_snapshot(BOARDS, &[owner.id, team], "updated boards")
    .await;

  // Upstream fails with a 500 on an admin's plain change to, or deletion of, a
  // missing board.
  let (u, r) = stack
    .call_as(&admin, Method::POST, &item(missing), Some(&json!({ "name": "x" })))
    .await;
  assert_eq!((u.status, r.status), (500, 404), "{u:?} {r:?}");
  let (u, r) = stack.call_as(&admin, Method::DELETE, &item(missing), None).await;
  assert_eq!((u.status, r.status), (500, 404), "{u:?} {r:?}");

  // Deleting: upstream keeps the share links, umami-rs deletes them.
  seed_share(&stack, own, 4, "{}").await;
  for (tokens, id, status) in [
    (&outsider, own, 401),
    (&viewer_tokens, team_board, 401),
    (&member_tokens, team_board, 200),
    (&owner_tokens, own, 200),
    (&owner_tokens, own, 401),
  ] {
    let (u, r) = stack.call_as(tokens, Method::DELETE, &item(id), None).await;
    assert_eq!(u.status, status, "DELETE {id}: {u:?}");
    assert_same(&u, &r, &[], &format!("DELETE {id}"));
  }
  stack
    .assert_same_snapshot(BOARDS, &[owner.id, team], "deleted boards")
    .await;
  let shares = "select count(*) as shares from share where entity_id = any($1)";
  assert_eq!(stack.upstream.snapshot(shares, &[own]).await, json!([{ "shares": 1 }]));
  assert_eq!(stack.rs.snapshot(shares, &[own]).await, json!([{ "shares": 0 }]));
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn cloning_boards() {
  let stack = Stack::connect().await;
  let (_, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (member, member_tokens) = stack.session("user").await;
  let (viewer, viewer_tokens) = stack.session("user").await;
  let (other, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-member"), (&viewer, "team-view-only")]).await;
  let website = seed_entity(&stack, Entity::Website, "Site", Some(owner.id), None).await;
  let foreign = seed_entity(&stack, Entity::Website, "Foreign", Some(other.id), None).await;
  let funnel = seed_report(&stack, "funnel", website, owner.id).await;
  let goal = seed_report(&stack, "goal", website, owner.id).await;

  // Report references that have gone stale are dropped from the copy.
  let parameters = layout(&[
    json!({ "type": "Funnel", "websiteId": website, "props": { "reportId": funnel, "steps": 3 } }),
    json!({ "type": "Funnel", "websiteId": website, "props": { "reportId": goal, "steps": 3 } }),
    json!({ "type": "Goal", "websiteId": website, "props": { "reportId": funnel } }),
  ]);
  let own = seed_board(&stack, "open", "Own", &parameters, (Some(owner.id), None)).await;
  let team_board = seed_board(&stack, "pixel", "Team", &json!({}), (None, Some(team))).await;
  let legacy = seed_board(&stack, "legacy", "Legacy", &json!({}), (Some(owner.id), None)).await;
  let dashboard = seed_board(&stack, "dashboard", "Dash", &json!({}), (Some(owner.id), None)).await;
  let clone = |id: Uuid| format!("/api/boards/{id}/clone");

  for (tokens, id, body, status) in [
    (&owner_tokens, own, json!({ "name": "n".repeat(201) }), 400),
    (&outsider, own, json!({}), 401),
    (&owner_tokens, own, json!({}), 200),
    (
      &owner_tokens,
      own,
      json!({ "name": "Copy", "description": "Mine" }),
      200,
    ),
    (
      &owner_tokens,
      own,
      json!({ "parameters": layout(&[json!({ "websiteId": foreign })]) }),
      400,
    ),
    (&admin, own, json!({ "name": "Admin copy" }), 200),
    (&member_tokens, team_board, json!({}), 200),
    (&viewer_tokens, team_board, json!({}), 401),
    (&owner_tokens, legacy, json!({}), 400),
    (&owner_tokens, dashboard, json!({}), 400),
    (&admin, Uuid::new_v4(), json!({}), 401),
  ] {
    let (u, r) = stack.call_as(tokens, Method::POST, &clone(id), Some(&body)).await;
    assert_eq!(u.status, status, "clone {id} {body}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("clone {id} {body}"));
  }

  stack
    .assert_same_snapshot(BOARDS, &[owner.id, team], "cloned boards")
    .await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn listing_and_sharing_boards() {
  let stack = Stack::connect().await;
  let (_, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (member, member_tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-member")]).await;
  let search = tag();

  for (kind, name) in [("mixed", "Bravo"), ("website", "alpha"), ("link", "Charlie")] {
    seed_board(
      &stack,
      kind,
      &format!("{name} {search}"),
      &json!({}),
      (Some(owner.id), None),
    )
    .await;
  }
  // Dashboards are not listed.
  seed_board(
    &stack,
    "dashboard",
    &format!("Dash {search}"),
    &json!({}),
    (Some(owner.id), None),
  )
  .await;
  let team_board = seed_board(
    &stack,
    "mixed",
    &format!("Team {search}"),
    &json!({}),
    (None, Some(team)),
  )
  .await;

  let team_path = format!("/api/teams/{team}/boards");
  for (tokens, path, status) in [
    (&owner_tokens, "/api/boards?orderBy=name".to_owned(), 200),
    (
      &owner_tokens,
      "/api/boards?orderBy=type&sortDescending=true&pageSize=2&page=2".to_owned(),
      200,
    ),
    (&owner_tokens, format!("/api/boards?search={search}&orderBy=name"), 200),
    (&owner_tokens, "/api/boards?search=SEEDED&orderBy=name".to_owned(), 200),
    (&member_tokens, format!("{team_path}?orderBy=name"), 200),
    (&owner_tokens, format!("{team_path}?orderBy=name"), 401),
    (&admin, format!("{team_path}?orderBy=name"), 200),
    (&outsider, "/api/boards".to_owned(), 200),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "GET {path}: {u:?}");
    assert_same(&u, &r, &[], &format!("GET {path}"));
  }

  let shares = format!("/api/boards/{team_board}/shares");
  let generated = |v: &mut Value| mask(v, &["id", "slug", "createdAt", "updatedAt"]);
  for (tokens, body, status) in [
    (&member_tokens, json!({}), 400),
    (&outsider, json!({ "name": "S" }), 401),
    (&member_tokens, json!({ "name": "Shared" }), 200),
    (&admin, json!({ "name": "By admin", "parameters": { "x": 1 } }), 200),
  ] {
    let (u, r) = stack.call_as(tokens, Method::POST, &shares, Some(&body)).await;
    assert_eq!(u.status, status, "POST {shares} {body}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("POST {shares} {body}"));
  }
  for (tokens, status) in [(&member_tokens, 200), (&outsider, 401)] {
    let (u, r) = stack.call_as(tokens, Method::GET, &shares, None).await;
    assert_eq!(u.status, status, "GET {shares}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("GET {shares}"));
  }

  // Upstream shares a board that does not exist.
  let (u, r) = stack
    .call_as(
      &admin,
      Method::POST,
      &format!("/api/boards/{}/shares", Uuid::new_v4()),
      Some(&json!({ "name": "S" })),
    )
    .await;
  assert_eq!((u.status, r.status), (200, 404), "{u:?} {r:?}");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn saving_the_dashboard() {
  let stack = Stack::connect().await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (newcomer, newcomer_tokens) = stack.session("user").await;
  let website = seed_entity(&stack, Entity::Website, "Site", Some(owner.id), None).await;
  let goal = seed_report(&stack, "goal", website, owner.id).await;

  for (tokens, method, body, status) in [
    (&owner_tokens, Method::GET, None, 200),
    (&owner_tokens, Method::POST, Some(json!({ "parameters": [] })), 400),
    (
      &owner_tokens,
      Method::POST,
      Some(json!({ "name": "Mine", "description": "", "parameters": layout(&[
          json!({ "type": "Funnel", "websiteId": website, "props": { "reportId": goal } }),
      ]) })),
      400,
    ),
    (
      &owner_tokens,
      Method::POST,
      Some(json!({ "name": "Mine", "description": "", "parameters": layout(&[
          json!({ "type": "Goal", "websiteId": website, "props": { "reportId": goal } }),
      ]) })),
      200,
    ),
    (&owner_tokens, Method::GET, None, 200),
    // Later saves keep what they leave out, except the layout.
    (
      &owner_tokens,
      Method::POST,
      Some(json!({ "description": "Updated" })),
      200,
    ),
    (&owner_tokens, Method::GET, None, 200),
  ] {
    let (u, r) = stack
      .call_as(tokens, method.clone(), "/api/dashboard", body.as_ref())
      .await;
    assert_eq!(u.status, status, "{method} {body:?}: {u:?}");
    assert_same_by(&u, &r, timestamps, &format!("{method} {body:?}"));
  }
  stack.assert_same_snapshot(BOARDS, &[owner.id], "dashboard").await;

  // Upstream fails with a 500 when a first save has no name or description.
  let (u, r) = stack
    .call_as(&newcomer_tokens, Method::POST, "/api/dashboard", Some(&json!({})))
    .await;
  assert_eq!((u.status, r.status), (500, 200), "{u:?} {r:?}");
  assert_eq!(r.body["id"], json!(newcomer.id), "{r:?}");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn charting_websites() {
  let stack = Stack::connect().await;
  let (_, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (member, member_tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-member")]).await;
  let own = seed_entity(&stack, Entity::Website, "Own", Some(owner.id), None).await;
  let team_site = seed_entity(&stack, Entity::Website, "Team", None, Some(team)).await;

  let today = Utc::now().duration_trunc(Duration::days(1)).unwrap();
  for (id, event_type, hours_ago, visitor) in [
    (own, 1, 1, 1),
    (own, 1, 2, 1),
    (own, 2, 2, 2),
    (own, 3, 26, 3),
    (own, 5, 26, 4),
    (team_site, 1, 50, 5),
  ] {
    let at = today - Duration::hours(hours_ago) + Duration::minutes(5);
    stack
      .execute(
        &format!(
          "insert into website_event (event_id, website_id, session_id, visit_id, url_path, event_type, created_at) \
           values ($1[1], $1[2], $1[3], $1[3], '/', {event_type}, '{}')",
          at.to_rfc3339()
        ),
        &[Uuid::new_v4(), id, Uuid::from_u128(visitor)],
      )
      .await;
  }

  for (tokens, path, status) in [
    (
      &owner_tokens,
      format!("/api/websites/charts?ids={own},{team_site},{own}"),
      200,
    ),
    (
      &member_tokens,
      format!("/api/websites/charts?ids={own},{team_site}"),
      200,
    ),
    (
      &admin,
      format!("/api/websites/charts?ids={team_site},{own}&timezone=Asia/Bangkok"),
      200,
    ),
    (&outsider, format!("/api/websites/charts?ids={own}"), 200),
    (&owner_tokens, "/api/websites/charts?ids=x".to_owned(), 400),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "GET {path}: {u:?}");
    assert_same(&u, &r, &[], &format!("GET {path}"));
  }

  let token = share_token(json!({ "websiteIds": [team_site] }));
  let (u, r) = stack
    .call_both(
      Method::GET,
      &format!("/api/websites/charts?ids={own},{team_site}"),
      &[("x-umami-share-token", &token), ("x-umami-share-context", "1")],
      None,
    )
    .await;
  assert_same(&u, &r, &[], "charts with a share token");
  assert_eq!(u.body["data"].as_object().map(|d| d.len()), Some(1), "{u:?}");
}
