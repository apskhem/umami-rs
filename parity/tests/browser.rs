//! A real browser running the tracker against both servers.
//!
//! Headless Chrome loads a page from another origin that embeds each server's
//! `/script.js`, then makes a pageview, a custom event, a client-side navigation
//! and an identify call. Every request must succeed (including Chrome's CORS
//! preflight) and both databases must end up with the same rows.
//!
//! Requires the stack in `parity/compose.yaml` and Chrome or Chromium (found
//! automatically, or set `PARITY_CHROME`); skipped if no browser is found.
//! Run with `cargo test -p umami-parity -- --ignored`.

use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::response::Html;
use axum::routing::get;
use serde::Deserialize;
use serde_json::Value;
use tokio::net::TcpListener;
use umami_parity::{Stack, Table, Target, token_claims};
use uuid::Uuid;

/// Headless Chrome's own user agent contains "HeadlessChrome", which isbot flags.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// The test page. It logs every tracker request (a CORS failure rejects the fetch),
/// drives the tracker, then writes the log into a JSON script element, which the
/// DOM dump outputs unescaped.
const PAGE: &str = r#"<!doctype html>
<html>
<head>
<title>Browser test</title>
<script>
  window.trackerLog = [];
  const realFetch = window.fetch;
  window.fetch = async (url, options) => {
    const type = JSON.parse(options.body).type;
    try {
      const response = await realFetch(url, options);
      trackerLog.push({ type, url: String(url), status: response.status, body: await response.clone().json() });
      return response;
    } catch (error) {
      trackerLog.push({ type, url: String(url), error: String(error) });
      throw error;
    }
  };
</script>
<script defer src="{script}" data-website-id="{website}"></script>
</head>
<body>
<script>
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  window.addEventListener('load', async () => {
    await sleep(800);
    await umami.track('signup-click', { plan: 'pro', seats: 3 });
    history.pushState({}, '', '/pricing?utm_source=browser-test');
    await sleep(800);
    await umami.identify('user-7', { tier: 'gold' });
    await sleep(500);
    const result = document.createElement('script');
    result.type = 'application/json';
    result.id = 'result';
    result.textContent = JSON.stringify({ log: trackerLog, session: umami.getSession() });
    document.body.appendChild(result);
  });
</script>
</body>
</html>
"#;

#[derive(Debug, Deserialize)]
struct PageResult {
    log: Vec<LoggedRequest>,
    session: Session,
}

#[derive(Debug, Deserialize)]
struct LoggedRequest {
    #[serde(rename = "type")]
    kind: String,
    status: Option<u16>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Session {
    cache: String,
}

/// Chrome or Chromium: `PARITY_CHROME`, the macOS app bundles, or a binary on `PATH`.
fn find_chrome() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PARITY_CHROME") {
        return Some(path.into());
    }

    let bundles = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ];
    let on_path = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();

    bundles
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .or_else(|| {
            on_path
                .iter()
                .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
                .find(|p| p.exists())
        })
}

/// Serves the test page for `target` on an ephemeral port; returns the page URL.
async fn serve_page(target: &Target, website: Uuid) -> String {
    let page = PAGE
        .replace("{script}", &format!("{}/script.js", target.url))
        .replace("{website}", &website.to_string());
    let router = Router::new().route("/page.html", get(move || async move { Html(page) }));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    // `localhost`, so both pages report the same hostname despite different ports.
    format!("http://localhost:{port}/page.html")
}

/// Loads `url` in headless Chrome and returns what the page recorded.
async fn run_browser(chrome: &PathBuf, url: &str) -> PageResult {
    let profile = std::env::temp_dir().join(format!("umami-parity-chrome-{}", Uuid::new_v4()));

    let output = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(chrome)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-first-run",
                "--no-default-browser-check",
                &format!("--user-data-dir={}", profile.display()),
                &format!("--user-agent={USER_AGENT}"),
                "--virtual-time-budget=15000",
                "--dump-dom",
                url,
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("Chrome timed out")
    .expect("failed to run Chrome");

    let _ = std::fs::remove_dir_all(&profile);

    let dom = String::from_utf8_lossy(&output.stdout);
    let json = dom
        .split_once(r#"id="result">"#)
        .and_then(|(_, rest)| rest.split_once("</script>"))
        .map(|(json, _)| json)
        .unwrap_or_else(|| panic!("page did not finish; DOM:\n{dom}"));

    serde_json::from_str(json).unwrap()
}

/// Rows for `website` without their timestamps (the two browser runs happen a
/// moment apart).
async fn rows_without_time(target: &Target, website: Uuid) -> Vec<(Table, Value)> {
    let mut all = Vec::new();

    for table in Table::ALL {
        for mut row in target.rows(table, website).await {
            row.as_object_mut().unwrap().remove("created_at");
            all.push((table, row));
        }
    }

    all
}

#[tokio::test]
#[ignore = "requires the parity stack (parity/compose.yaml) and Chrome"]
async fn tracker_in_a_real_browser() {
    let Some(chrome) = find_chrome() else {
        eprintln!("skipping: no Chrome or Chromium found (set PARITY_CHROME)");
        return;
    };

    let stack = Stack::connect().await;
    let website = stack.website().await;

    let mut sessions = Vec::new();

    for target in [&stack.upstream, &stack.rs] {
        let url = serve_page(target, website).await;
        let result = run_browser(&chrome, &url).await;

        let kinds: Vec<&str> = result.log.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["event", "event", "event", "identify"],
            "{}: {result:?}",
            target.name
        );

        for request in &result.log {
            assert_eq!(
                request.error, None,
                "{}: request failed: {result:?}",
                target.name
            );
            assert_eq!(request.status, Some(200), "{}: {result:?}", target.name);
        }

        sessions.push(token_claims(&result.session.cache).expect("valid cache token"));
    }

    assert_eq!(sessions[0], sessions[1], "final session differs");

    let upstream = rows_without_time(&stack.upstream, website).await;
    let rs = rows_without_time(&stack.rs, website).await;

    assert_eq!(upstream.len(), 9, "unexpected rows: {upstream:#?}");
    assert_eq!(upstream, rs);
}
