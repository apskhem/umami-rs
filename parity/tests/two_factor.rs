//! Two-factor authentication parity: setup, login completion, lockout and admin
//! policies must behave as upstream's.
//!
//! Each server keeps its own secrets, so flows run on both in step and their
//! responses are compared. Tests here run one at a time: the global policy affects
//! every user.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use umami_parity::{
    APP_SECRET, Response, Stack, TWO_FACTOR_KEY, TestUser, assert_same, bearer, token,
};
use umami_server::auth::{totp, two_factor};
use umami_server::{crypto, jwt};
use uuid::Uuid;

static SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// The current code, after waiting out the end of a 30-second step so both
/// servers see the same step.
async fn fresh_code(secret: &str) -> String {
    if now() % 30 >= 27 {
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
    totp::code_at(secret, now()).unwrap()
}

fn key() -> [u8; 32] {
    hex::decode(TWO_FACTOR_KEY).unwrap().try_into().unwrap()
}

/// A request to one server with a bearer token.
async fn call(
    stack: &Stack,
    server: usize,
    method: Method,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> Response {
    let target = stack.targets()[server];
    let auth = bearer(token);
    stack
        .call(
            target,
            method,
            path,
            &[("authorization", &auth)],
            body.as_ref(),
        )
        .await
}

/// The same request to both servers, each with its own token.
async fn both(
    stack: &Stack,
    method: Method,
    path: &str,
    tokens: &[String; 2],
    bodies: [Option<Value>; 2],
) -> [Response; 2] {
    let [upstream_body, rs_body] = bodies;
    [
        call(stack, 0, method.clone(), path, &tokens[0], upstream_body).await,
        call(stack, 1, method, path, &tokens[1], rs_body).await,
    ]
}

async fn sessions(stack: &Stack, user: &TestUser) -> [String; 2] {
    let (upstream, rs) = stack.login(&user.username, "umami").await;
    [token(&upstream), token(&rs)]
}

/// Enables 2FA directly in both databases with a known secret.
async fn enable_directly(stack: &Stack, user: &TestUser, secret: &str) {
    let stored = two_factor::encrypt_secret(&key(), secret);
    for target in stack.targets() {
        sqlx::query(
            "insert into two_factor_auth (id, user_id, secret, is_enabled, updated_at) \
             values ($1, $2, $3, true, now())",
        )
        .bind(format!("c{}", Uuid::new_v4().simple()))
        .bind(user.id)
        .bind(&stored)
        .execute(&target.db)
        .await
        .unwrap();
    }
}

/// Forgets used codes, as if their 90-second replay window had passed.
async fn expire_used_codes(stack: &Stack, user: &TestUser) {
    for target in stack.targets() {
        sqlx::query("delete from two_factor_otp_used where user_id = $1")
            .bind(user.id)
            .execute(&target.db)
            .await
            .unwrap();
    }
}

async fn partial_tokens(stack: &Stack, user: &TestUser) -> [String; 2] {
    let (upstream, rs) = stack.login(&user.username, "umami").await;
    assert_same(&upstream, &rs, &["partialToken"], "2FA login");
    assert_eq!(upstream.body["requiresTwoFactor"], true, "{upstream:?}");

    [&upstream, &rs].map(|r| r.body["partialToken"].as_str().unwrap().to_owned())
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn setup_login_and_disable() {
    let _serial = SERIAL.lock().await;
    let stack = Stack::connect().await;
    let user = stack.create_user("user").await;
    let tokens = sessions(&stack, &user).await;

    let [u, r] = both(
        &stack,
        Method::GET,
        "/api/2fa/status",
        &tokens,
        [None, None],
    )
    .await;
    assert_eq!(u.body["isEnabled"], false);
    assert_same(&u, &r, &[], "status before setup");

    // Setup: each server issues its own secret.
    let initiated = both(
        &stack,
        Method::POST,
        "/api/2fa/setup/initiate",
        &tokens,
        [None, None],
    )
    .await;
    let secrets: Vec<String> = initiated
        .iter()
        .map(|response| {
            assert_eq!(response.status, 200, "{response:?}");
            assert!(
                response.body["qrCodeDataUrl"]
                    .as_str()
                    .unwrap()
                    .starts_with("data:image/")
            );
            response.body["manualKey"].as_str().unwrap().to_owned()
        })
        .collect();
    assert!(secrets.iter().all(|s| s.len() == 32));

    let wrong = Some(json!({ "token": "999999" }));
    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/setup/confirm",
        &tokens,
        [wrong.clone(), wrong],
    )
    .await;
    assert_same(&u, &r, &[], "confirm with a wrong code");

    let codes = [fresh_code(&secrets[0]).await, fresh_code(&secrets[1]).await];
    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/setup/confirm",
        &tokens,
        [
            Some(json!({ "token": codes[0] })),
            Some(json!({ "token": codes[1] })),
        ],
    )
    .await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_eq!(r.status, 200, "{r:?}");
    let backup_codes: Vec<String> = [&u, &r]
        .map(|resp| resp.body["backupCodes"][0].as_str().unwrap().to_owned())
        .to_vec();
    assert_eq!(r.body["backupCodes"].as_array().unwrap().len(), 10);

    let [u, r] = both(
        &stack,
        Method::GET,
        "/api/2fa/status",
        &tokens,
        [None, None],
    )
    .await;
    assert_eq!(u.body["isEnabled"], true);
    assert_same(&u, &r, &[], "status after setup");

    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/setup/initiate",
        &tokens,
        [None, None],
    )
    .await;
    assert_same(&u, &r, &[], "initiate when enabled");

    // Login now needs the second step. A backup code works once.
    let partial = partial_tokens(&stack, &user).await;
    let backup = |code: &str| Some(json!({ "backupCode": code.to_lowercase() }));
    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/verify",
        &partial,
        [backup(&backup_codes[0]), backup(&backup_codes[1])],
    )
    .await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_same(&u, &r, &["token"], "verify with a backup code");

    // A session from either server's 2FA step works on the other.
    for (server, session) in [(1, token(&u)), (0, token(&r))] {
        let me = call(&stack, server, Method::GET, "/api/me", &session, None).await;
        assert_eq!(me.status, 200, "{me:?}");
    }

    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/verify",
        &partial,
        [backup(&backup_codes[0]), backup(&backup_codes[1])],
    )
    .await;
    assert_same(&u, &r, &[], "reused backup code");

    // A TOTP code works once.
    expire_used_codes(&stack, &user).await;
    let codes = [fresh_code(&secrets[0]).await, fresh_code(&secrets[1]).await];
    let code_bodies = || {
        [
            Some(json!({ "token": codes[0] })),
            Some(json!({ "token": codes[1] })),
        ]
    };
    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/verify",
        &partial,
        code_bodies(),
    )
    .await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_same(&u, &r, &["token"], "verify with a code");

    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/verify",
        &partial,
        code_bodies(),
    )
    .await;
    assert_same(&u, &r, &[], "replayed code");

    // Disabling needs the password and an unused code.
    let disable = |password: &str, code: &str| Some(json!({ "password": password, "token": code }));
    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/disable",
        &tokens,
        [disable("wrong", &codes[0]), disable("wrong", &codes[1])],
    )
    .await;
    assert_same(&u, &r, &[], "disable with a wrong password");

    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/disable",
        &tokens,
        [disable("umami", &codes[0]), disable("umami", &codes[1])],
    )
    .await;
    assert_same(&u, &r, &[], "disable with a used code");

    expire_used_codes(&stack, &user).await;
    let codes = [fresh_code(&secrets[0]).await, fresh_code(&secrets[1]).await];
    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/disable",
        &tokens,
        [disable("umami", &codes[0]), disable("umami", &codes[1])],
    )
    .await;
    assert_eq!(u.body, json!({ "ok": true }), "{u:?}");
    assert_same(&u, &r, &[], "disable");

    let (u, r) = stack.login(&user.username, "umami").await;
    assert_same(&u, &r, &["token"], "login after disabling");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn codes_tolerate_clock_drift() {
    let _serial = SERIAL.lock().await;
    let stack = Stack::connect().await;
    let user = stack.create_user("user").await;
    let secret = totp::generate_secret();
    enable_directly(&stack, &user, &secret).await;
    let partial = partial_tokens(&stack, &user).await;

    // The previous step's code, as from a phone clock running 30 seconds slow:
    // upstream accepts only the current step, umami-rs one step either side.
    fresh_code(&secret).await;
    let drifted = totp::code_at(&secret, now() - 30).unwrap();
    let body = || Some(json!({ "token": drifted }));
    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/verify",
        &partial,
        [body(), body()],
    )
    .await;
    assert_eq!((u.status, r.status), (400, 200), "{u:?} {r:?}");
    assert!(r.body["token"].is_string(), "{r:?}");

    // Two steps off is still refused.
    let partial = partial_tokens(&stack, &user).await;
    let far = totp::code_at(&secret, now() - 60).unwrap();
    let body = || Some(json!({ "token": far }));
    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/verify",
        &partial,
        [body(), body()],
    )
    .await;
    assert_eq!(u.status, 400, "{u:?}");
    assert_same(&u, &r, &[], "a code two steps old");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn wrong_codes_lock_the_user_out() {
    let _serial = SERIAL.lock().await;
    let stack = Stack::connect().await;
    let user = stack.create_user("user").await;
    let secret = totp::generate_secret();
    enable_directly(&stack, &user, &secret).await;
    let partial = partial_tokens(&stack, &user).await;

    let wrong = totp::code_at(&secret, now() + 3600).unwrap();
    let body = || Some(json!({ "token": wrong }));

    for attempt in 1..=6 {
        let [u, r] = both(
            &stack,
            Method::POST,
            "/api/2fa/verify",
            &partial,
            [body(), body()],
        )
        .await;
        assert_same(&u, &r, &[], &format!("attempt {attempt}"));

        // The fifth failure starts the lockout; later attempts are refused.
        let locked = u.body["error"].get("lockedUntil").is_some();
        assert_eq!(locked, attempt >= 5, "attempt {attempt}: {u:?}");
        assert_eq!(
            r.body["error"].get("lockedUntil").is_some(),
            locked,
            "attempt {attempt}: {r:?}"
        );
        assert_eq!(
            u.status,
            if attempt >= 6 { 429 } else { 400 },
            "attempt {attempt}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn verify_rejections_match_upstream() {
    let _serial = SERIAL.lock().await;
    let stack = Stack::connect().await;
    let user = stack.create_user("user").await;
    let secret = crypto::hash(&[APP_SECRET]);
    let session = token(&stack.login(&user.username, "umami").await.0);

    // A valid partial token for a user without 2FA.
    let partial = jwt::sign_secure(
        &json!({ "userId": user.id, "type": "partial-auth", "iat": now(), "exp": now() + 300 }),
        &secret,
    );

    let cases: Vec<(Option<String>, Value)> = vec![
        (None, json!({ "token": "123456" })),
        (Some("garbage".into()), json!({ "token": "123456" })),
        (Some(session), json!({ "token": "123456" })),
        (Some(partial.clone()), json!({ "token": "12345" })),
        (
            Some(partial.clone()),
            json!({ "token": "123456", "extra": 1 }),
        ),
        (
            Some(partial.clone()),
            json!({ "token": "123456", "backupCode": "x" }),
        ),
        (Some(partial.clone()), json!({ "backupCode": "" })),
        (Some(partial.clone()), json!(null)),
        (Some(partial), json!({ "token": "123456" })),
    ];

    for (token, body) in cases {
        let auth = token.as_deref().map(bearer);
        let headers: Vec<(&str, &str)> =
            auth.iter().map(|a| ("authorization", a.as_str())).collect();
        let (u, r) = stack
            .call_both(Method::POST, "/api/2fa/verify", &headers, Some(&body))
            .await;
        assert_same(&u, &r, &[], &format!("verify {token:?} {body}"));
    }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn admin_policies_match_upstream() {
    let _serial = SERIAL.lock().await;
    let stack = Stack::connect().await;
    let admin = stack.create_user("admin").await;
    let member = stack.create_user("user").await;
    let admin_tokens = sessions(&stack, &admin).await;
    let member_tokens = sessions(&stack, &member).await;

    let team_id = Uuid::new_v4();
    for target in stack.targets() {
        sqlx::query("insert into team (team_id, name) values ($1, 'Parity')")
            .bind(team_id)
            .execute(&target.db)
            .await
            .unwrap();
        sqlx::query("insert into team_user (team_user_id, team_id, user_id, role) values ($1, $2, $3, 'team-member')")
            .bind(Uuid::new_v4())
            .bind(team_id)
            .bind(member.id)
            .execute(&target.db)
            .await
            .unwrap();
    }

    let required = |value: bool| Some(json!({ "required": value }));
    let user_path = format!("/api/admin/users/{}/2fa", member.id);
    let team_path = format!("/api/admin/teams/{team_id}/2fa");
    let status = async || {
        let [u, r] = both(
            &stack,
            Method::GET,
            "/api/2fa/status",
            &member_tokens,
            [None, None],
        )
        .await;
        assert_same(&u, &r, &[], "member status");
        u.body["requiredReason"].clone()
    };
    let disable_attempt = async || {
        let body = Some(json!({ "password": "umami", "token": "123456" }));
        let [u, r] = both(
            &stack,
            Method::POST,
            "/api/2fa/disable",
            &member_tokens,
            [body.clone(), body],
        )
        .await;
        assert_same(&u, &r, &[], "disable while required");
        u.status
    };

    // Non-admins are refused (401, as upstream).
    for (method, path, body) in [
        (Method::POST, "/api/admin/2fa/global", required(true)),
        (Method::GET, user_path.as_str(), None),
        (Method::POST, user_path.as_str(), required(true)),
        (Method::DELETE, user_path.as_str(), None),
        (Method::POST, team_path.as_str(), required(true)),
        (
            Method::POST,
            "/api/admin/2fa/global",
            Some(json!({ "required": "yes" })),
        ),
    ] {
        let [u, r] = both(&stack, method, path, &member_tokens, [body.clone(), body]).await;
        assert_same(&u, &r, &[], &format!("non-admin {path}"));
    }

    assert_eq!(status().await, Value::Null);

    for (path, reason) in [
        ("/api/admin/2fa/global", "global"),
        (user_path.as_str(), "user"),
        (team_path.as_str(), "team"),
    ] {
        let [u, r] = both(
            &stack,
            Method::POST,
            path,
            &admin_tokens,
            [required(true), required(true)],
        )
        .await;
        assert_eq!(u.status, 200, "{path}: {u:?}");
        assert_same(&u, &r, &[], path);

        assert_eq!(status().await, json!(reason), "{path}");
        assert_eq!(disable_attempt().await, 403, "{path}");

        let [u, r] = both(
            &stack,
            Method::POST,
            path,
            &admin_tokens,
            [required(false), required(false)],
        )
        .await;
        assert_same(&u, &r, &[], path);
    }

    assert_eq!(status().await, Value::Null);

    // Per-user status and reset.
    enable_directly(&stack, &member, &totp::generate_secret()).await;
    for method in [Method::GET, Method::DELETE, Method::GET] {
        let [u, r] = both(
            &stack,
            method.clone(),
            &user_path,
            &admin_tokens,
            [None, None],
        )
        .await;
        assert_eq!(u.status, 200, "{method} {u:?}");
        assert_same(&u, &r, &[], &format!("{method} {user_path}"));
    }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn setup_cancel_and_validation_match_upstream() {
    let _serial = SERIAL.lock().await;
    let stack = Stack::connect().await;
    let user = stack.create_user("user").await;
    let tokens = sessions(&stack, &user).await;

    both(
        &stack,
        Method::POST,
        "/api/2fa/setup/initiate",
        &tokens,
        [None, None],
    )
    .await;
    let [u, r] = both(
        &stack,
        Method::POST,
        "/api/2fa/setup/cancel",
        &tokens,
        [None, None],
    )
    .await;
    assert_same(&u, &r, &[], "cancel");

    // Nothing pending any more, and malformed bodies are rejected first.
    for body in [
        json!({ "token": "123456" }),
        json!({ "token": "12345" }),
        json!({}),
        json!(null),
    ] {
        let [u, r] = both(
            &stack,
            Method::POST,
            "/api/2fa/setup/confirm",
            &tokens,
            [Some(body.clone()), Some(body.clone())],
        )
        .await;
        assert_same(&u, &r, &[], &format!("confirm {body}"));
    }

    // Unauthenticated: invalid bodies are 400s before auth is checked, as upstream.
    for (path, body) in [
        ("/api/2fa/setup/confirm", json!({ "token": "123456" })),
        ("/api/2fa/setup/confirm", json!({})),
        ("/api/2fa/disable", json!({ "password": "x" })),
        ("/api/2fa/setup/initiate", json!({})),
    ] {
        let (u, r) = stack.call_both(Method::POST, path, &[], Some(&body)).await;
        assert_same(&u, &r, &[], &format!("unauthenticated {path} {body}"));
    }
}
