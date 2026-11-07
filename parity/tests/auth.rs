//! Authentication parity: login, token verification, API keys and share tokens
//! must behave as upstream's, and tokens issued by either server must be accepted
//! by the other.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use aes_gcm::aead::{
  Aead,
  KeyInit,
};
use aes_gcm::{
  Aes256Gcm,
  Nonce,
};
use reqwest::Method;
use serde_json::{
  Value,
  json,
};
use umami_parity::fixtures::{
  secret,
  share_token,
};
use umami_parity::{
  Stack,
  TWO_FACTOR_KEY,
  assert_same,
  bearer,
  token,
};
use umami_server::{
  crypto,
  jwt,
};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn login_matches_upstream() {
  let stack = Stack::connect().await;
  let user = stack.create_user("admin").await;

  let (upstream, rs) = stack.login(&user.username, "umami").await;
  assert_eq!(upstream.status, 200, "{upstream:?}");
  assert_same(&upstream, &rs, &["token"], "login");

  // Usernames match case-insensitively. Upstream echoes the username as typed;
  // umami-rs returns it as stored.
  let typed = user.username.to_uppercase();
  let (upstream, rs) = stack.login(&typed, "umami").await;
  assert_eq!(upstream.body["user"]["username"], json!(typed));
  assert_eq!(rs.body["user"]["username"], json!(user.username));
  let mut rs = rs;
  rs.body["user"]["username"] = json!(typed);
  assert_same(&upstream, &rs, &["token"], "login with another case");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn login_failures_match_upstream() {
  let stack = Stack::connect().await;
  let user = stack.create_user("user").await;

  let cases = [
    json!({ "username": user.username, "password": "wrong" }),
    json!({ "username": "no-such-user", "password": "umami" }),
    json!({ "username": user.username }),
    json!({ "username": 5, "password": "umami" }),
    json!(null),
  ];

  for body in cases {
    let (upstream, rs) = stack.call_both(Method::POST, "/api/auth/login", &[], Some(&body)).await;
    assert_same(&upstream, &rs, &[], &format!("login {body}"));
  }

  // Deleted users cannot sign in.
  for target in stack.targets() {
    sqlx::query(r#"update "user" set deleted_at = now() where user_id = $1"#)
      .bind(user.id)
      .execute(&target.db)
      .await
      .unwrap();
  }
  let (upstream, rs) = stack.login(&user.username, "umami").await;
  assert_eq!(upstream.status, 401);
  assert_same(&upstream, &rs, &[], "deleted user");
}

/// A token issued by either server works on both.
#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn session_tokens_are_interchangeable() {
  let stack = Stack::connect().await;
  let user = stack.create_user("user").await;
  let (upstream_login, rs_login) = stack.login(&user.username, "umami").await;

  for token in [token(&upstream_login), token(&rs_login)] {
    let auth = bearer(&token);
    let headers = [("authorization", auth.as_str())];

    for (method, path) in [
      (Method::POST, "/api/auth/verify"),
      (Method::GET, "/api/me"),
      (Method::POST, "/api/auth/logout"),
    ] {
      let (upstream, rs) = stack.call_both(method, path, &headers, None).await;

      assert_ne!(upstream.status, 401, "{path} rejected the token: {upstream:?}");
      assert_same(&upstream, &rs, &[], path);
    }
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn sso_issues_a_day_long_session() {
  let stack = Stack::connect().await;
  let user = stack.create_user("user").await;
  let (_, rs_login) = stack.login(&user.username, "umami").await;
  let auth = bearer(&token(&rs_login));
  let headers = [("authorization", auth.as_str())];

  // Upstream needs Redis for this and fails without it; umami-rs issues a
  // self-contained token that both servers accept.
  let (upstream, rs) = stack.call_both(Method::POST, "/api/auth/sso", &headers, None).await;
  assert_eq!((upstream.status, rs.status), (500, 200), "{upstream:?} {rs:?}");

  let (_, me) = stack.call_both(Method::GET, "/api/me", &headers, None).await;
  assert_eq!(rs.body["user"], me.body["user"]);

  let sso = bearer(rs.body["token"].as_str().unwrap());
  let (upstream, rs) = stack
    .call_both(Method::GET, "/api/me", &[("authorization", &sso)], None)
    .await;
  assert_eq!(upstream.status, 200, "{upstream:?}");
  assert_same(&upstream, &rs, &["token"], "the sso token");

  let claims: Value = jwt::verify_secure(sso.trim_start_matches("Bearer "), &secret()).unwrap();
  let lifetime = claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap();
  assert_eq!(lifetime, 24 * 60 * 60, "{claims}");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn changing_the_password_revokes_tokens() {
  let stack = Stack::connect().await;
  let user = stack.create_user("user").await;
  let (upstream_login, rs_login) = stack.login(&user.username, "umami").await;

  let new_hash = bcrypt::hash("changed", 4).unwrap();
  for target in stack.targets() {
    sqlx::query(r#"update "user" set password = $2 where user_id = $1"#)
      .bind(user.id)
      .bind(&new_hash)
      .execute(&target.db)
      .await
      .unwrap();
  }

  for token in [token(&upstream_login), token(&rs_login)] {
    let auth = bearer(&token);
    let (upstream, rs) = stack
      .call_both(Method::GET, "/api/me", &[("authorization", &auth)], None)
      .await;

    assert_eq!(upstream.status, 401, "{upstream:?}");
    assert_same(&upstream, &rs, &[], "revoked token");
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn unauthenticated_requests_match_upstream() {
  let stack = Stack::connect().await;
  let cache_token = json!({ "type": "cache", "sessionId": "x" });

  for authorization in [
    None,
    Some("Bearer".to_owned()),
    Some("Bearer garbage".to_owned()),
    Some(bearer(&jwt::sign(&cache_token, &secret()))),
    Some(bearer(&jwt::sign_secure(
      &json!({ "userId": Uuid::new_v4() }),
      &secret(),
    ))),
  ] {
    let headers: Vec<(&str, &str)> = authorization
      .as_deref()
      .map(|value| vec![("authorization", value)])
      .unwrap_or_default();

    for (method, path) in [(Method::GET, "/api/me"), (Method::POST, "/api/auth/verify")] {
      let (upstream, rs) = stack.call_both(method, path, &headers, None).await;

      assert_eq!(upstream.status, 401, "{path} {authorization:?}: {upstream:?}");
      assert_same(&upstream, &rs, &[], path);
    }
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn api_keys_match_upstream() {
  let stack = Stack::connect().await;
  let user = stack.create_user("user").await;
  let key_id = Uuid::new_v4();
  let key = format!("umami_{}", Uuid::new_v4().simple());

  for target in stack.targets() {
    sqlx::query(
      "insert into api_key (api_key_id, user_id, name, key_hash, key_prefix) \
             values ($1, $2, 'parity', $3, $4)",
    )
    .bind(key_id)
    .bind(user.id)
    .bind(crypto::hash(&[&key]))
    .bind(&key[..14])
    .execute(&target.db)
    .await
    .unwrap();
  }

  let auth = bearer(&key);
  let headers = [("authorization", auth.as_str())];

  let (upstream, rs) = stack.call_both(Method::GET, "/api/me", &headers, None).await;
  assert_eq!(upstream.body["authType"], "api-key", "{upstream:?}");
  assert_same(&upstream, &rs, &[], "api key /api/me");

  // Credential and admin routes refuse API keys.
  let (upstream, rs) = stack.call_both(Method::POST, "/api/auth/verify", &headers, None).await;
  assert_eq!(upstream.status, 401);
  assert_same(&upstream, &rs, &[], "api key on /api/auth");

  // Unknown keys are rejected.
  let unknown = bearer("umami_unknown");
  let (upstream, rs) = stack
    .call_both(Method::GET, "/api/me", &[("authorization", &unknown)], None)
    .await;
  assert_eq!(upstream.status, 401);
  assert_same(&upstream, &rs, &[], "unknown api key");
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn share_tokens_match_upstream() {
  let stack = Stack::connect().await;
  let share = share_token(json!({ "shareId": "abc" }));

  let with_context = [("x-umami-share-token", share.as_str()), ("x-umami-share-context", "1")];
  let (upstream, rs) = stack.call_both(Method::GET, "/api/me", &with_context, None).await;
  assert_eq!(upstream.body["authType"], "share", "{upstream:?}");
  assert_same(&upstream, &rs, &[], "share token");

  // Outside a share context, or with a non-share token, access is refused.
  let cache = jwt::sign(&json!({ "type": "cache" }), &secret());
  for headers in [
    vec![("x-umami-share-token", share.as_str())],
    vec![("x-umami-share-token", cache.as_str()), ("x-umami-share-context", "1")],
  ] {
    let (upstream, rs) = stack.call_both(Method::GET, "/api/me", &headers, None).await;
    assert_eq!(upstream.status, 401, "{headers:?}");
    assert_same(&upstream, &rs, &[], "refused share token");
  }
}

/// A TOTP secret as upstream's `encryptSecret` stores it: `ciphertext:iv:tag` (hex).
fn encrypt_totp_secret(secret: &str) -> String {
  let key = hex::decode(TWO_FACTOR_KEY).unwrap();
  let mut iv = [0u8; 12];
  getrandom::fill(&mut iv).unwrap();

  let sealed = Aes256Gcm::new_from_slice(&key)
    .unwrap()
    .encrypt(&Nonce::from(iv), secret.as_bytes())
    .unwrap();
  let (ciphertext, tag) = sealed.split_at(sealed.len() - 16);

  format!("{}:{}:{}", hex::encode(ciphertext), hex::encode(iv), hex::encode(tag))
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn two_factor_logins_issue_partial_tokens() {
  let stack = Stack::connect().await;
  let user = stack.create_user("user").await;
  // otplib requires at least 16 bytes; this is 20.
  let secret = encrypt_totp_secret("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP");

  for target in stack.targets() {
    sqlx::query(
      "insert into two_factor_auth (id, user_id, secret, is_enabled, updated_at) \
             values ($1, $2, $3, true, now())",
    )
    .bind(format!("tf{}", Uuid::new_v4().simple()))
    .bind(user.id)
    .bind(&secret)
    .execute(&target.db)
    .await
    .unwrap();
  }

  let (upstream, rs) = stack.login(&user.username, "umami").await;
  assert_eq!(upstream.body["requiresTwoFactor"], true, "{upstream:?}");
  assert_same(&upstream, &rs, &["partialToken"], "2FA login");

  for partial in [&upstream.body["partialToken"], &rs.body["partialToken"]] {
    let auth = bearer(partial.as_str().unwrap());

    // A partial token is not a session.
    let (upstream_me, rs_me) = stack
      .call_both(Method::GET, "/api/me", &[("authorization", &auth)], None)
      .await;
    assert_eq!(upstream_me.status, 401);
    assert_same(&upstream_me, &rs_me, &[], "partial token on /api/me");

    // Upstream's 2FA step accepts it: a wrong code is reported as such, rather
    // than the token being refused.
    let verify = stack
      .call(
        &stack.upstream,
        Method::POST,
        "/api/2fa/verify",
        &[("authorization", &auth)],
        Some(&json!({ "token": "000000" })),
      )
      .await;
    assert_eq!(
      verify.body["error"]["code"], "two-factor-error-invalid-code",
      "{verify:?}"
    );
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn config_matches_upstream() {
  let stack = Stack::connect().await;
  let (upstream, rs) = stack.call_both(Method::GET, "/api/config", &[], None).await;

  assert_eq!(upstream.status, 200);
  assert_same(&upstream, &rs, &[], "config");
  assert!(matches!(upstream.body, Value::Object(_)));
}
