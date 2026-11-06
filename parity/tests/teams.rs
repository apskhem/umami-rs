//! Team management parity: `/api/teams/*`, `/api/me/teams` and `/api/admin/teams`.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use reqwest::Method;
use serde_json::{Value, json};
use umami_parity::{Stack, TestUser, assert_same, assert_same_by, mask};
use uuid::Uuid;

/// Hides the values each server generates for records it creates, and orders
/// membership lists (which upstream returns in no particular order).
fn generated(value: &mut Value) {
    fn walk(value: &mut Value) {
        // Memberships are identified by team and user; their own IDs are random.
        if value.get("teamId").is_some() && value.get("userId").is_some() {
            mask(value, &["id"]);
        }
        match value {
            Value::Object(object) => {
                if let Some(Value::Array(members)) = object.get_mut("members") {
                    members.sort_by_key(|m| m["userId"].to_string());
                }
                object.values_mut().for_each(walk);
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }

    walk(value);
    mask(value, &["createdAt", "updatedAt"]);
}

struct Team {
    id: Uuid,
    access_code: String,
}

/// Creates the same team, with the same members, in both databases.
async fn seed_team(stack: &Stack, members: &[(&TestUser, &str)]) -> Team {
    let id = Uuid::new_v4();
    let access_code = format!("team_{}", &Uuid::new_v4().simple().to_string()[..16]);

    stack
        .execute(
            &format!(
                "insert into team (team_id, name, access_code, created_at, updated_at) \
                 values ($1[1], 'Parity team', '{access_code}', \
                 '2026-01-02T03:04:05.678Z', '2026-01-02T03:04:05.678Z')"
            ),
            &[id],
        )
        .await;
    for (user, role) in members {
        stack
            .execute(
                &format!(
                    "insert into team_user (team_user_id, team_id, user_id, role, created_at, updated_at) \
                     values ($1[1], $1[2], $1[3], '{role}', now(), now())"
                ),
                &[Uuid::new_v4(), id, user.id],
            )
            .await;
    }

    Team { id, access_code }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn creating_teams() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (owner, user) = stack.session("user").await;
    let (_, viewer) = stack.session("view-only").await;

    for body in [
        json!(null),
        json!({}),
        json!({ "name": "n".repeat(51) }),
        json!({ "name": "Team", "ownerId": "not-a-uuid" }),
    ] {
        let (u, r) = stack
            .call_as(&user, Method::POST, "/api/teams", Some(&body))
            .await;
        assert_eq!(u.status, 400, "{body}");
        assert_same(&u, &r, &[], &format!("create {body}"));
    }

    let (u, r) = stack
        .call_as(
            &viewer,
            Method::POST,
            "/api/teams",
            Some(&json!({ "name": "Team" })),
        )
        .await;
    assert_eq!(u.status, 401);
    assert_same(&u, &r, &[], "create as view-only");

    // The creator owns the team; `ownerId` counts only for admins.
    let body = json!({ "name": "Parity team", "ownerId": Uuid::new_v4() });
    let (u, r) = stack
        .call_as(&user, Method::POST, "/api/teams", Some(&body))
        .await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_eq!(u.body[1]["userId"], json!(owner.id));
    assert_eq!(u.body[1]["role"], "team-owner");
    let normalize = |value: &mut Value| {
        generated(value);
        mask(value, &["id", "teamId", "accessCode"]);
    };
    assert_same_by(&u, &r, normalize, "create");
    for response in [&u, &r] {
        let code = response.body[0]["accessCode"].as_str().unwrap();
        assert!(code.starts_with("team_") && code.len() == 21, "{code}");
    }

    let (other, _) = stack.session("user").await;
    let body = json!({ "name": "Parity team", "ownerId": other.id });
    let (u, r) = stack
        .call_as(&admin, Method::POST, "/api/teams", Some(&body))
        .await;
    assert_eq!(u.body[1]["userId"], body["ownerId"]);
    assert_same_by(&u, &r, normalize, "create for another owner");

    // An owner that does not exist: upstream creates the team anyway.
    let body = json!({ "name": "Parity team", "ownerId": Uuid::new_v4() });
    let (u, r) = stack
        .call_as(&admin, Method::POST, "/api/teams", Some(&body))
        .await;
    assert_eq!((u.status, r.status), (200, 404), "{u:?} {r:?}");
    assert_eq!(r.body["error"]["message"], "User not found");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn viewing_and_updating_teams() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (owner, owner_tokens) = stack.session("user").await;
    let (manager, manager_tokens) = stack.session("user").await;
    let (member, member_tokens) = stack.session("user").await;
    let (_, outsider) = stack.session("user").await;
    let team = seed_team(
        &stack,
        &[
            (&owner, "team-owner"),
            (&manager, "team-manager"),
            (&member, "team-member"),
        ],
    )
    .await;
    let path = format!("/api/teams/{}", team.id);
    let renamed = format!("Renamed {}", &Uuid::new_v4().simple().to_string()[..10]);

    for (tokens, path, status) in [
        (&member_tokens, path.clone(), 200),
        (&admin, path.clone(), 200),
        (&outsider, path.clone(), 401),
        (&admin, format!("/api/teams/{}", Uuid::new_v4()), 404),
        (&outsider, format!("/api/teams/{}", Uuid::new_v4()), 401),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same_by(&u, &r, generated, &format!("GET {path}"));
    }

    for (tokens, body, status) in [
        (&manager_tokens, json!({ "name": "n".repeat(51) }), 400),
        (&manager_tokens, json!({ "accessCode": 5 }), 400),
        (&member_tokens, json!({ "name": renamed }), 401),
        (&outsider, json!({ "name": renamed }), 401),
        (&manager_tokens, json!({ "name": renamed }), 200),
        (
            &owner_tokens,
            json!({ "accessCode": format!("{}x", team.access_code) }),
            200,
        ),
        (&admin, json!({}), 200),
    ] {
        let (u, r) = stack
            .call_as(tokens, Method::POST, &path, Some(&body))
            .await;
        assert_eq!(u.status, status, "{body}: {u:?}");
        assert_same_by(&u, &r, generated, &format!("update {body}"));
    }

    stack
        .assert_same_snapshot(
            "select team_id, name, access_code, deleted_at from team where team_id = any($1)",
            &[team.id],
            "updated team",
        )
        .await;

    // Upstream lists teams in no particular order, so each list has one team.
    for (tokens, path) in [
        (&member_tokens, "/api/teams"),
        (
            &member_tokens,
            "/api/me/teams?orderBy=name&sortDescending=true",
        ),
        (
            &member_tokens,
            &format!("/api/users/{}/teams?page=1&pageSize=5", member.id),
        ),
        (&admin, &format!("/api/users/{}/teams", member.id)),
        (
            &admin,
            &format!("/api/admin/teams?search={renamed}&orderBy=name"),
        ),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, path, None).await;
        assert_eq!(u.status, 200, "{path}: {u:?}");
        assert_same_by(&u, &r, generated, &format!("GET {path}"));
    }

    let (u, _) = stack
        .call_as(&member_tokens, Method::GET, "/api/teams", None)
        .await;
    assert_eq!(u.body["count"], 1);
    assert_eq!(
        u.body["data"][0]["_count"],
        json!({ "websites": 0, "members": 3 })
    );

    for (tokens, path, status) in [
        (&member_tokens, "/api/admin/teams", 401),
        (&member_tokens, "/api/teams?sortDescending=1", 400),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, path, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same(&u, &r, &[], &format!("GET {path}"));
    }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn managing_members() {
    let stack = Stack::connect().await;
    let (_, admin) = stack.session("admin").await;
    let (owner, owner_tokens) = stack.session("user").await;
    let (manager, manager_tokens) = stack.session("user").await;
    let (member, member_tokens) = stack.session("user").await;
    let (viewer, viewer_tokens) = stack.session("user").await;
    let (newcomer, newcomer_tokens) = stack.session("user").await;
    let (_, outsider) = stack.session("user").await;
    let team = seed_team(
        &stack,
        &[
            (&owner, "team-owner"),
            (&manager, "team-manager"),
            (&member, "team-member"),
            (&viewer, "team-view-only"),
        ],
    )
    .await;
    let users = format!("/api/teams/{}/users", team.id);
    let user_path = |user: &TestUser| format!("{users}/{}", user.id);

    // Listing members (oldest first) needs membership.
    for (tokens, path, status) in [
        (&viewer_tokens, users.clone(), 200),
        (
            &viewer_tokens,
            format!("{users}?search={}", &member.username[7..]),
            200,
        ),
        (&viewer_tokens, format!("{users}?pageSize=2&page=2"), 200),
        (&outsider, users.clone(), 401),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same_by(&u, &r, generated, &format!("GET {path}"));
    }

    // Adding members: managers and owners only, never as owner, never twice.
    for (tokens, body, status) in [
        (
            &manager_tokens,
            json!({ "userId": newcomer.id, "role": "team-owner" }),
            400,
        ),
        (
            &manager_tokens,
            json!({ "userId": "nope", "role": "team-member" }),
            400,
        ),
        (
            &member_tokens,
            json!({ "userId": newcomer.id, "role": "team-member" }),
            401,
        ),
        (
            &manager_tokens,
            json!({ "userId": member.id, "role": "team-member" }),
            400,
        ),
        (
            &manager_tokens,
            json!({ "userId": newcomer.id, "role": "team-view-only" }),
            200,
        ),
    ] {
        let (u, r) = stack
            .call_as(tokens, Method::POST, &users, Some(&body))
            .await;
        assert_eq!(u.status, status, "{body}: {u:?}");
        assert_same_by(&u, &r, generated, &format!("add {body}"));
    }

    // Reading a membership needs team:update; unknown members are `null`.
    for (tokens, path, status) in [
        (&manager_tokens, user_path(&member), 200),
        (&manager_tokens, format!("{users}/{}", Uuid::new_v4()), 200),
        (&member_tokens, user_path(&viewer), 401),
    ] {
        let (u, r) = stack.call_as(tokens, Method::GET, &path, None).await;
        assert_eq!(u.status, status, "{path}: {u:?}");
        assert_same_by(&u, &r, generated, &format!("GET {path}"));
    }

    // Changing roles: only members ranked below the actor.
    let role = |role: &str| json!({ "role": role });
    for (tokens, target, body, status) in [
        (&manager_tokens, &member, role("team-owner"), 400),
        (&member_tokens, &viewer, role("team-member"), 401),
        (&manager_tokens, &owner, role("team-member"), 401),
        (&manager_tokens, &manager, role("team-member"), 401),
        (&manager_tokens, &member, role("team-view-only"), 200),
        (&owner_tokens, &manager, role("team-member"), 200),
        (&admin, &owner, role("team-manager"), 200),
    ] {
        let path = user_path(target);
        let (u, r) = stack
            .call_as(tokens, Method::POST, &path, Some(&body))
            .await;
        assert_eq!(u.status, status, "{path} {body}: {u:?}");
        assert_same_by(&u, &r, generated, &format!("update {path} {body}"));
    }
    let (u, r) = stack
        .call_as(
            &admin,
            Method::POST,
            &format!("{users}/{}", Uuid::new_v4()),
            Some(&role("team-member")),
        )
        .await;
    assert_eq!(u.status, 400);
    assert_same(&u, &r, &[], "update a non-member");

    // Removing members. The owner is now a manager and the manager a member.
    for (tokens, target, status) in [
        (&member_tokens, &owner, 401),
        (&viewer_tokens, &viewer, 200),
        (&viewer_tokens, &viewer, 400),
        (&owner_tokens, &manager, 200),
        (&admin, &member, 200),
        (&admin, &member, 400),
    ] {
        let path = user_path(target);
        let (u, r) = stack.call_as(tokens, Method::DELETE, &path, None).await;
        assert_eq!(u.status, status, "DELETE {path}: {u:?}");
        assert_same(&u, &r, &[], &format!("DELETE {path}"));
    }

    // Joining by access code.
    for (tokens, body, status) in [
        (&outsider, json!({ "accessCode": "c".repeat(51) }), 400),
        (&outsider, json!({ "accessCode": "team_unknown" }), 404),
        (
            &newcomer_tokens,
            json!({ "accessCode": team.access_code }),
            400,
        ),
        (&outsider, json!({ "accessCode": team.access_code }), 200),
    ] {
        let (u, r) = stack
            .call_as(tokens, Method::POST, "/api/teams/join", Some(&body))
            .await;
        assert_eq!(u.status, status, "{body}: {u:?}");
        assert_same_by(&u, &r, generated, &format!("join {body}"));
    }

    stack
        .assert_same_snapshot(
            "select user_id, role from team_user where team_id = any($1) order by user_id",
            &[team.id],
            "members",
        )
        .await;
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn deleting_teams() {
    let stack = Stack::connect().await;
    let (owner, owner_tokens) = stack.session("user").await;
    let (manager, manager_tokens) = stack.session("user").await;
    let team = seed_team(
        &stack,
        &[(&owner, "team-owner"), (&manager, "team-manager")],
    )
    .await;
    let [website, board] = std::array::from_fn(|_| Uuid::new_v4());
    let ids = [team.id, website, board];

    stack
        .execute(
            "insert into website (website_id, name, domain, team_id) \
             values ($1[2], 'Team site', 'team.example', $1[1]); \
             insert into board (board_id, type, name, description, parameters, team_id) \
             values ($1[3], 'dashboard', 'Board', '', '{}', $1[1])",
            &ids,
        )
        .await;

    let path = format!("/api/teams/{}", team.id);
    for (tokens, status) in [(&manager_tokens, 401), (&owner_tokens, 200)] {
        let (u, r) = stack.call_as(tokens, Method::DELETE, &path, None).await;
        assert_eq!(u.status, status, "{u:?}");
        assert_same(&u, &r, &[], "delete team");
    }

    // The team, its members and boards are deleted on both servers.
    for sql in [
        "select team_id from team where team_id = any($1)",
        "select user_id from team_user where team_id = any($1)",
        "select board_id from board where board_id = any($1)",
    ] {
        stack
            .assert_same_snapshot(sql, &ids, "after deleting a team")
            .await;
    }

    // Upstream warns that deleting a team deletes its websites, but keeps them
    // without a team; umami-rs deletes them.
    let websites = "select website_id, team_id from website where website_id = any($1)";
    assert_eq!(
        stack.upstream.snapshot(websites, &ids).await,
        json!([{ "website_id": website, "team_id": null }])
    );
    assert_eq!(stack.rs.snapshot(websites, &ids).await, json!([]));
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn upstream_inconsistencies() {
    let stack = Stack::connect().await;
    let (owner, owner_tokens) = stack.session("user").await;
    let team = seed_team(&stack, &[(&owner, "team-owner")]).await;
    let other = seed_team(&stack, &[(&owner, "team-owner")]).await;

    // Adding a user that does not exist: upstream stores the membership, which
    // then lists with `user: null`; umami-rs answers 404.
    let body = json!({ "userId": Uuid::new_v4(), "role": "team-member" });
    let (u, r) = stack
        .call_as(
            &owner_tokens,
            Method::POST,
            &format!("/api/teams/{}/users", team.id),
            Some(&body),
        )
        .await;
    assert_eq!((u.status, r.status), (200, 404), "{u:?} {r:?}");
    assert_eq!(r.body["error"]["message"], "User not found");

    // Another team's access code: a 500 upstream, a 400 here.
    let body = json!({ "accessCode": other.access_code });
    let (u, r) = stack
        .call_as(
            &owner_tokens,
            Method::POST,
            &format!("/api/teams/{}", team.id),
            Some(&body),
        )
        .await;
    assert_eq!((u.status, r.status), (500, 400), "{u:?} {r:?}");
    assert_eq!(
        r.body["error"]["message"],
        "That access code is already taken."
    );
}
