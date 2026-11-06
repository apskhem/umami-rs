//! Share link and segment parity: `/api/share`, `/api/share/{slug}`,
//! `/api/share/id/{shareId}`, `/api/websites/{id}/shares` and
//! `/api/websites/{id}/segments`.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use reqwest::Method;
use serde_json::{
  Value,
  json,
};
use umami_parity::{
  APP_SECRET,
  Response,
  Stack,
  TestUser,
  assert_same,
  assert_same_by,
  mask,
};
use umami_server::{
  crypto,
  jwt,
};
use uuid::Uuid;

/// Hides the values each server generates itself.
fn generated(value: &mut Value) {
  mask(value, &["id", "slug", "createdAt", "updatedAt"]);
}

/// Hides the timestamps each server sets when it writes.
fn timestamps(value: &mut Value) {
  mask(value, &["createdAt", "updatedAt"]);
}

/// A unique tag, for slugs and names.
fn tag() -> String {
  Uuid::new_v4().simple().to_string()[..10].to_owned()
}

fn secret() -> String {
  crypto::hash(&[APP_SECRET])
}

/// A share token for `claims`, as `/api/share/{slug}` issues them.
fn share_token(claims: Value) -> String {
  let mut claims = claims;
  claims["type"] = json!("share");
  claims["iat"] = json!(1);
  jwt::sign(&claims, &secret())
}

/// Creates the same team, with the same members, in both databases.
async fn seed_team(stack: &Stack, members: &[(&TestUser, &str)]) -> Uuid {
  let id = Uuid::new_v4();
  stack
    .execute("insert into team (team_id, name) values ($1[1], 'Parity team')", &[id])
    .await;
  for (user, role) in members {
    stack
      .execute(
        &format!("insert into team_user (team_user_id, team_id, user_id, role) values ($1[1], $1[2], $1[3], '{role}')"),
        &[Uuid::new_v4(), id, user.id],
      )
      .await;
  }
  id
}

/// Creates the same website, link, pixel or board (by `table`) in both databases.
async fn seed_entity(stack: &Stack, table: &str, user: Option<Uuid>, team: Option<Uuid>) -> Uuid {
  let id = Uuid::new_v4();
  let owner = |id: Option<Uuid>| id.map_or_else(|| "null".to_owned(), |id| format!("'{id}'"));
  let (columns, values) = match table {
    "website" => ("name, domain", "'Parity', 'example.com'"),
    "link" => ("name, url, slug", "'Parity', 'https://example.com', $1[1]::text"),
    "pixel" => ("name, slug", "'Parity', $1[1]::text"),
    _ => ("name, type, description, parameters", "'Parity', 'mixed', '', '{}'"),
  };
  stack
    .execute(
      &format!(
        "insert into {table} ({table}_id, user_id, team_id, {columns}) values ($1[1], {}, {}, {values})",
        owner(user),
        owner(team)
      ),
      &[id],
    )
    .await;
  id
}

