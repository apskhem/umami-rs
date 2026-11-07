//! Telemetry script parity: `/telemetry.js` and `/api/scripts/telemetry`. The
//! stack sets `DISABLE_TELEMETRY`, so both serve the disabled script; the
//! enabled one is checked against upstream's template in the HTTP tests.
//!
//! Requires the stack in `parity/compose.yaml`; run with `cargo test -p umami-parity -- --ignored`.

use umami_parity::Stack;

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml)"]
async fn telemetry_script_matches_upstream() {
  let stack = Stack::connect().await;

  for path in ["/telemetry.js", "/api/scripts/telemetry"] {
    let fetch = |base: &str| stack.http().get(format!("{base}{path}")).send();
    let (u, r) = (
      fetch(&stack.upstream.url).await.unwrap(),
      fetch(&stack.rs.url).await.unwrap(),
    );
    assert_eq!(u.status(), r.status(), "{path}");
    for name in [
      "content-type",
      "cache-control",
      "access-control-allow-origin",
      "access-control-allow-methods",
    ] {
      assert_eq!(u.headers().get(name), r.headers().get(name), "{path}: {name}");
    }
    let (u, r) = (u.text().await.unwrap(), r.text().await.unwrap());
    assert_eq!(u, "/* telemetry disabled */", "{path}");
    assert_eq!(u, r, "{path}");
  }
}
