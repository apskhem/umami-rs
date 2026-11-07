//! Annotation parity: `/api/websites/{id}/annotations` and `/{annotationId}`.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use reqwest::Method;
use serde_json::{
  Value,
  json,
};
use umami_parity::fixtures::{
  Entity,
  seed_entity,
  seed_team,
  share_token,
  timestamps,
};
use umami_parity::{
  Stack,
  assert_same,
  assert_same_by,
  mask,
};
use uuid::Uuid;

/// Creates the same annotation in both databases.
async fn seed_annotation(stack: &Stack, website: Uuid, user: Uuid, date: &str, note: &str) -> Uuid {
  let id = Uuid::new_v4();
  stack
    .execute(
      &format!(
        "insert into annotation (annotation_id, website_id, user_id, date, all_day, note, created_at) \
         values ($1[1], $1[2], $1[3], '{date}', true, '{note}', '2026-01-02T03:04:05.678Z')"
      ),
      &[id, website, user],
    )
    .await;
  id
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn annotations_match_upstream() {
  let stack = Stack::connect().await;
  let (_, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (member, member_tokens) = stack.session("user").await;
  let (viewer, viewer_tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-member"), (&viewer, "team-view-only")]).await;
  let own = seed_entity(&stack, Entity::Website, "Own", Some(owner.id), None).await;
  let team_site = seed_entity(&stack, Entity::Website, "Team", None, Some(team)).await;
  let list = |site: Uuid| format!("/api/websites/{site}/annotations");
  let generated = |v: &mut Value| mask(v, &["id", "createdAt", "updatedAt"]);

  for body in [
    json!({ "note": "Launch" }),
    json!({ "date": "soon", "note": "Launch" }),
    json!({ "date": [], "note": "Launch" }),
    json!({ "date": "2026-03-01", "note": "" }),
    json!({ "date": "2026-03-01", "note": "n".repeat(501) }),
    json!({ "date": "2026-03-01", "note": "Launch", "allDay": null }),
  ] {
    let (u, r) = stack
      .call_as(&owner_tokens, Method::POST, &list(own), Some(&body))
      .await;
    assert_eq!(u.status, 400, "{body}: {u:?}");
    assert_same(&u, &r, &[], &format!("create {body}"));
  }

  for (tokens, site, body, status) in [
    (
      &owner_tokens,
      own,
      json!({ "date": "2026-03-01", "note": "Launch" }),
      200,
    ),
    (
      &owner_tokens,
      own,
      json!({ "date": "2026-03-05T10:30:00.123456Z", "allDay": false, "note": "Outage", "extra": 1 }),
      200,
    ),
    (
      &owner_tokens,
      own,
      json!({ "date": 1772668800000_u64, "note": "Numeric" }),
      200,
    ),
    (&owner_tokens, own, json!({ "date": null, "note": "Epoch" }), 200),
    (&outsider, own, json!({ "date": "2026-03-01", "note": "Denied" }), 401),
    (
      &viewer_tokens,
      team_site,
      json!({ "date": "2026-03-01", "note": "Denied" }),
      401,
    ),
    (
      &member_tokens,
      team_site,
      json!({ "date": "2026-03-02", "note": "Team" }),
      200,
    ),
    (&admin, own, json!({ "date": "2026-03-03", "note": "Admin" }), 200),
  ] {
    let (u, r) = stack.call_as(tokens, Method::POST, &list(site), Some(&body)).await;
    assert_eq!(u.status, status, "{body}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("create {body}"));
  }

  // Upstream annotates a website that does not exist.
  let body = json!({ "date": "2026-03-01", "note": "Nowhere" });
  let (u, r) = stack
    .call_as(&admin, Method::POST, &list(Uuid::new_v4()), Some(&body))
    .await;
  assert_eq!((u.status, r.status), (200, 404), "{u:?} {r:?}");

  stack
    .assert_same_snapshot(
      "select website_id, user_id, date, all_day, note, created_at = updated_at as fresh \
       from annotation where website_id = any($1) order by date, note",
      &[own, team_site],
      "created annotations",
    )
    .await;

  let march = |day: u32| {
    chrono::NaiveDate::from_ymd_opt(2026, 3, day)
      .unwrap()
      .and_hms_opt(0, 0, 0)
      .unwrap()
      .and_utc()
      .timestamp_millis()
  };
  for (tokens, path, status) in [
    (&owner_tokens, list(own), 200),
    (&owner_tokens, format!("{}?pageSize=2&page=2", list(own)), 200),
    (&owner_tokens, format!("{}?search=OUT", list(own)), 200),
    (
      &owner_tokens,
      format!("{}?startAt={}&endAt={}", list(own), march(1), march(4)),
      200,
    ),
    (&owner_tokens, format!("{}?startAt={}", list(own), march(4)), 200),
    (&owner_tokens, format!("{}?startAt=x", list(own)), 400),
    (&owner_tokens, format!("{}?page=0", list(own)), 400),
    (&viewer_tokens, list(team_site), 200),
    (&outsider, list(own), 401),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "GET {path}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("GET {path}"));
  }

  // One annotation, with the same ID in both databases.
  let saved = seed_annotation(&stack, own, owner.id, "2026-04-01T00:00:00Z", "Saved").await;
  let item = |site: Uuid, id: Uuid| format!("/api/websites/{site}/annotations/{id}");
  for (tokens, path, status) in [
    (&owner_tokens, item(own, saved), 200),
    (&owner_tokens, item(team_site, saved), 401),
    (&admin, item(team_site, saved), 404),
    (&admin, item(own, Uuid::new_v4()), 404),
    (&outsider, item(own, saved), 401),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "GET {path}: {u:?}");
    assert_same(&u, &r, &[], &format!("GET {path}"));
  }

  // Share links read annotations unless they disable filters.
  for (parameters, status) in [(json!({}), 200), (json!({ "allowFilter": false }), 401)] {
    let token = share_token(json!({ "websiteId": own, "parameters": parameters }));
    let headers = [("x-umami-share-token", token.as_str()), ("x-umami-share-context", "1")];
    let (u, r) = stack.call_both(Method::GET, &item(own, saved), &headers, None).await;
    assert_eq!(u.status, status, "{parameters}: {u:?}");
    assert_same(&u, &r, &[], &format!("share link {parameters}"));
  }

  for (tokens, method, body, status) in [
    (&owner_tokens, Method::POST, Some(json!({ "note": "Moved" })), 400),
    (
      &outsider,
      Method::POST,
      Some(json!({ "date": "2026-04-02", "note": "Moved" })),
      401,
    ),
    // Without `allDay`, an update makes the annotation all-day again.
    (
      &owner_tokens,
      Method::POST,
      Some(json!({ "date": "2026-04-02T12:00:00Z", "allDay": false, "note": "Moved" })),
      200,
    ),
    (
      &owner_tokens,
      Method::POST,
      Some(json!({ "date": "2026-04-03", "note": "Moved again" })),
      200,
    ),
    (&outsider, Method::DELETE, None, 401),
    (&owner_tokens, Method::DELETE, None, 200),
    (&owner_tokens, Method::DELETE, None, 404),
    (
      &owner_tokens,
      Method::POST,
      Some(json!({ "date": "2026-04-03", "note": "Gone" })),
      404,
    ),
  ] {
    let path = item(own, saved);
    let (u, r) = stack.call_as(tokens, method.clone(), &path, body.as_ref()).await;
    assert_eq!(u.status, status, "{method} {body:?}: {u:?}");
    assert_same_by(&u, &r, timestamps, &format!("{method} {path} {body:?}"));
  }

  // Upstream fails with a 500 on a malformed ID.
  let path = format!("{}/annotation", list(own));
  let (u, r) = stack.call_as(&owner_tokens, Method::GET, &path, None).await;
  assert_eq!((u.status, r.status), (500, 404), "{u:?} {r:?}");
}