/// Creates the same share link in both databases.
async fn seed_share(stack: &Stack, entity: Uuid, share_type: i32, parameters: &str) -> (Uuid, String) {
  let id = Uuid::new_v4();
  let slug = format!("parity{}", tag());
  stack
    .execute(
      &format!(
        "insert into share (share_id, entity_id, name, share_type, slug, parameters, created_at) \
         values ($1[1], $1[2], 'Shared', {share_type}, '{slug}', '{parameters}', '2026-01-02T03:04:05.678Z')"
      ),
      &[id, entity],
    )
    .await;
  (id, slug)
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn creating_shares() {
  let stack = Stack::connect().await;
  let (_, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (member, member_tokens) = stack.session("user").await;
  let (viewer, viewer_tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-member"), (&viewer, "team-view-only")]).await;
  let own = seed_entity(&stack, "website", Some(owner.id), None).await;
  let team_site = seed_entity(&stack, "website", None, Some(team)).await;
  let link = seed_entity(&stack, "link", Some(owner.id), None).await;
  let website_shares = |id: Uuid| format!("/api/websites/{id}/shares");

  for (tokens, path, body, status) in [
    (&owner_tokens, website_shares(own), json!({}), 400),
    (
      &owner_tokens,
      website_shares(own),
      json!({ "name": "n".repeat(201) }),
      400,
    ),
    (
      &owner_tokens,
      website_shares(own),
      json!({ "name": "S", "parameters": null }),
      400,
    ),
    (&outsider, website_shares(own), json!({ "name": "S" }), 401),
    (&viewer_tokens, website_shares(team_site), json!({ "name": "S" }), 401),
    (&owner_tokens, website_shares(own), json!({ "name": "Default" }), 200),
    (
      &member_tokens,
      website_shares(team_site),
      json!({ "name": "Team", "parameters": { "overview": true, "allowFilter": false } }),
      200,
    ),
    (&admin, website_shares(own), json!({ "name": "Admin" }), 200),
  ] {
    let (u, r) = stack.call_as(tokens, Method::POST, &path, Some(&body)).await;
    assert_eq!(u.status, status, "{path} {body}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("POST {path} {body}"));
    if status == 200 {
      assert_eq!(u.body["shareType"], 1);
      assert_eq!(r.body["slug"].as_str().map(str::len), Some(16), "{r:?}");
    }
  }

  let slug = format!("parity{}", tag());
  for (tokens, body, status) in [
    (
      &owner_tokens,
      json!({ "entityId": "x", "shareType": 1, "name": "S", "parameters": {} }),
      400,
    ),
    (
      &owner_tokens,
      json!({ "entityId": own, "shareType": 1.5, "name": "S", "parameters": {} }),
      400,
    ),
    (
      &owner_tokens,
      json!({ "entityId": own, "shareType": 1, "name": "S" }),
      400,
    ),
    (
      &owner_tokens,
      json!({ "entityId": own, "shareType": 1, "name": "S", "slug": "s".repeat(101), "parameters": {} }),
      400,
    ),
    (
      &outsider,
      json!({ "entityId": own, "shareType": 1, "name": "S", "parameters": {} }),
      401,
    ),
    (
      &viewer_tokens,
      json!({ "entityId": team_site, "shareType": 1, "name": "S", "parameters": {} }),
      401,
    ),
    (
      &owner_tokens,
      json!({ "entityId": own, "shareType": "1", "name": "Mine", "slug": slug, "parameters": { "events": true } }),
      200,
    ),
    (
      &member_tokens,
      json!({ "entityId": team_site, "shareType": 1, "name": "Team", "slug": "", "parameters": {} }),
      200,
    ),
    (
      &owner_tokens,
      json!({ "entityId": link, "shareType": 2, "name": "Link", "parameters": {} }),
      200,
    ),
  ] {
    let (u, r) = stack.call_as(tokens, Method::POST, "/api/share", Some(&body)).await;
    assert_eq!(u.status, status, "{body}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("POST /api/share {body}"));
  }

  stack
    .assert_same_snapshot(
      "select entity_id, name, share_type, parameters, length(slug) as slug_length, \
       created_at = updated_at as fresh from share where entity_id = any($1) order by name",
      &[own, team_site, link],
      "created shares",
    )
    .await;
  let chosen = format!("select count(*) as n from share where slug = '{slug}'");
  for target in stack.targets() {
    assert_eq!(
      target.snapshot(&chosen, &[]).await,
      json!([{ "n": 1 }]),
      "{}",
      target.name
    );
  }

  // A taken slug is a 500 upstream and a 400 here.
  let body = json!({ "entityId": own, "shareType": 1, "name": "Clash", "slug": slug, "parameters": {} });
  let (u, r) = stack
    .call_as(&owner_tokens, Method::POST, "/api/share", Some(&body))
    .await;
  assert_eq!((u.status, r.status), (500, 400), "{u:?} {r:?}");
  assert_eq!(r.body["error"]["message"], "That share ID is already taken.");

  // Upstream shares a website as a link, or a website that does not exist;
  // neither share can be opened, so umami-rs refuses them.
  let body = json!({ "entityId": own, "shareType": 2, "name": "Wrong", "parameters": {} });
  let (u, r) = stack
    .call_as(&owner_tokens, Method::POST, "/api/share", Some(&body))
    .await;
  assert_eq!((u.status, r.status), (200, 400), "{u:?} {r:?}");

  let ghost = Uuid::new_v4();
  let body = json!({ "entityId": ghost, "shareType": 1, "name": "Ghost", "parameters": {} });
  let (u, r) = stack.call_as(&admin, Method::POST, "/api/share", Some(&body)).await;
  assert_eq!((u.status, r.status), (200, 404), "{u:?} {r:?}");
  let (u, r) = stack
    .call_as(
      &admin,
      Method::POST,
      &website_shares(ghost),
      Some(&json!({ "name": "Ghost" })),
    )
    .await;
  assert_eq!((u.status, r.status), (200, 404), "{u:?} {r:?}");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn listing_and_managing_shares() {
  let stack = Stack::connect().await;
  let (_, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (member, member_tokens) = stack.session("user").await;
  let (viewer, viewer_tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-member"), (&viewer, "team-view-only")]).await;
  let own = seed_entity(&stack, "website", Some(owner.id), None).await;
  let team_site = seed_entity(&stack, "website", None, Some(team)).await;

  let mut own_shares = Vec::new();
  for minutes in 0..3 {
    let (id, slug) = seed_share(&stack, own, 1, r#"{"overview": true}"#).await;
    stack
      .execute(
        &format!(
          "update share set created_at = '2026-01-02T03:04:05.678Z'::timestamptz + interval '{minutes} minutes', \
           name = 'Share {minutes}' where share_id = $1[1]"
        ),
        &[id],
      )
      .await;
    own_shares.push((id, slug));
  }
  let (team_share, _) = seed_share(&stack, team_site, 1, "{}").await;

  let list = format!("/api/websites/{own}/shares");
  let team_list = format!("/api/websites/{team_site}/shares");
  for (tokens, path, status) in [
    (&owner_tokens, list.clone(), 200),
    (&owner_tokens, format!("{list}?pageSize=2&page=2"), 200),
    (&owner_tokens, format!("{list}?search=Share+1&orderBy=name"), 200),
    (&admin, list.clone(), 200),
    (&viewer_tokens, team_list.clone(), 200),
    (&outsider, list.clone(), 401),
    (&owner_tokens, format!("{list}?pageSize=0"), 400),
    (&owner_tokens, format!("{list}?segment=x"), 400),
    (&owner_tokens, format!("{list}?match=none"), 400),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "{path}: {u:?}");
    assert_same(&u, &r, &[], &format!("GET {path}"));
  }

  // Share links cannot list shares, even of the website they share.
  let token = share_token(json!({ "websiteId": own }));
  let headers = [("x-umami-share-token", token.as_str()), ("x-umami-share-context", "1")];
  let (u, r) = stack.call_both(Method::GET, &list, &headers, None).await;
  assert_eq!(u.status, 401, "{u:?}");
  assert_same(&u, &r, &[], "share token listing shares");

  let by_id = |id: Uuid| format!("/api/share/id/{id}");
  let (first, _) = own_shares[0].clone();
  let (second, _) = own_shares[1].clone();
  let (_, taken) = own_shares[2].clone();
  let fresh = format!("parity{}", tag());
  for (tokens, method, path, body, status) in [
    (&owner_tokens, Method::GET, by_id(first), None, 200),
    (&admin, Method::GET, by_id(first), None, 200),
    (&viewer_tokens, Method::GET, by_id(team_share), None, 200),
    (&outsider, Method::GET, by_id(first), None, 401),
    (
      &owner_tokens,
      Method::POST,
      by_id(first),
      Some(json!({ "name": "S", "parameters": {} })),
      400,
    ),
    (
      &owner_tokens,
      Method::POST,
      by_id(first),
      Some(json!({ "name": "S", "slug": "s", "parameters": [] })),
      400,
    ),
    (
      &outsider,
      Method::POST,
      by_id(first),
      Some(json!({ "name": "S", "slug": fresh, "parameters": {} })),
      401,
    ),
    (
      &viewer_tokens,
      Method::POST,
      by_id(team_share),
      Some(json!({ "name": "S", "slug": fresh, "parameters": {} })),
      401,
    ),
    (
      &owner_tokens,
      Method::POST,
      by_id(first),
      Some(json!({ "name": "Renamed", "slug": fresh, "parameters": { "events": true } })),
      200,
    ),
    (&viewer_tokens, Method::DELETE, by_id(team_share), None, 401),
    (&member_tokens, Method::DELETE, by_id(team_share), None, 200),
    (&owner_tokens, Method::DELETE, by_id(second), None, 200),
  ] {
    let (u, r) = stack.call_as(tokens, method.clone(), &path, body.as_ref()).await;
    assert_eq!(u.status, status, "{method} {path} {body:?}: {u:?}");
    assert_same_by(&u, &r, timestamps, &format!("{method} {path} {body:?}"));
  }

  stack
    .assert_same_snapshot(
      "select share_id, name, slug, parameters, updated_at > created_at as updated \
       from share where entity_id = any($1) order by name",
      &[own, team_site],
      "managed shares",
    )
    .await;

  // A taken slug is a 500 upstream and a 400 here.
  let body = json!({ "name": "Clash", "slug": taken, "parameters": {} });
  let (u, r) = stack
    .call_as(&owner_tokens, Method::POST, &by_id(first), Some(&body))
    .await;
  assert_eq!((u.status, r.status), (500, 400), "{u:?} {r:?}");

  // Missing and malformed shares fail upstream with a 500; here they are 404s.
  for path in [by_id(second), "/api/share/id/nope".to_owned()] {
    for method in [Method::GET, Method::DELETE] {
      let (u, r) = stack.call_as(&owner_tokens, method.clone(), &path, None).await;
      assert_eq!((u.status, r.status), (500, 404), "{method} {path}: {u:?} {r:?}");
    }
  }
  let body = json!({ "name": "S", "slug": "s", "parameters": {} });
  let (u, r) = stack
    .call_as(&owner_tokens, Method::POST, &by_id(second), Some(&body))
    .await;
  assert_eq!(u.status, 404, "{u:?}");
  assert_same(&u, &r, &[], "updating a missing share");
}

/// Asserts both servers open a share link the same way, including the claims of
/// the share token they issue. Returns upstream's response.
async fn open_share(stack: &Stack, slug: &str) -> Response {
  let path = format!("/api/share/{slug}");
  let (u, r) = stack.call_both(Method::GET, &path, &[], None).await;
  assert_same(&u, &r, &["token"], &format!("GET {path}"));

  if u.status == 200 {
    let claims = |response: &Response| {
      let mut claims = jwt::verify::<Value>(response.body["token"].as_str().unwrap(), &secret()).unwrap();
      assert!(claims["iat"].is_i64(), "{claims}");
      claims.as_object_mut().unwrap().remove("iat");
      claims
    };
    assert_eq!(claims(&u), claims(&r), "{path}: token claims differ");
  }
  u
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn opening_shares() {
  let stack = Stack::connect().await;
  let (owner, _) = stack.session("user").await;
  let (member, _) = stack.session("user").await;
  let (other, _) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-owner")]).await;
  let website = seed_entity(&stack, "website", Some(owner.id), None).await;
  let link = seed_entity(&stack, "link", Some(owner.id), None).await;
  let pixel = seed_entity(&stack, "pixel", Some(owner.id), None).await;
  let team_site = seed_entity(&stack, "website", None, Some(team)).await;
  let member_site = seed_entity(&stack, "website", Some(member.id), None).await;
  let foreign = seed_entity(&stack, "website", Some(other.id), None).await;

  let (_, website_slug) = seed_share(&stack, website, 1, r#"{"overview": true}"#).await;
  let (_, link_slug) = seed_share(&stack, link, 2, "{}").await;
  let (_, pixel_slug) = seed_share(&stack, pixel, 3, "{}").await;

  let u = open_share(&stack, &website_slug).await;
  assert_eq!(u.status, 200, "{u:?}");
  assert_eq!(u.body["websiteId"], json!(website));
  open_share(&stack, &link_slug).await;
  open_share(&stack, &pixel_slug).await;

  // Boards share the entities their owner may share.
  let rows = |ids: &[Uuid]| json!([{ "columns": ids.iter().map(|id| json!({ "component": { "websiteId": id } })).collect::<Vec<_>>() }]);
  for (user, team, ids) in [
    (Some(owner.id), None, vec![website, foreign]),
    (None, Some(team), vec![team_site, member_site, foreign]),
  ] {
    let board = seed_entity(&stack, "board", user, team).await;
    let parameters = json!({ "linkId": link, "rows": rows(&ids) });
    stack
      .execute(
        &format!("update board set parameters = '{parameters}' where board_id = $1[1]"),
        &[board],
      )
      .await;
    let (_, slug) = seed_share(&stack, board, 4, "{}").await;
    let u = open_share(&stack, &slug).await;
    assert_eq!(u.status, 200, "{u:?}");
    assert!(
      !u.body["websiteIds"].as_array().unwrap().contains(&json!(foreign)),
      "{u:?}"
    );
  }

  // Unknown slugs, unknown share types and shares of missing entities are 404s.
  let (_, unknown_type) = seed_share(&stack, website, 9, "{}").await;
  let (_, missing) = seed_share(&stack, Uuid::new_v4(), 1, "{}").await;
  let (_, mistyped) = seed_share(&stack, website, 2, "{}").await;
  for slug in ["nope".to_owned(), unknown_type, missing, mistyped] {
    let u = open_share(&stack, &slug).await;
    assert_eq!(u.status, 404, "{slug}: {u:?}");
  }

  // The issued token opens the shared website on both servers.
  for target in stack.targets() {
    let opened = stack
      .call(target, Method::GET, &format!("/api/share/{website_slug}"), &[], None)
      .await;
    let token = opened.body["token"].as_str().unwrap();
    let headers = [("x-umami-share-token", token), ("x-umami-share-context", "1")];
    let response = stack
      .call(target, Method::GET, &format!("/api/websites/{website}"), &headers, None)
      .await;
    assert_eq!(response.status, 200, "{}: {response:?}", target.name);
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn managing_segments() {
  let stack = Stack::connect().await;
  let (_, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (member, member_tokens) = stack.session("user").await;
  let (viewer, viewer_tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let team = seed_team(&stack, &[(&member, "team-member"), (&viewer, "team-view-only")]).await;
  let own = seed_entity(&stack, "website", Some(owner.id), None).await;
  let team_site = seed_entity(&stack, "website", None, Some(team)).await;
  let segments = |id: Uuid| format!("/api/websites/{id}/segments");

  let full = json!({
      "filters": [{ "name": "browser", "operator": "eq", "value": "chrome", "extra": 1 }],
      "sessionPropertyFilters": [
          { "propertyName": "plan", "dataType": 1, "operator": "c", "value": "pro", "x": true },
      ],
      "match": "any",
      "unknown": "dropped",
  });
  let cohort = json!({
      "filters": [{ "name": "country", "operator": "eq", "value": "DE" }],
      "dateRange": "30day",
      "action": { "type": "path", "value": "/" },
  });
  for (tokens, path, body, status) in [
    (
      &owner_tokens,
      segments(own),
      json!({ "name": "S", "parameters": {} }),
      400,
    ),
    (
      &owner_tokens,
      segments(own),
      json!({ "type": "group", "name": "S", "parameters": {} }),
      400,
    ),
    (
      &owner_tokens,
      segments(own),
      json!({ "type": "segment", "name": "S", "parameters": [] }),
      400,
    ),
    (
      &owner_tokens,
      segments(own),
      json!({ "type": "segment", "name": "S", "parameters": { "filters": [{ "name": "b", "operator": "like", "value": "x" }] } }),
      400,
    ),
    (
      &owner_tokens,
      segments(own),
      json!({ "type": "segment", "name": "S", "parameters": { "sessionPropertyFilters": [{ "propertyName": "", "dataType": 1, "operator": "eq", "value": "" }] } }),
      400,
    ),
    (
      &owner_tokens,
      segments(own),
      json!({ "type": "cohort", "name": "S", "parameters": full }),
      400,
    ),
    (
      &outsider,
      segments(own),
      json!({ "type": "segment", "name": "S", "parameters": {} }),
      401,
    ),
    (
      &viewer_tokens,
      segments(team_site),
      json!({ "type": "segment", "name": "S", "parameters": {} }),
      401,
    ),
    (
      &owner_tokens,
      segments(own),
      json!({ "type": "segment", "name": "Full", "parameters": full }),
      200,
    ),
    (
      &owner_tokens,
      segments(own),
      json!({ "type": "segment", "name": "Empty", "parameters": {} }),
      200,
    ),
    (
      &owner_tokens,
      segments(own),
      json!({ "type": "cohort", "name": "Cohort", "parameters": cohort }),
      200,
    ),
    (
      &member_tokens,
      segments(team_site),
      json!({ "type": "segment", "name": "Team", "parameters": {} }),
      200,
    ),
  ] {
    let (u, r) = stack.call_as(tokens, Method::POST, &path, Some(&body)).await;
    assert_eq!(u.status, status, "{path} {body}: {u:?}");
    assert_same_by(&u, &r, generated, &format!("POST {path} {body}"));
  }

  stack
    .assert_same_snapshot(
      "select website_id, type, name, parameters, created_at = updated_at as fresh \
       from segment where website_id = any($1) order by name",
      &[own, team_site],
      "created segments",
    )
    .await;

  // Distinct creation times, for umami-rs's order.
  stack
    .execute(
      "update segment set created_at = '2026-01-02T03:04:05Z'::timestamptz + \
       (case name when 'Full' then 0 when 'Empty' then 1 else 2 end) * interval '1 minute' \
       where website_id = $1[1]",
      &[own],
    )
    .await;

  // Upstream lists segments unordered; umami-rs oldest first.
  let unordered = |value: &mut Value| {
    generated(value);
    if let Some(data) = value["data"].as_array_mut() {
      data.sort_by_key(|segment| segment["name"].to_string());
    }
  };
  for (tokens, path, status) in [
    (&owner_tokens, segments(own), 400),
    (&owner_tokens, format!("{}?type=group", segments(own)), 400),
    (&owner_tokens, format!("{}?type=segment", segments(own)), 200),
    (
      &owner_tokens,
      format!("{}?type=segment&search=FU&page=2&pageSize=1", segments(own)),
      200,
    ),
    (&owner_tokens, format!("{}?type=cohort", segments(own)), 200),
    (&viewer_tokens, format!("{}?type=segment", segments(team_site)), 200),
    (&admin, format!("{}?type=segment", segments(own)), 200),
    (&outsider, format!("{}?type=segment", segments(own)), 401),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "{path}: {u:?}");
    assert_same_by(&u, &r, unordered, &format!("GET {path}"));
  }

  // umami-rs lists them oldest first.
  let (_, r) = stack
    .call_as(
      &owner_tokens,
      Method::GET,
      &format!("{}?type=segment", segments(own)),
      None,
    )
    .await;
  assert_eq!(r.body["data"][0]["name"], "Full", "{r:?}");

  // Share links see segments unless they disable filters.
  for (parameters, status) in [(json!({}), 200), (json!({ "allowFilter": false }), 401)] {
    let token = share_token(json!({ "websiteId": own, "parameters": parameters }));
    let headers = [("x-umami-share-token", token.as_str()), ("x-umami-share-context", "1")];
    let path = format!("{}?type=segment", segments(own));
    let (u, r) = stack.call_both(Method::GET, &path, &headers, None).await;
    assert_eq!(u.status, status, "{parameters}: {u:?}");
    assert_same_by(&u, &r, unordered, &format!("share token {parameters}"));
  }

  // The same segment in both databases, for reading, updating and deleting.
  let segment = Uuid::new_v4();
  let team_segment = Uuid::new_v4();
  stack
    .execute(
      "insert into segment (segment_id, website_id, type, name, parameters, created_at) values \
       ($1[1], $1[2], 'segment', 'Seeded', '{\"match\": \"all\"}', '2026-01-02T03:04:05.678Z'), \
       ($1[3], $1[4], 'segment', 'Team seeded', '{}', '2026-01-02T03:04:05.678Z')",
      &[segment, own, team_segment, team_site],
    )
    .await;
  let one = |website: Uuid, id: Uuid| format!("/api/websites/{website}/segments/{id}");
  let update = json!({ "type": "cohort", "name": "Updated", "parameters": cohort });
  for (tokens, method, path, body, status) in [
    (&owner_tokens, Method::GET, one(own, segment), None, 200),
    (&viewer_tokens, Method::GET, one(team_site, team_segment), None, 200),
    (&outsider, Method::GET, one(own, segment), None, 401),
    // A segment of another website is not found.
    (&owner_tokens, Method::GET, one(own, team_segment), None, 404),
    (&owner_tokens, Method::GET, one(own, Uuid::new_v4()), None, 404),
    (
      &owner_tokens,
      Method::POST,
      one(own, segment),
      Some(json!({ "type": "segment", "name": "S" })),
      400,
    ),
    (&outsider, Method::POST, one(own, segment), Some(update.clone()), 401),
    (
      &viewer_tokens,
      Method::POST,
      one(team_site, team_segment),
      Some(update.clone()),
      401,
    ),
    (
      &owner_tokens,
      Method::POST,
      one(own, team_segment),
      Some(update.clone()),
      404,
    ),
    (
      &owner_tokens,
      Method::POST,
      one(own, segment),
      Some(update.clone()),
      200,
    ),
    (&viewer_tokens, Method::DELETE, one(team_site, team_segment), None, 401),
    (&owner_tokens, Method::DELETE, one(own, team_segment), None, 404),
    (&member_tokens, Method::DELETE, one(team_site, team_segment), None, 200),
    (&member_tokens, Method::DELETE, one(team_site, team_segment), None, 404),
  ] {
    let (u, r) = stack.call_as(tokens, method.clone(), &path, body.as_ref()).await;
    assert_eq!(u.status, status, "{method} {path} {body:?}: {u:?}");
    assert_same_by(&u, &r, timestamps, &format!("{method} {path} {body:?}"));
  }

  stack
    .assert_same_snapshot(
      "select segment_id, type, name, parameters, updated_at > created_at as updated \
       from segment where segment_id = any($1)",
      &[segment, team_segment],
      "managed segments",
    )
    .await;

  // A malformed segment ID fails upstream with a 500; here it is a 404.
  let path = format!("{}/nope", segments(own));
  let (u, r) = stack.call_as(&owner_tokens, Method::GET, &path, None).await;
  assert_eq!((u.status, r.status), (500, 404), "{u:?} {r:?}");

  // Admins can save segments for any ID upstream; here the website must exist.
  let body = json!({ "type": "segment", "name": "Ghost", "parameters": {} });
  let (u, r) = stack
    .call_as(&admin, Method::POST, &segments(Uuid::new_v4()), Some(&body))
    .await;
  assert_eq!((u.status, r.status), (200, 404), "{u:?} {r:?}");
}
