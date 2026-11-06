//! Account self-service parity: password changes and API keys.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use reqwest::Method;
use serde_json::{Value, json};
use umami_parity::{Response, Stack, assert_same, bearer, token};
use uuid::Uuid;

/// The same request to both servers, each with its own session.
async fn both(
    stack: &Stack,
    method: Method,
    path: &str,
    tokens: &[String; 2],
    body: Option<&Value>,
) -> (Response, Response) {
    let [upstream, rs] = tokens.clone().map(|t| bearer(&t));
    (
        stack
            .call(
                &stack.upstream,
                method.clone(),
                path,
                &[("authorization", &upstream)],
                body,
            )
            .await,
        stack
            .call(&stack.rs, method, path, &[("authorization", &rs)], body)
            .await,
    )
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn changing_the_password() {
    let stack = Stack::connect().await;
    let user = stack.create_user("user").await;
    let (u, r) = stack.login(&user.username, "umami").await;
    let tokens = [token(&u), token(&r)];

    for body in [
        json!({ "currentPassword": "umami", "newPassword": "short" }),
        json!({ "newPassword": "long enough" }),
        json!({ "currentPassword": "wrong", "newPassword": "long enough" }),
        json!(null),
    ] {
        let (u, r) = both(
            &stack,
            Method::POST,
            "/api/me/password",
            &tokens,
            Some(&body),
        )
        .await;
        assert_eq!(u.status, 400, "{body}: {u:?}");
        assert_same(&u, &r, &[], &format!("password {body}"));
    }

    // Without a session: a valid body is refused as unauthenticated.
    let body = json!({ "currentPassword": "umami", "newPassword": "long enough" });
    let (u, r) = stack
        .call_both(Method::POST, "/api/me/password", &[], Some(&body))
        .await;
    assert_eq!(u.status, 401);
    assert_same(&u, &r, &[], "password without a session");

    let (u, r) = both(
        &stack,
        Method::POST,
        "/api/me/password",
        &tokens,
        Some(&body),
    )
    .await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_same(&u, &r, &[], "password change");

    // The change signs out existing sessions, and the new password works.
    let (u, r) = both(&stack, Method::GET, "/api/me", &tokens, None).await;
    assert_eq!(u.status, 401);
    assert_same(&u, &r, &[], "old session");

    let (u, r) = stack.login(&user.username, "long enough").await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_same(&u, &r, &["token"], "login with the new password");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn managing_api_keys() {
    let stack = Stack::connect().await;
    let user = stack.create_user("user").await;
    let (u, r) = stack.login(&user.username, "umami").await;
    let tokens = [token(&u), token(&r)];

    let (u, r) = both(&stack, Method::GET, "/api/me/api-keys", &tokens, None).await;
    assert_eq!(u.body, json!([]));
    assert_same(&u, &r, &[], "no keys");

    for name in ["", "   ", &"n".repeat(256)] {
        let body = json!({ "name": name });
        let (u, r) = both(
            &stack,
            Method::POST,
            "/api/me/api-keys",
            &tokens,
            Some(&body),
        )
        .await;
        assert_eq!(u.status, 400);
        assert_same(&u, &r, &[], &format!("key named {name:?}"));
    }

    // Creation returns the key once; names are trimmed.
    let body = json!({ "name": "  ci deploy  " });
    let (u, r) = both(
        &stack,
        Method::POST,
        "/api/me/api-keys",
        &tokens,
        Some(&body),
    )
    .await;
    assert_eq!(u.status, 200, "{u:?}");
    assert_same(
        &u,
        &r,
        &["id", "key", "keyPrefix", "createdAt"],
        "create key",
    );
    for created in [&u, &r] {
        let key = created.body["key"].as_str().unwrap();
        assert_eq!(created.body["name"], "ci deploy");
        assert_eq!(key.len(), 38);
        assert!(key.starts_with("umami_"));
        assert_eq!(created.body["keyPrefix"], &key[..14]);
    }
    let keys = [&u, &r].map(|c| c.body["key"].as_str().unwrap().to_owned());
    let ids = [&u, &r].map(|c| c.body["id"].as_str().unwrap().to_owned());

    let (u, r) = both(&stack, Method::GET, "/api/me/api-keys", &tokens, None).await;
    let names = |response: &Response| {
        let list = response.body.as_array().unwrap();
        assert!(
            list.iter().all(|k| k.get("key").is_none()),
            "keys are never listed: {list:?}"
        );
        list.iter().map(|k| k["name"].clone()).collect::<Vec<_>>()
    };
    assert_eq!(names(&u), names(&r));
    assert_eq!(names(&u), [json!("ci deploy")]);

    // The keys authenticate, but cannot manage keys.
    let (u, r) = both(&stack, Method::GET, "/api/me", &keys, None).await;
    assert_eq!(u.body["authType"], "api-key", "{u:?}");
    assert_same(&u, &r, &["token", "apiKey"], "key on /api/me");
    let (u, r) = both(&stack, Method::GET, "/api/me/api-keys", &keys, None).await;
    assert_eq!(u.status, 401);
    assert_same(&u, &r, &[], "key managing keys");

    // Deletion.
    let unknown = format!("/api/me/api-keys/{}", Uuid::new_v4());
    let (u, r) = both(&stack, Method::DELETE, &unknown, &tokens, None).await;
    assert_eq!(u.status, 404);
    assert_same(&u, &r, &[], "delete unknown key");

    let upstream = stack
        .call(
            &stack.upstream,
            Method::DELETE,
            &format!("/api/me/api-keys/{}", ids[0]),
            &[("authorization", &bearer(&tokens[0]))],
            None,
        )
        .await;
    let rs = stack
        .call(
            &stack.rs,
            Method::DELETE,
            &format!("/api/me/api-keys/{}", ids[1]),
            &[("authorization", &bearer(&tokens[1]))],
            None,
        )
        .await;
    assert_eq!(upstream.body, json!({ "ok": true }));
    assert_same(&upstream, &rs, &[], "delete key");

    let (u, r) = both(&stack, Method::GET, "/api/me", &keys, None).await;
    assert_eq!(u.status, 401);
    assert_same(&u, &r, &[], "deleted key");
}
