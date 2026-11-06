//! User management parity: `/api/users/*` and `/api/admin/users`.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use reqwest::Method;
use serde_json::json;
use umami_parity::{Stack, assert_same, assert_same_by, bearer, mask};
use uuid::Uuid;

/// A unique, lowercase tag for names in this test run.
fn tag() -> String {
    Uuid::new_v4().simple().to_string()[..10].to_owned()
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn creating_users() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (_, user) = stack.session("user").await;
    let username = format!("Parity-New-{}", tag());

    // The body is validated before authentication and permissions.
    for body in [
        json!(null),
        json!({ "username": username, "password": "short", "role": "user" }),
        json!({ "username": username, "password": "long enough" }),
        json!({ "username": username, "password": "long enough", "role": "owner" }),
        json!({ "username": "u".repeat(256), "password": "long enough", "role": "user" }),
        json!({ "username": username, "password": "p".repeat(256), "role": "user" }),
        json!({ "id": "not-a-uuid", "username": username, "password": "long enough", "role": "user" }),
        json!({ "id": null, "username": username, "password": "long enough", "role": "user" }),
    ] {
        let (u, r) = stack
            .call_as(&admin, Method::POST, "/api/users", Some(&body))
            .await;
        assert_eq!(u.status, 400, "{body}: {u:?}");
        assert_same(&u, &r, &[], &format!("create {body}"));
    }

    let id = Uuid::new_v4();
    let body =
        json!({ "id": id, "username": username, "password": "long enough", "role": "view-only" });

    let (u, r) = stack
        .call_as(&user, Method::POST, "/api/users", Some(&body))
        .await;
    assert_eq!(u.status, 401);
    assert_same(&u, &r, &[], "create as a non-admin");

    let (u, r) = stack
        .call_both(Method::POST, "/api/users", &[], Some(&body))
        .await;
    assert_eq!(u.status, 401);
    assert_same(&u, &r, &[], "create without a session");

    // Usernames are stored lowercased.
    let (u, r) = stack
        .call_as(&admin, Method::POST, "/api/users", Some(&body))
        .await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_eq!(u.body["username"], json!(username.to_lowercase()));
    assert_same(&u, &r, &[], "create");

    // Taken usernames are refused in any case.
    let again =
        json!({ "username": username.to_uppercase(), "password": "long enough", "role": "user" });
    let (u, r) = stack
        .call_as(&admin, Method::POST, "/api/users", Some(&again))
        .await;
    assert_eq!(u.status, 400);
    assert_same(&u, &r, &[], "duplicate username");

    stack
        .assert_same_snapshot(
            r#"select user_id, username, role, display_name, logo_url, two_factor_required,
               deleted_at, updated_at = created_at as fresh from "user" where user_id = any($1)"#,
            &[id],
            "created user",
        )
        .await;

    // Each server stamped its own creation time.
    let (u, r) = stack.login(&username, "long enough").await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_same_by(
        &u,
        &r,
        |body| {
            body.as_object_mut().unwrap().remove("token");
            mask(body, &["createdAt"]);
        },
        "login as the new user",
    );
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn viewing_and_updating_users() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (member, user) = stack.session("user").await;
    let (other, _) = stack.session("user").await;
    let other_path = format!("/api/users/{}", other.id);
    let member_path = format!("/api/users/{}", member.id);

    for (tokens, path, status) in [
        (&user, member_path.as_str(), 200),
        (&admin, member_path.as_str(), 200),
        (&user, other_path.as_str(), 401),
        // An unknown user is `null`, not a 404.
        (&admin, &format!("/api/users/{}", Uuid::new_v4()), 200),
        // Non-admins are refused before the ID is looked up.
        (&user, "/api/users/not-a-uuid", 401),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, path, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same(&u, &r, &[], &format!("GET {path}"));
    }

    // Non-admins can change only their own password; role and username are ignored.
    let (u, r) = stack
        .call_as(
            &user,
            Method::POST,
            &other_path,
            Some(&json!({ "password": "new password" })),
        )
        .await;
    assert_eq!(u.status, 401);
    assert_same(&u, &r, &[], "update another user");

    for body in [
        json!({ "password": "short" }),
        json!({ "role": "owner" }),
        json!({ "username": 7 }),
        json!([]),
    ] {
        let (u, r) = stack
            .call_as(&admin, Method::POST, &other_path, Some(&body))
            .await;
        assert_eq!(u.status, 400, "{body}");
        assert_same(&u, &r, &[], &format!("update with {body}"));
    }

    // Renaming to a taken username is refused; to a free one, lowercased.
    let (u, r) = stack
        .call_as(
            &admin,
            Method::POST,
            &other_path,
            Some(&json!({ "username": member.username })),
        )
        .await;
    assert_eq!(u.status, 400);
    assert_same(&u, &r, &[], "rename to a taken username");

    let renamed = format!("Parity-Renamed-{}", tag());
    let (u, r) = stack
        .call_as(
            &admin,
            Method::POST,
            &other_path,
            Some(&json!({ "username": renamed, "role": "view-only" })),
        )
        .await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_same(&u, &r, &[], "rename as admin");

    let (u, r) = stack
        .call_as(
            &admin,
            Method::POST,
            &format!("/api/users/{}", Uuid::new_v4()),
            Some(&json!({ "role": "user" })),
        )
        .await;
    assert_eq!(u.status, 404);
    assert_same(&u, &r, &[], "update an unknown user");

    // A non-admin changing their own password: role and username are ignored.
    let (u, r) = stack
        .call_as(
            &user,
            Method::POST,
            &member_path,
            Some(&json!({ "password": "new password", "role": "admin", "username": "x" })),
        )
        .await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_eq!(u.body["role"], "user");
    assert_same(&u, &r, &[], "update self");

    stack
        .assert_same_snapshot(
            r#"select user_id, username, role from "user" where user_id = any($1) order by user_id"#,
            &[member.id, other.id],
            "updated users",
        )
        .await;

    let (u, r) = stack.login(&member.username, "new password").await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_same(&u, &r, &["token"], "login with the changed password");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn listing_users() {
    let stack = Stack::connect().await;
    let (admin, admin_tokens) = stack.session("admin").await;
    let (_, user) = stack.session("user").await;
    let (listed, _) = stack.session("view-only").await;

    // A website, so the count is not zero.
    let website = Uuid::new_v4();
    stack
        .execute(
            "insert into website (website_id, name, domain, user_id, created_at) \
             select $1[1], 'Counted', 'example.com', $1[2], '2026-01-02T03:04:05.678Z'",
            &[website, listed.id],
        )
        .await;

    let search = |username: &str| format!("/api/admin/users?search={}", &username[7..]);
    for (tokens, path, status) in [
        (&admin_tokens, search(&listed.username), 200),
        (
            &admin_tokens,
            format!(
                "{}&orderBy=username&sortDescending=true",
                search(&listed.username)
            ),
            200,
        ),
        // Unknown sort fields fall back to the newest first.
        (
            &admin_tokens,
            format!("{}&orderBy=password", search(&admin.username)),
            200,
        ),
        (
            &admin_tokens,
            format!("{}&page=2&pageSize=1", search(&listed.username)),
            200,
        ),
        (&admin_tokens, "/api/admin/users?page=0".to_owned(), 400),
        (
            &admin_tokens,
            "/api/admin/users?sortDescending=yes".to_owned(),
            400,
        ),
        (&user, search(&listed.username), 401),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same(&u, &r, &[], &format!("GET {path}"));
    }

    let (u, _) = stack
        .call_as(&admin_tokens, Method::GET, &search(&listed.username), None)
        .await;
    assert_eq!(u.body["count"], 1);
    assert_eq!(u.body["data"][0]["_count"]["websites"], 1);

    // Search is case-insensitive and, as in Prisma, `_` is a wildcard.
    let wildcard = format!(
        "/api/admin/users?search={}",
        listed.username.replace('-', "_").to_uppercase()
    );
    let (u, r) = stack
        .call_as(&admin_tokens, Method::GET, &wildcard, None)
        .await;
    assert_eq!(u.body["count"], 1, "{u:?}");
    assert_same(&u, &r, &[], "wildcard search");

    // The user's own lists.
    let teams = format!("/api/users/{}/teams", listed.id);
    let websites = format!("/api/users/{}/websites", listed.id);
    for (tokens, path, status) in [
        (&admin_tokens, teams.as_str(), 200),
        (&admin_tokens, websites.as_str(), 200),
        (&user, teams.as_str(), 401),
        (&user, websites.as_str(), 401),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, path, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same(&u, &r, &[], &format!("GET {path}"));
    }

    // API keys cannot reach user administration.
    let (u, r) = stack
        .call_as(
            &admin_tokens,
            Method::POST,
            "/api/me/api-keys",
            Some(&json!({ "name": "parity" })),
        )
        .await;
    let keys = [&u, &r].map(|response| bearer(response.body["key"].as_str().unwrap()));
    for path in ["/api/admin/users", websites.as_str()] {
        let u = stack
            .call(
                &stack.upstream,
                Method::GET,
                path,
                &[("authorization", &keys[0])],
                None,
            )
            .await;
        let r = stack
            .call(
                &stack.rs,
                Method::GET,
                path,
                &[("authorization", &keys[1])],
                None,
            )
            .await;
        assert_eq!(u.status, 401, "{path}: {u:?}");
        assert_same(&u, &r, &[], &format!("API key on {path}"));
    }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn deleting_users() {
    let stack = Stack::connect().await;
    let (admin, admin_tokens) = stack.session("admin").await;
    let (_, user) = stack.session("user").await;
    let (victim, _) = stack.session("user").await;
    let (colleague, _) = stack.session("user").await;

    let [
        own_site,
        team_site,
        other_team_site,
        own_team,
        other_team,
        link,
        session,
    ] = std::array::from_fn(|_| Uuid::new_v4());
    let ids = [
        victim.id,
        colleague.id,
        own_site,
        team_site,
        other_team_site,
        own_team,
        other_team,
        link,
        session,
    ];

    // The victim owns a website (with collected data and a share), a team with
    // a website and a link, and created a website in a colleague's team.
    stack.execute(
        "insert into website (website_id, name, domain, user_id, created_by) \
         values ($1[3], 'Own', 'own.example', $1[1], $1[1]); \
         insert into session (session_id, website_id) values ($1[9], $1[3]); \
         insert into website_event (event_id, website_id, session_id, visit_id, url_path, event_type) \
         values (gen_random_uuid(), $1[3], $1[9], $1[9], '/', 1); \
         insert into share (share_id, entity_id, name, share_type, slug, parameters) \
         values (gen_random_uuid(), $1[3], 'Own', 1, 'parity-' || $1[3], '{}'); \
         insert into team (team_id, name) values ($1[6], 'Owned'), ($1[7], 'Colleague'); \
         insert into team_user (team_user_id, team_id, user_id, role) values \
           (gen_random_uuid(), $1[6], $1[1], 'team-owner'), \
           (gen_random_uuid(), $1[6], $1[2], 'team-member'), \
           (gen_random_uuid(), $1[7], $1[2], 'team-owner'), \
           (gen_random_uuid(), $1[7], $1[1], 'team-member'); \
         insert into website (website_id, name, domain, team_id, created_by) values \
           ($1[4], 'Team', 'team.example', $1[6], $1[2]), \
           ($1[5], 'Other team', 'other.example', $1[7], $1[1]); \
         insert into link (link_id, name, url, slug, team_id) \
         values ($1[8], 'Link', 'https://example.com', 'parity-' || $1[8], $1[6]); \
         insert into share (share_id, entity_id, name, share_type, slug, parameters) \
         values (gen_random_uuid(), $1[8], 'Link', 2, 'parity-share-' || $1[8], '{}')",
        &ids,
    )
    .await;

    let victim_path = format!("/api/users/{}", victim.id);
    for (tokens, path, status) in [
        (&user, victim_path.clone(), 401),
        (&admin_tokens, format!("/api/users/{}", admin.id), 400),
    ] {
        let (u, r) = stack.call_as(tokens, Method::DELETE, &path, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same(&u, &r, &[], &format!("DELETE {path}"));
    }

    let (u, r) = stack
        .call_as(&admin_tokens, Method::DELETE, &victim_path, None)
        .await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_same(&u, &r, &[], "delete user");

    // Everything the user owned is gone; the team's website and the website in
    // the colleague's team survive without their team or creator.
    let snapshots = [
        r#"select user_id, deleted_at from "user" where user_id = any($1)"#,
        "select website_id, user_id, team_id, created_by, updated_at is null as untouched \
         from website where website_id = any($1) order by website_id",
        "select team_id from team where team_id = any($1) order by team_id",
        "select team_id, user_id, role from team_user where team_id = any($1) order by team_id, user_id",
        "select link_id, user_id, team_id from link where link_id = any($1)",
        "select entity_id, slug from share where entity_id = any($1) order by slug",
        "select session_id from session where session_id = any($1)",
        "select count(*) from website_event where session_id = any($1)",
    ];
    for sql in snapshots {
        stack
            .assert_same_snapshot(sql, &ids, "after deleting a user")
            .await;
    }
    let websites = stack
        .upstream
        .snapshot(
            "select website_id from website where website_id = any($1)",
            &ids,
        )
        .await;
    assert_eq!(websites.as_array().unwrap().len(), 2, "{websites}");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn deleting_users_with_revenue_fails() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (victim, _) = stack.session("user").await;
    let [website, session, event] = std::array::from_fn(|_| Uuid::new_v4());
    let ids = [victim.id, website, session, event];

    // Upstream deletes the website's sessions but not its revenue, which Prisma's
    // emulated relations refuse: the delete fails and nothing changes.
    stack.execute(
        "insert into website (website_id, name, domain, user_id) values ($1[2], 'Shop', 'shop.example', $1[1]); \
         insert into session (session_id, website_id) values ($1[3], $1[2]); \
         insert into website_event (event_id, website_id, session_id, visit_id, url_path, event_type) \
         values ($1[4], $1[2], $1[3], $1[3], '/', 2); \
         insert into revenue (revenue_id, website_id, session_id, event_id, event_name, currency, revenue, created_at) \
         values (gen_random_uuid(), $1[2], $1[3], $1[4], 'buy', 'USD', 1, now())",
        &ids,
    )
    .await;

    let path = format!("/api/users/{}", victim.id);
    let (u, r) = stack.call_as(&admin, Method::DELETE, &path, None).await;
    assert_eq!((u.status, r.status), (500, 500), "{u:?} {r:?}");

    for sql in [
        r#"select user_id from "user" where user_id = any($1)"#,
        "select website_id from website where website_id = any($1)",
        "select event_id from website_event where event_id = any($1)",
    ] {
        let rows = stack
            .assert_same_snapshot(sql, &ids, "after a failed delete")
            .await;
        assert_eq!(rows.as_array().unwrap().len(), 1, "{sql}: {rows}");
    }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn unknown_users_differ_only_in_status() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;

    // Upstream fails with a 500 when deleting a user that does not exist, or
    // looking one up by a malformed ID; umami-rs answers 404.
    for (method, path) in [
        (Method::DELETE, format!("/api/users/{}", Uuid::new_v4())),
        (Method::GET, "/api/users/not-a-uuid".to_owned()),
    ] {
        let (u, r) = stack.call_as(&admin, method.clone(), &path, None).await;
        assert_eq!(u.status, 500, "{method} {path}: {u:?}");
        assert_eq!(r.status, 404, "{method} {path}: {r:?}");
    }
}
