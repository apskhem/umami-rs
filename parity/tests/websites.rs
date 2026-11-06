//! Website management parity: `/api/websites`, `/api/websites/{id}` (and its
//! `reset` and `transfer`), the website lists of users and teams, and
//! `/api/admin/websites`.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use reqwest::Method;
use serde_json::{Value, json};
use umami_parity::{APP_SECRET, Stack, TestUser, assert_same, assert_same_by, mask};
use umami_server::{crypto, jwt};
use uuid::Uuid;

/// Hides the timestamps each server sets when it writes a website.
fn timestamps(value: &mut Value) {
    mask(value, &["createdAt", "updatedAt", "resetAt"]);
}

/// A unique tag, for names and search terms.
fn tag() -> String {
    Uuid::new_v4().simple().to_string()[..10].to_owned()
}

/// Creates the same team, with the same members, in both databases.
async fn seed_team(stack: &Stack, members: &[(&TestUser, &str)]) -> Uuid {
    let id = Uuid::new_v4();
    stack
        .execute(
            "insert into team (team_id, name, created_at) values ($1[1], 'Parity team', '2026-01-02T03:04:05.678Z')",
            &[id],
        )
        .await;
    for (user, role) in members {
        stack
            .execute(
                &format!(
                    "insert into team_user (team_user_id, team_id, user_id, role) \
                     values ($1[1], $1[2], $1[3], '{role}')"
                ),
                &[Uuid::new_v4(), id, user.id],
            )
            .await;
    }
    id
}

