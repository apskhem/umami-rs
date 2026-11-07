//! Link and pixel parity: `/api/links` and `/api/pixels` (with their `{id}`,
//! `shares` and `charts` routes), the team lists, and the `/q/{slug}` and
//! `/p/{slug}` tracking routes.
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

/// The routes of links or of pixels.
#[derive(Clone, Copy)]
struct Kind {
  entity: Entity,
  path: &'static str,
  table: &'static str,
  share_type: i32,
}

const LINKS: Kind = Kind {
  entity: Entity::Link,
  path: "/api/links",
  table: "link",
  share_type: 2,
};

const PIXELS: Kind = Kind {
  entity: Entity::Pixel,
  path: "/api/pixels",
  table: "pixel",
  share_type: 3,
};

impl Kind {
  fn is_link(self) -> bool {
    self.table == "link"
  }

  /// A valid create body with a fixed ID, so both servers create the same row.
  fn new_body(self, name: &str, extra: Value) -> Value {
    let mut body = json!({ "id": Uuid::new_v4(), "name": name, "slug": format!("parity{}", tag()) });
    if self.is_link() {
      body["url"] = json!("https://example.com/target");
    }
    for (key, value) in extra.as_object().unwrap() {
      body[key] = value.clone();
    }
    body
  }

  fn item(self, id: impl std::fmt::Display) -> String {
    format!("{}/{id}", self.path)
  }

