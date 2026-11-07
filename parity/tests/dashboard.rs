//! Dashboard parity: the pages upstream's Next.js server renders and the static
//! build umami-rs serves (`dashboard.rs`) answer the same paths with the same
//! status, redirects and security headers. The page bodies differ by design
//! (server-rendered vs. one client-rendered shell); the browser-level behaviour
//! is covered by the HTTP tests and the README.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use umami_parity::Stack;

/// Fetches `path` from both servers without following redirects.
async fn fetch_both(stack: &Stack, path: &str) -> [reqwest::Response; 2] {
  let http = reqwest::Client::builder()
    .redirect(reqwest::redirect::Policy::none())
    .build()
    .unwrap();
  let fetch = |base: &str| http.get(format!("{base}{path}")).send();

  [
    fetch(&stack.upstream.url).await.unwrap(),
    fetch(&stack.rs.url).await.unwrap(),
  ]
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn dashboard_paths_match_upstream() {
  let stack = Stack::connect().await;
  let website = stack.website().await;
  let team = uuid::Uuid::new_v4();

  for path in [
    // Pages.
    "/".to_owned(),
    "/login".to_owned(),
    "/websites".to_owned(),
    format!("/websites/{website}"),
    format!("/websites/{website}/realtime"),
    format!("/websites/{website}/sessions/{team}"),
    format!("/teams/{team}/websites"),
    format!("/teams/{team}/websites/{website}/events"),
    "/settings/preferences".to_owned(),
    "/admin/users".to_owned(),
    "/share/abc123/events".to_owned(),
    // `next.config.ts` redirects and Next's trailing-slash redirect.
    "/settings".to_owned(),
    "/admin?tab=1".to_owned(),
    format!("/teams/{team}"),
    format!("/teams/{team}/settings"),
    format!("/teams/{team}/dashboard"),
    format!("/teams/{team}/dashboard/edit"),
    "/websites/".to_owned(),
    // Not pages.
    "/nope".to_owned(),
    "/websites/a/b/c/d".to_owned(),
  ] {
    let [u, r] = fetch_both(&stack, &path).await;

    assert_eq!(u.status(), r.status(), "{path}");
    assert_eq!(u.headers().get("location"), r.headers().get("location"), "{path}");
    for name in [
      "content-type",
      "cache-control",
      "content-security-policy",
      "x-dns-prefetch-control",
    ] {
      assert_eq!(u.headers().get(name), r.headers().get(name), "{path}: {name}");
    }
  }
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn dashboard_serves_public_files() {
  let stack = Stack::connect().await;

  for path in [
    "/favicon.ico",
    "/site.webmanifest",
    "/intl/messages/en-US.json",
    "/images/os/mac-os.png",
  ] {
    let [u, r] = fetch_both(&stack, path).await;

    assert_eq!(u.status(), r.status(), "{path}");
    assert_eq!(
      u.headers().get("content-type"),
      r.headers().get("content-type"),
      "{path}"
    );
    assert_eq!(u.bytes().await.unwrap(), r.bytes().await.unwrap(), "{path}");
  }
}