/// Creates the same website in both databases.
async fn seed_website(stack: &Stack, name: &str, user: Option<Uuid>, team: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    let owner = match (user, team) {
        (Some(_), _) => "$1[2], null",
        (None, Some(_)) => "null, $1[2]",
        (None, None) => "null, null",
    };
    stack
        .execute(
            &format!(
                "insert into website (website_id, name, domain, user_id, team_id, created_at) \
                 values ($1[1], '{name}', 'example.com', {owner}, '2026-01-02T03:04:05.678Z')"
            ),
            &[id, user.or(team).unwrap_or(id)],
        )
        .await;
    id
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn creating_websites() {
    let stack = Stack::connect().await;
    let (owner, user) = stack.session("user").await;
    let (member, member_tokens) = stack.session("user").await;
    let (viewer, viewer_tokens) = stack.session("user").await;
    let (_, view_only) = stack.session("view-only").await;
    let team = seed_team(
        &stack,
        &[(&member, "team-member"), (&viewer, "team-view-only")],
    )
    .await;

    for body in [
        json!(null),
        json!({ "name": "   ", "domain": "example.com" }),
        json!({ "name": "n".repeat(101), "domain": "example.com" }),
        json!({ "name": "Site" }),
        json!({ "name": "Site", "domain": "https://example.com" }),
        json!({ "name": "Site", "domain": "Example.com" }),
        json!({ "name": "Site", "domain": "example.com", "shareId": "s".repeat(51) }),
        json!({ "name": "Site", "domain": "example.com", "teamId": "team" }),
        json!({ "name": "Site", "domain": "example.com", "id": 5 }),
    ] {
        let (u, r) = stack
            .call_as(&user, Method::POST, "/api/websites", Some(&body))
            .await;
        assert_eq!(u.status, 400, "{body}: {u:?}");
        assert_same(&u, &r, &[], &format!("create {body}"));
    }

    // Fixed IDs, so both servers create the same website.
    let create = |mut body: Value| {
        body["id"] = json!(Uuid::new_v4());
        body
    };

    // Names and domains are trimmed; a share ID creates a share link.
    let share = format!("parity{}", tag());
    for (tokens, body, status) in [
        (
            &user,
            create(json!({ "name": "  Own site ", "domain": " own.example " })),
            200,
        ),
        (
            &user,
            create(
                json!({ "name": "Shared", "domain": "localhost:3000", "shareId": share, "teamId": null }),
            ),
            200,
        ),
        (
            &view_only,
            create(json!({ "name": "Site", "domain": "example.com" })),
            401,
        ),
        (
            &member_tokens,
            create(json!({ "name": "Team site", "domain": "team.example", "teamId": team })),
            200,
        ),
        (
            &viewer_tokens,
            create(json!({ "name": "Team site", "domain": "team.example", "teamId": team })),
            401,
        ),
        (
            &user,
            create(json!({ "name": "Team site", "domain": "team.example", "teamId": team })),
            401,
        ),
    ] {
        let (u, r) = stack
            .call_as(tokens, Method::POST, "/api/websites", Some(&body))
            .await;
        assert_eq!(u.status, status, "{body}: {u:?}");
        assert_same_by(&u, &r, timestamps, &format!("create {body}"));
    }

    stack
        .assert_same_snapshot(
            "select w.website_id, w.name, w.domain, w.user_id, w.team_id, w.created_by, \
             w.recorder_enabled, w.replay_config, w.updated_at = w.created_at as fresh, \
             s.slug, s.name as share_name, s.share_type, s.parameters \
             from website w left join share s on s.entity_id = w.website_id \
             where w.created_by = any($1) order by w.name",
            &[owner.id, member.id],
            "created websites",
        )
        .await;

    // A taken share ID: upstream creates the website, then fails with a 500 on
    // the share link; umami-rs answers 400 and creates neither.
    let body = create(json!({ "name": "Clash", "domain": "example.com", "shareId": share }));
    let (u, r) = stack
        .call_as(&user, Method::POST, "/api/websites", Some(&body))
        .await;
    assert_eq!((u.status, r.status), (500, 400), "{u:?} {r:?}");
    assert_eq!(
        r.body["error"]["message"],
        "That share ID is already taken."
    );

    let clash = "select name from website where website_id = any($1)";
    let id = [body["id"].as_str().unwrap().parse().unwrap()];
    assert_eq!(
        stack.upstream.snapshot(clash, &id).await,
        json!([{ "name": "Clash" }])
    );
    assert_eq!(stack.rs.snapshot(clash, &id).await, json!([]));

    // A taken website ID is a 500 upstream and a 400 here.
    let (u, r) = stack
        .call_as(&user, Method::POST, "/api/websites", Some(&body))
        .await;
    assert_eq!((u.status, r.status), (500, 400), "{u:?} {r:?}");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn viewing_and_updating_websites() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (owner, owner_tokens) = stack.session("user").await;
    let (member, member_tokens) = stack.session("user").await;
    let (viewer, viewer_tokens) = stack.session("user").await;
    let (_, outsider) = stack.session("user").await;
    let team = seed_team(
        &stack,
        &[(&member, "team-member"), (&viewer, "team-view-only")],
    )
    .await;
    let own = seed_website(&stack, "Own", Some(owner.id), None).await;
    let team_site = seed_website(&stack, "Team", None, Some(team)).await;
    let own_path = format!("/api/websites/{own}");
    let team_path = format!("/api/websites/{team_site}");

    for (tokens, path, status) in [
        (&owner_tokens, own_path.clone(), 200),
        (&admin, own_path.clone(), 200),
        (&outsider, own_path.clone(), 401),
        (&viewer_tokens, team_path.clone(), 200),
        (&outsider, team_path.clone(), 401),
        // Unknown and malformed IDs are `null` for admins.
        (&admin, format!("/api/websites/{}", Uuid::new_v4()), 200),
        (&admin, "/api/websites/not-a-uuid".to_owned(), 200),
        (&outsider, format!("/api/websites/{}", Uuid::new_v4()), 401),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same(&u, &r, &[], &format!("GET {path}"));
    }

    let share = format!("parity{}", tag());
    let taken = format!("parity{}", tag());
    stack
        .execute(
            &format!(
                "insert into share (share_id, entity_id, name, share_type, slug, parameters) \
                 values ($1[1], $1[2], 'Taken', 1, '{taken}', '{{}}')"
            ),
            &[Uuid::new_v4(), Uuid::new_v4()],
        )
        .await;

    for (tokens, path, body, status) in [
        (&owner_tokens, &own_path, json!({ "name": "" }), 400),
        (&owner_tokens, &own_path, json!({ "domain": "nope" }), 400),
        (
            &owner_tokens,
            &own_path,
            json!({ "replayConfig": { "sampleRate": 2 } }),
            400,
        ),
        (
            &owner_tokens,
            &own_path,
            json!({ "replayConfig": "on" }),
            400,
        ),
        (&owner_tokens, &own_path, json!({ "shareId": 5 }), 400),
        (&outsider, &own_path, json!({ "name": "Mine" }), 401),
        (&viewer_tokens, &team_path, json!({ "name": "Mine" }), 401),
        (
            &member_tokens,
            &team_path,
            json!({ "name": "  Team renamed  " }),
            200,
        ),
        (
            &owner_tokens,
            &own_path,
            json!({ "name": "Renamed", "domain": "renamed.example" }),
            200,
        ),
        // Recorder settings merge with the stored ones; `null` clears them.
        (
            &owner_tokens,
            &own_path,
            json!({ "replayConfig": { "replayEnabled": true, "sampleRate": 0.5, "x": 1 } }),
            200,
        ),
        (
            &owner_tokens,
            &own_path,
            json!({ "replayConfig": { "maxDuration": 60, "replayEnabled": false } }),
            200,
        ),
        (
            &owner_tokens,
            &own_path,
            json!({ "replayConfig": { "heatmapEnabled": true, "maskLevel": "strict" } }),
            200,
        ),
        (
            &owner_tokens,
            &own_path,
            json!({ "replayConfig": null }),
            200,
        ),
        // Share links: create, keep, refuse a taken one, delete.
        (&owner_tokens, &own_path, json!({ "shareId": share }), 200),
        (&owner_tokens, &own_path, json!({ "shareId": "" }), 200),
        (&owner_tokens, &own_path, json!({ "name": "Kept" }), 200),
        (&owner_tokens, &own_path, json!({ "shareId": taken }), 400),
        (&owner_tokens, &own_path, json!({ "shareId": null }), 200),
        (
            &admin,
            &format!("/api/websites/{}", Uuid::new_v4()),
            json!({ "name": "Ghost" }),
            400,
        ),
    ] {
        let (u, r) = stack.call_as(tokens, Method::POST, path, Some(&body)).await;
        assert_eq!(u.status, status, "{path} {body}: {u:?}");
        assert_same_by(&u, &r, timestamps, &format!("update {path} {body}"));
    }

    stack
        .assert_same_snapshot(
            "select website_id, name, domain, recorder_enabled, replay_config, \
             (select count(*) from share s where s.entity_id = w.website_id) as shares \
             from website w where website_id = any($1) order by name",
            &[own, team_site],
            "updated websites",
        )
        .await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn listing_websites() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (owner, owner_tokens) = stack.session("user").await;
    let (manager, manager_tokens) = stack.session("user").await;
    let (member, member_tokens) = stack.session("user").await;
    let team = seed_team(
        &stack,
        &[(&manager, "team-manager"), (&member, "team-member")],
    )
    .await;
    let tag = tag();

    // Distinct creation times: upstream orders ties arbitrarily (umami-rs by ID).
    for (minutes, name) in ["Alpha", "Bravo", "Charlie"].iter().enumerate() {
        let id = seed_website(&stack, &format!("{name} {tag}"), Some(owner.id), None).await;
        stack
            .execute(
                &format!(
                    "update website set created_at = created_at + interval '{minutes} minutes' \
                     where website_id = $1[1]"
                ),
                &[id],
            )
            .await;
    }
    let team_site = seed_website(&stack, &format!("Delta {tag}"), None, Some(team)).await;
    stack
        .execute(
            "update website set created_by = $1[2] where website_id = $1[1]; \
             insert into share (share_id, entity_id, name, share_type, slug, parameters) \
             values ($1[3], $1[1], 'Delta', 1, 'parity-' || $1[3], '{}')",
            &[team_site, member.id, Uuid::new_v4()],
        )
        .await;

    let user_websites = format!("/api/users/{}/websites", owner.id);
    let team_websites = format!("/api/teams/{team}/websites");
    for (tokens, path) in [
        (&owner_tokens, "/api/websites".to_owned()),
        (
            &owner_tokens,
            "/api/websites?orderBy=createdAt&sortDescending=true&pageSize=2".to_owned(),
        ),
        (
            &owner_tokens,
            "/api/websites?orderBy=password&page=2&pageSize=2".to_owned(),
        ),
        (&owner_tokens, "/api/websites?search=BRAVO".to_owned()),
        (&owner_tokens, "/api/websites?includeTeams=".to_owned()),
        (&owner_tokens, "/api/me/websites?search=BRAVO".to_owned()),
        // Team access counts for owners and managers, not members.
        (&manager_tokens, "/api/websites?includeTeams=1".to_owned()),
        (
            &member_tokens,
            "/api/me/websites?includeTeams=true".to_owned(),
        ),
        (&admin, format!("{user_websites}?search=ali")),
        (&member_tokens, format!("{team_websites}?orderBy=name")),
        (
            &admin,
            format!("/api/admin/websites?search={tag}&orderBy=name"),
        ),
        (
            &admin,
            format!("/api/admin/websites?search={tag}&pageSize=1"),
        ),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
        assert_eq!(u.status, 200, "{path}: {u:?}");
        assert_same(&u, &r, &[], &format!("GET {path}"));
    }

    let (u, _) = stack
        .call_as(&owner_tokens, Method::GET, "/api/websites", None)
        .await;
    assert_eq!(u.body["count"], 3);
    assert_eq!(u.body["orderBy"], "name");
    assert_eq!(u.body["data"][0]["user"]["username"], json!(owner.username));

    let (u, _) = stack
        .call_as(
            &admin,
            Method::GET,
            &format!("/api/admin/websites?search=Delta {tag}"),
            None,
        )
        .await;
    assert_eq!(u.body["data"][0]["team"]["id"], json!(team));
    assert!(u.body["data"][0]["shareId"].is_string(), "{u:?}");

    for (tokens, path, status) in [
        (&member_tokens, user_websites.clone(), 401),
        (&owner_tokens, team_websites.clone(), 401),
        (&owner_tokens, "/api/admin/websites".to_owned(), 401),
        (&owner_tokens, "/api/websites?pageSize=0".to_owned(), 400),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same(&u, &r, &[], &format!("GET {path}"));
    }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn transferring_websites() {
    let stack = Stack::connect().await;
    let (owner, owner_tokens) = stack.session("user").await;
    let (manager, manager_tokens) = stack.session("user").await;
    let (_, outsider) = stack.session("user").await;
    let team = seed_team(
        &stack,
        &[(&owner, "team-owner"), (&manager, "team-manager")],
    )
    .await;
    let own = seed_website(&stack, "Own", Some(owner.id), None).await;
    let other = seed_website(&stack, "Other", Some(manager.id), None).await;
    let transfer = |id: Uuid| format!("/api/websites/{id}/transfer");

    for (tokens, path, body, status) in [
        (&owner_tokens, transfer(own), json!({}), 400),
        (
            &owner_tokens,
            transfer(own),
            json!({ "teamId": "team" }),
            400,
        ),
        (&outsider, transfer(own), json!({ "teamId": team }), 401),
        // Owners may move their websites into a team they own or manage...
        (&owner_tokens, transfer(own), json!({ "teamId": team }), 200),
        (
            &manager_tokens,
            transfer(other),
            json!({ "teamId": team }),
            200,
        ),
        // ...but only team owners may take a team website for themselves.
        (
            &manager_tokens,
            transfer(other),
            json!({ "userId": manager.id }),
            401,
        ),
        (
            &owner_tokens,
            transfer(own),
            json!({ "userId": manager.id }),
            401,
        ),
        (
            &owner_tokens,
            transfer(own),
            json!({ "userId": owner.id }),
            200,
        ),
    ] {
        let (u, r) = stack
            .call_as(tokens, Method::POST, &path, Some(&body))
            .await;
        assert_eq!(u.status, status, "{path} {body}: {u:?}");
        assert_same_by(&u, &r, timestamps, &format!("transfer {path} {body}"));
    }

    stack
        .assert_same_snapshot(
            "select website_id, user_id, team_id from website where website_id = any($1) order by name",
            &[own, other],
            "transferred websites",
        )
        .await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn resetting_and_deleting_websites() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (owner, owner_tokens) = stack.session("user").await;
    let (_, outsider) = stack.session("user").await;
    let website = seed_website(&stack, "Data", Some(owner.id), None).await;
    let [session, event, segment] = std::array::from_fn(|_| Uuid::new_v4());
    let ids = [website, session, event, segment];

    let collect = "insert into session (session_id, website_id) values ($1[2], $1[1]); \
         insert into website_event (event_id, website_id, session_id, visit_id, url_path, event_type) \
         values ($1[3], $1[1], $1[2], $1[2], '/', 2); \
         insert into revenue (revenue_id, website_id, session_id, event_id, event_name, currency, revenue, created_at) \
         values (gen_random_uuid(), $1[1], $1[2], $1[3], 'buy', 'USD', 1, now()); \
         insert into event_data (event_data_id, website_id, website_event_id, data_key, data_type, created_at) \
         values (gen_random_uuid(), $1[1], $1[3], 'key', 1, now())";
    stack.execute(collect, &ids).await;
    stack
        .execute(
            "insert into segment (segment_id, website_id, type, name, parameters) \
             values ($1[4], $1[1], 'segment', 'Segment', '{}')",
            &ids,
        )
        .await;

    let data = [
        "select session_id from session where website_id = any($1)",
        "select event_id from website_event where website_id = any($1)",
        "select event_name from revenue where website_id = any($1)",
        "select data_key from event_data where website_id = any($1)",
        "select segment_id from segment where website_id = any($1)",
        "select website_id, reset_at is not null as reset from website where website_id = any($1)",
    ];

    let reset = format!("/api/websites/{website}/reset");
    for (tokens, status) in [(&outsider, 401), (&owner_tokens, 200)] {
        let (u, r) = stack.call_as(tokens, Method::POST, &reset, None).await;
        assert_eq!(u.status, status, "{u:?}");
        assert_same(&u, &r, &[], "reset");
    }
    for sql in data {
        stack.assert_same_snapshot(sql, &ids, "after a reset").await;
    }

    stack.execute(collect, &ids).await;
    let path = format!("/api/websites/{website}");
    for (tokens, status) in [(&outsider, 401), (&owner_tokens, 200)] {
        let (u, r) = stack.call_as(tokens, Method::DELETE, &path, None).await;
        assert_eq!(u.status, status, "{u:?}");
        assert_same(&u, &r, &[], "delete");
    }
    for sql in data {
        let rows = stack
            .assert_same_snapshot(sql, &ids, "after deleting")
            .await;
        assert_eq!(rows, json!([]), "{sql}");
    }

    // Upstream fails with a 500 for an unknown website; umami-rs answers 404.
    for (method, path) in [
        (Method::DELETE, format!("/api/websites/{}", Uuid::new_v4())),
        (
            Method::POST,
            format!("/api/websites/{}/reset", Uuid::new_v4()),
        ),
    ] {
        let (u, r) = stack.call_as(&admin, method.clone(), &path, None).await;
        assert_eq!(
            (u.status, r.status),
            (500, 404),
            "{method} {path}: {u:?} {r:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn reading_websites_with_share_tokens() {
    let stack = Stack::connect().await;
    let (owner, _) = stack.session("user").await;
    let shared = seed_website(&stack, "Shared", Some(owner.id), None).await;
    let listed = seed_website(&stack, "Listed", Some(owner.id), None).await;
    let private = seed_website(&stack, "Private", Some(owner.id), None).await;
    let secret = crypto::hash(&[APP_SECRET]);

    let share = |claims: Value| jwt::sign(&claims, &secret);
    let single = share(json!({ "type": "share", "websiteId": shared, "iat": 1 }));
    let list = share(json!({ "type": "share", "websiteIds": [listed], "iat": 1 }));

    for (token, website, context, status) in [
        (&single, shared, true, 200),
        (&list, listed, true, 200),
        (&single, private, true, 401),
        (&single, shared, false, 401),
    ] {
        let mut headers = vec![("x-umami-share-token", token.as_str())];
        if context {
            headers.push(("x-umami-share-context", "1"));
        }
        let path = format!("/api/websites/{website}");
        let (u, r) = stack.call_both(Method::GET, &path, &headers, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same(&u, &r, &[], &format!("share token on {path}"));
    }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn upstream_inconsistencies() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (owner, owner_tokens) = stack.session("user").await;
    let website = seed_website(&stack, "Original", Some(owner.id), None).await;
    let path = format!("/api/websites/{website}");

    // A taken share ID refuses the whole update here; upstream renames the
    // website before refusing.
    let taken = format!("parity{}", tag());
    stack
        .execute(
            &format!(
                "insert into share (share_id, entity_id, name, share_type, slug, parameters) \
                 values ($1[1], $1[2], 'Taken', 1, '{taken}', '{{}}')"
            ),
            &[Uuid::new_v4(), Uuid::new_v4()],
        )
        .await;
    let body = json!({ "name": "Renamed", "shareId": taken });
    let (u, r) = stack
        .call_as(&owner_tokens, Method::POST, &path, Some(&body))
        .await;
    assert_eq!(u.status, 400);
    assert_same(&u, &r, &[], "update with a taken share ID");

    let name = "select name from website where website_id = any($1)";
    assert_eq!(
        stack.upstream.snapshot(name, &[website]).await,
        json!([{ "name": "Renamed" }])
    );
    assert_eq!(
        stack.rs.snapshot(name, &[website]).await,
        json!([{ "name": "Original" }])
    );

    // Admins can name users and teams that do not exist; upstream stores the
    // dangling IDs, umami-rs answers 404.
    let unknown = Uuid::new_v4();
    for (path, body, message) in [
        (
            "/api/websites".to_owned(),
            json!({ "name": "Site", "domain": "example.com", "teamId": unknown }),
            "Team not found.",
        ),
        (
            format!("{path}/transfer"),
            json!({ "teamId": unknown }),
            "Team not found.",
        ),
        (
            format!("{path}/transfer"),
            json!({ "userId": unknown }),
            "User not found",
        ),
    ] {
        let (u, r) = stack
            .call_as(&admin, Method::POST, &path, Some(&body))
            .await;
        assert_eq!(
            (u.status, r.status),
            (200, 404),
            "{path} {body}: {u:?} {r:?}"
        );
        assert_eq!(r.body["error"]["message"], message);
    }
}