  /// The rows of `ids`, without their timestamps.
  fn rows(self, ids_column: &str) -> String {
    let table = self.table;
    let url = if self.is_link() { "url, " } else { "" };
    format!(
      "select {table}_id, name, {url}slug, user_id, team_id, deleted_at, \
       updated_at >= created_at as stamped from {table} where {ids_column} = any($1) order by name, {table}_id"
    )
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn creating_links_and_pixels() {
  for kind in [LINKS, PIXELS] {
    let stack = Stack::connect().await;
    let (owner, user) = stack.session("user").await;
    let (member, member_tokens) = stack.session("user").await;
    let (viewer, viewer_tokens) = stack.session("user").await;
    let (_, view_only) = stack.session("view-only").await;
    let team = seed_team(&stack, &[(&member, "team-member"), (&viewer, "team-view-only")]).await;

    let mut invalid = vec![
      json!(null),
      json!({ "name": "n" }),
      kind.new_body(&"n".repeat(101), json!({})),
      kind.new_body("n", json!({ "slug": "short" })),
      kind.new_body("n", json!({ "slug": "s".repeat(101) })),
      kind.new_body("n", json!({ "id": "x" })),
      kind.new_body("n", json!({ "name": 5 })),
    ];
    if kind.is_link() {
      invalid.push(kind.new_body("n", json!({ "url": "u".repeat(501) })));
      invalid.push(json!({ "name": "n", "slug": format!("parity{}", tag()) }));
    }
    for body in invalid {
      let (u, r) = stack.call_as(&user, Method::POST, kind.path, Some(&body)).await;
      assert_eq!(u.status, 400, "{body}: {u:?}");
      assert_same(&u, &r, &[], &format!("create {body}"));
    }

    for (tokens, body, status) in [
      (&user, kind.new_body("", json!({ "teamId": null })), 200),
      (&user, kind.new_body("Own", json!({ "url": "not a url" })), 200),
      (&view_only, kind.new_body("Denied", json!({})), 401),
      (&member_tokens, kind.new_body("Team", json!({ "teamId": team })), 200),
      (&viewer_tokens, kind.new_body("Team", json!({ "teamId": team })), 401),
      (&user, kind.new_body("Team", json!({ "teamId": team })), 401),
    ] {
      let (u, r) = stack.call_as(tokens, Method::POST, kind.path, Some(&body)).await;
      assert_eq!(u.status, status, "{body}: {u:?}");
      assert_same_by(&u, &r, timestamps, &format!("create {body}"));
      if status == 200 {
        // Pixels have no URL, whatever the body says.
        assert_eq!(u.body.get("url").is_some(), kind.is_link(), "{u:?}");
      }
    }

    let created = stack
      .assert_same_snapshot(&kind.rows("coalesce(user_id, team_id)"), &[owner.id, team], "created")
      .await;
    assert_eq!(created.as_array().unwrap().len(), 3, "{created}");

    // A taken slug or ID: upstream fails with a 500, umami-rs answers 400.
    let first = &created[0];
    let slug = first["slug"].as_str().unwrap();
    let (u, r) = stack
      .call_as(
        &user,
        Method::POST,
        kind.path,
        Some(&kind.new_body("Clash", json!({ "slug": slug }))),
      )
      .await;
    assert_eq!((u.status, r.status), (500, 400), "{u:?} {r:?}");
    assert_eq!(r.body["error"]["message"], "That slug is already taken.");

    let id = &first[format!("{}_id", kind.table)];
    let (u, r) = stack
      .call_as(
        &user,
        Method::POST,
        kind.path,
        Some(&kind.new_body("Clash", json!({ "id": id }))),
      )
      .await;
    assert_eq!((u.status, r.status), (500, 400), "{u:?} {r:?}");
    assert_eq!(
      r.body["error"]["message"],
      format!("That {} ID is already taken.", kind.table)
    );
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn viewing_updating_and_deleting_links_and_pixels() {
  for kind in [LINKS, PIXELS] {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (owner, owner_tokens) = stack.session("user").await;
    let (member, member_tokens) = stack.session("user").await;
    let (viewer, viewer_tokens) = stack.session("user").await;
    let (_, outsider) = stack.session("user").await;
    let team = seed_team(&stack, &[(&member, "team-member"), (&viewer, "team-view-only")]).await;
    let own = seed_entity(&stack, kind.entity, "Own", Some(owner.id), None).await;
    let team_owned = seed_entity(&stack, kind.entity, "Team", None, Some(team)).await;
    let other = seed_entity(&stack, kind.entity, "Other", Some(owner.id), None).await;
    let missing = Uuid::new_v4();

    for (tokens, id, status) in [
      (&owner_tokens, own, 200),
      (&outsider, own, 401),
      (&viewer_tokens, team_owned, 200),
      (&owner_tokens, team_owned, 401),
      (&admin, own, 200),
      (&admin, missing, 200),
      (&owner_tokens, missing, 401),
    ] {
      let (u, r) = stack.call_as(tokens, Method::GET, &kind.item(id), None).await;
      assert_eq!(u.status, status, "GET {id}: {u:?}");
      assert_same(&u, &r, &[], &format!("GET {id}"));
    }

    // Share links naming it (or naming it as a website) grant access.
    for claims in [
      json!({ "websiteId": own }),
      json!({ "linkId": own }),
      json!({ "pixelIds": [own] }),
    ] {
      let token = share_token(claims.clone());
      let (u, r) = stack
        .call_both(
          Method::GET,
          &kind.item(own),
          &[("x-umami-share-token", &token), ("x-umami-share-context", "1")],
          None,
        )
        .await;
      assert_same(&u, &r, &[], &format!("GET with {claims}"));
    }

    let taken = stack
      .upstream
      .snapshot(
        &format!("select slug from {} where {0}_id = any($1)", kind.table),
        &[other],
      )
      .await;
    let taken = taken[0]["slug"].as_str().unwrap().to_owned();

    let mut updates = vec![
      (&owner_tokens, own, json!({ "slug": "short" }), 400),
      (&owner_tokens, own, json!({ "name": null }), 400),
      (&outsider, own, json!({ "name": "Renamed" }), 401),
      (&viewer_tokens, team_owned, json!({ "name": "Renamed" }), 401),
      (&owner_tokens, own, json!({ "slug": taken }), 400),
      (&owner_tokens, own, json!({}), 200),
      (
        &owner_tokens,
        own,
        json!({ "name": "Renamed", "slug": format!("parity{}", tag()), "url": "https://example.org/" }),
        200,
      ),
      (&member_tokens, team_owned, json!({ "name": "Team renamed" }), 200),
      (&admin, own, json!({ "name": "By admin" }), 200),
    ];
    if kind.is_link() {
      updates.push((&owner_tokens, own, json!({ "url": "u".repeat(501) }), 400));
    } else {
      // Pixels ignore `url`, valid or not.
      updates.push((&owner_tokens, own, json!({ "url": null }), 200));
    }
    for (tokens, id, body, status) in updates {
      let (u, r) = stack.call_as(tokens, Method::POST, &kind.item(id), Some(&body)).await;
      assert_eq!(u.status, status, "POST {id} {body}: {u:?}");
      assert_same_by(&u, &r, timestamps, &format!("POST {id} {body}"));
    }
    stack
      .assert_same_snapshot(&kind.rows(&format!("{}_id", kind.table)), &[own, team_owned], "updated")
      .await;

    // Upstream fails with a 500 on an admin's change to a missing one.
    let (u, r) = stack
      .call_as(&admin, Method::POST, &kind.item(missing), Some(&json!({ "name": "x" })))
      .await;
    assert_eq!((u.status, r.status), (500, 404), "{u:?} {r:?}");
    let (u, r) = stack.call_as(&admin, Method::DELETE, &kind.item(missing), None).await;
    assert_eq!((u.status, r.status), (500, 404), "{u:?} {r:?}");

    // Deleting: upstream keeps the share links and collected events, umami-rs
    // deletes them.
    seed_share(&stack, own, kind.share_type, "{}").await;
    stack
      .execute(
        "insert into website_event (event_id, website_id, session_id, visit_id, url_path, event_type) \
         values ($1[2], $1[1], $1[2], $1[2], '/q', 3); \
         insert into session (session_id, website_id) values ($1[2], $1[1])",
        &[own, Uuid::new_v4()],
      )
      .await;
    for (tokens, id, status) in [
      (&outsider, own, 401),
      (&viewer_tokens, team_owned, 401),
      (&member_tokens, team_owned, 200),
      (&owner_tokens, own, 200),
      (&owner_tokens, own, 401),
    ] {
      let (u, r) = stack.call_as(tokens, Method::DELETE, &kind.item(id), None).await;
      assert_eq!(u.status, status, "DELETE {id}: {u:?}");
      assert_same(&u, &r, &[], &format!("DELETE {id}"));
    }
    stack
      .assert_same_snapshot(&kind.rows(&format!("{}_id", kind.table)), &[own, team_owned], "deleted")
      .await;

    let leftovers = "select (select count(*) from share where entity_id = any($1)) as shares, \
                     (select count(*) from website_event where website_id = any($1)) as events, \
                     (select count(*) from session where website_id = any($1)) as sessions";
    assert_eq!(
      stack.upstream.snapshot(leftovers, &[own]).await,
      json!([{ "shares": 1, "events": 1, "sessions": 1 }])
    );
    assert_eq!(
      stack.rs.snapshot(leftovers, &[own]).await,
      json!([{ "shares": 0, "events": 0, "sessions": 0 }])
    );
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn listing_links_and_pixels() {
  for kind in [LINKS, PIXELS] {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (owner, owner_tokens) = stack.session("user").await;
    let (member, member_tokens) = stack.session("user").await;
    let (_, outsider) = stack.session("user").await;
    let team = seed_team(&stack, &[(&member, "team-member")]).await;
    let search = tag();

    for name in ["Bravo", "alpha", "Charlie"] {
      seed_entity(&stack, kind.entity, &format!("{name} {search}"), Some(owner.id), None).await;
    }
    seed_entity(&stack, kind.entity, "Unrelated", Some(owner.id), None).await;
    seed_entity(&stack, kind.entity, &format!("Team {search}"), None, Some(team)).await;
    seed_entity(&stack, kind.entity, "Members own", Some(member.id), None).await;
    // Links are also found by their URL.
    stack
      .execute(
        &format!("update link set url = 'https://example.com/{search}' where name = 'Unrelated' and user_id = $1[1]"),
        &[owner.id],
      )
      .await;

    let team_path = format!("/api/teams/{team}/{}", kind.table.to_owned() + "s");
    for (tokens, path, status) in [
      (&owner_tokens, format!("{}?orderBy=name", kind.path), 200),
      (
        &owner_tokens,
        format!("{}?orderBy=name&sortDescending=true&pageSize=2&page=2", kind.path),
        200,
      ),
      (
        &owner_tokens,
        format!("{}?search={search}&orderBy=name", kind.path),
        200,
      ),
      (&owner_tokens, format!("{}?search=BRAVO&orderBy=slug", kind.path), 200),
      (&owner_tokens, format!("{}?orderBy=name&pageSize=0", kind.path), 400),
      (&member_tokens, format!("{team_path}?orderBy=name"), 200),
      (&member_tokens, format!("{team_path}?search=nothing"), 200),
      (&owner_tokens, format!("{team_path}?orderBy=name"), 401),
      (&admin, format!("{team_path}?orderBy=name"), 200),
      (&outsider, format!("{}?orderBy=name", kind.path), 200),
    ] {
      let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
      assert_eq!(u.status, status, "GET {path}: {u:?}");
      assert_same(&u, &r, &[], &format!("GET {path}"));
    }
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn sharing_links_and_pixels() {
  for kind in [LINKS, PIXELS] {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (owner, owner_tokens) = stack.session("user").await;
    let (viewer, viewer_tokens) = stack.session("user").await;
    let (_, outsider) = stack.session("user").await;
    let team = seed_team(&stack, &[(&viewer, "team-view-only")]).await;
    let own = seed_entity(&stack, kind.entity, "Own", Some(owner.id), None).await;
    let team_owned = seed_entity(&stack, kind.entity, "Team", None, Some(team)).await;
    let shares = |id: Uuid| format!("{}/shares", kind.item(id));

    for (tokens, path, body, status) in [
      (&owner_tokens, shares(own), json!({}), 400),
      (
        &owner_tokens,
        shares(own),
        json!({ "name": "S", "parameters": [] }),
        400,
      ),
      (&outsider, shares(own), json!({ "name": "S" }), 401),
      (&viewer_tokens, shares(team_owned), json!({ "name": "S" }), 401),
      (&owner_tokens, shares(own), json!({ "name": "Default" }), 200),
      (
        &owner_tokens,
        shares(own),
        json!({ "name": "With parameters", "parameters": { "overview": true } }),
        200,
      ),
      (&admin, shares(team_owned), json!({ "name": "Admin" }), 200),
    ] {
      let (u, r) = stack.call_as(tokens, Method::POST, &path, Some(&body)).await;
      assert_eq!(u.status, status, "POST {path} {body}: {u:?}");
      assert_same_by(
        &u,
        &r,
        |v| mask(v, &["id", "slug", "createdAt", "updatedAt"]),
        &format!("POST {path} {body}"),
      );
      if status == 200 {
        assert_eq!(u.body["shareType"], kind.share_type, "{u:?}");
      }
    }

    // Admins pass the check for a missing one: upstream shares it anyway.
    let (u, r) = stack
      .call_as(
        &admin,
        Method::POST,
        &shares(Uuid::new_v4()),
        Some(&json!({ "name": "S" })),
      )
      .await;
    assert_eq!((u.status, r.status), (200, 404), "{u:?} {r:?}");

    for (tokens, path, status) in [
      (&owner_tokens, shares(own), 200),
      (&owner_tokens, format!("{}?pageSize=1&page=2", shares(own)), 200),
      (&viewer_tokens, shares(team_owned), 200),
      (&outsider, shares(own), 401),
      (&owner_tokens, format!("{}?segment=x", shares(own)), 400),
    ] {
      let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
      assert_eq!(u.status, status, "GET {path}: {u:?}");
      assert_same_by(
        &u,
        &r,
        |v| mask(v, &["id", "slug", "createdAt", "updatedAt"]),
        &format!("GET {path}"),
      );
    }
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn charting_links_and_pixels() {
  let stack = Stack::connect().await;
  let (_, admin) = stack.session("admin").await;
  let (owner, owner_tokens) = stack.session("user").await;
  let (_, outsider) = stack.session("user").await;
  let link = seed_entity(&stack, Entity::Link, "Link", Some(owner.id), None).await;
  let quiet = seed_entity(&stack, Entity::Link, "Quiet", Some(owner.id), None).await;
  let pixel = seed_entity(&stack, Entity::Pixel, "Pixel", Some(owner.id), None).await;

  // Visits in the last few days (not near midnight UTC, so the default range
  // is the same for both servers), with a visitor coming back.
  let today = Utc::now().duration_trunc(Duration::days(1)).unwrap();
  let visits = [
    (link, 3, 1, 3),
    (link, 3, 1, 4),
    (link, 3, 2, 4),
    (link, 3, 30, 1),
    (link, 3, 54, 2),
    // Not a link event: left out.
    (link, 1, 30, 5),
    (pixel, 4, 7, 1),
    (pixel, 4, 20, 2),
    (pixel, 4, 200, 3),
  ];
  for (id, event_type, hours_ago, visitor) in visits {
    let at = today - Duration::hours(hours_ago) + Duration::minutes(5);
    stack
      .execute(
        &format!(
          "insert into website_event (event_id, website_id, session_id, visit_id, url_path, event_type, created_at) \
           values ($1[1], $1[2], $1[3], $1[3], '/q', {event_type}, '{}')",
          at.to_rfc3339()
        ),
        &[Uuid::new_v4(), id, Uuid::from_u128(visitor)],
      )
      .await;
  }

  let start = (today - Duration::days(3)).timestamp_millis();
  let end = (today + Duration::hours(12)).timestamp_millis() - 1;
  for (tokens, path, status) in [
    (&owner_tokens, format!("/api/links/charts?ids={link},{quiet}"), 200),
    (
      &owner_tokens,
      format!("/api/links/charts?ids={quiet},{link}&timezone=Asia/Bangkok"),
      200,
    ),
    (
      &owner_tokens,
      format!("/api/links/charts?ids={link}&startAt={start}&endAt={end}"),
      200,
    ),
    (
      &owner_tokens,
      format!("/api/links/charts?ids={link}&startAt={start}&timezone=asia/bangkok"),
      200,
    ),
    (&owner_tokens, format!("/api/pixels/charts?ids={pixel},{link}"), 200),
    (&admin, format!("/api/pixels/charts?ids={pixel},{link}"), 200),
    (&outsider, format!("/api/links/charts?ids={link}"), 200),
    (&owner_tokens, "/api/links/charts?ids=,".to_owned(), 400),
    (
      &owner_tokens,
      format!("/api/links/charts?ids={link}&timezone=Mars/Base"),
      400,
    ),
    (&owner_tokens, format!("/api/links/charts?ids={link}&endAt=1.5"), 400),
  ] {
    let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
    assert_eq!(u.status, status, "GET {path}: {u:?}");
    assert_same(&u, &r, &[], &format!("GET {path}"));
  }

  // Share tokens see what they name.
  let token = share_token(json!({ "linkId": link }));
  let (u, r) = stack
    .call_both(
      Method::GET,
      &format!("/api/links/charts?ids={link},{quiet}"),
      &[("x-umami-share-token", &token), ("x-umami-share-context", "1")],
      None,
    )
    .await;
  assert_same(&u, &r, &[], "charts with a share token");
  assert_eq!(u.body["data"].as_object().map(|d| d.len()), Some(1), "{u:?}");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn tracking_links_and_pixels() {
  let stack = Stack::connect().await;
  let http = reqwest::Client::builder()
    .redirect(reqwest::redirect::Policy::none())
    .build()
    .unwrap();
  let owner = stack.create_user("user").await;
  let link = seed_entity(&stack, Entity::Link, "Link", Some(owner.id), None).await;
  let pixel = seed_entity(&stack, Entity::Pixel, "Pixel", Some(owner.id), None).await;
  stack
    .execute(
      "update link set url = 'https://example.com/landing?ref=umami' where link_id = $1[1]",
      &[link],
    )
    .await;

  let get = |target: &str, path: &str, headers: &[(&str, &str)]| {
    let mut request = http.get(format!("{target}{path}"));
    for (name, value) in headers {
      request = request.header(*name, *value);
    }
    request.send()
  };
  let visitor = [
    (
      "user-agent",
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) \
       Chrome/120.0.0.0 Safari/537.36",
    ),
    ("referer", "https://www.google.com/search?q=umami"),
    ("x-forwarded-for", "203.0.113.7"),
    ("accept-language", "en-US"),
  ];
  let bot = [("user-agent", "Googlebot/2.1 (+http://www.google.com/bot.html)")];
  let ignored = [("user-agent", visitor[0].1), ("x-forwarded-for", "198.51.100.66")];

  for (path, headers) in [
    (
      format!("/q/{link}?utm_source=newsletter&utm_campaign=launch"),
      &visitor[..],
    ),
    (format!("/q/{link}"), &bot[..]),
    (format!("/q/{link}"), &ignored[..]),
    (format!("/p/{pixel}"), &visitor[..3]),
  ] {
    let u = get(&stack.upstream.url, &path, headers).await.unwrap();
    let r = get(&stack.rs.url, &path, headers).await.unwrap();
    assert_eq!(u.status(), r.status(), "{path}");
    for name in ["location", "content-type", "cache-control", "pragma", "expires"] {
      assert_eq!(u.headers().get(name), r.headers().get(name), "{path}: {name}");
    }
    assert_eq!(u.bytes().await.unwrap(), r.bytes().await.unwrap(), "{path}: body");
  }

  // The visits are recorded alike, except that upstream records its own
  // listening address as the hostname.
  let events = "select url_path, url_query, referrer_domain, referrer_path, referrer_query, \
                page_title, event_type, event_name, tag, utm_source, utm_campaign, session_id, visit_id \
                from website_event where website_id = any($1) order by event_type";
  let recorded = stack.assert_same_snapshot(events, &[link, pixel], "events").await;
  assert_eq!(recorded.as_array().unwrap().len(), 2, "{recorded}");
  let sessions = "select session_id, browser, os, device, screen, language, country, distinct_id \
                  from session where website_id = any($1) order by session_id";
  stack.assert_same_snapshot(sessions, &[link, pixel], "sessions").await;

  let hostnames = "select distinct hostname from website_event where website_id = any($1)";
  assert_eq!(
    stack.upstream.snapshot(hostnames, &[link, pixel]).await,
    json!([{ "hostname": "0.0.0.0" }])
  );
  assert_eq!(
    stack.rs.snapshot(hostnames, &[link, pixel]).await,
    json!([{ "hostname": "localhost" }])
  );

  for path in ["/q/missing-slug", "/p/missing-slug"] {
    let (u, r) = stack.call_both(Method::GET, path, &[], None).await;
    assert_eq!(u.status, 404, "{path}: {u:?}");
    assert_same(&u, &r, &[], path);
  }
}
