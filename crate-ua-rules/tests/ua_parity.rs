//! Checks the user-agent port against what the JS libraries upstream Umami uses
//! return for a corpus of real user agents (`tools/ua-rules/fixtures.js`).

use crate_ua_rules as ua;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    ua: String,
    bot: bool,
    browser: Option<String>,
    os: Option<String>,
    /// ua-parser-js 1.x, the version ported.
    device: Option<String>,
    /// ua-parser-js 2.x, the version upstream runs.
    device_upstream: Option<String>,
}

fn cases() -> Vec<Case> {
    serde_json::from_str(include_str!("fixtures/ua.json")).unwrap()
}

fn check(field: &str, actual: impl Fn(&Case) -> String, expected: impl Fn(&Case) -> String) {
    let cases = cases();
    let failures: Vec<String> = cases
        .iter()
        .filter(|case| actual(case) != expected(case))
        .map(|case| {
            format!(
                "  {:?}: got {}, want {}",
                case.ua,
                actual(case),
                expected(case)
            )
        })
        .collect();

    assert!(
        failures.is_empty(),
        "{field}: {} of {} user agents differ:\n{}",
        failures.len(),
        cases.len(),
        failures
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn is_bot_matches_isbot() {
    check(
        "bot",
        |c| ua::is_bot(&c.ua).to_string(),
        |c| c.bot.to_string(),
    );
}

#[test]
fn browser_matches_detect_browser() {
    check(
        "browser",
        |c| format!("{:?}", ua::browser_name(&c.ua)),
        |c| format!("{:?}", c.browser.as_deref()),
    );
}

#[test]
fn os_matches_detect_browser() {
    check(
        "os",
        |c| format!("{:?}", ua::detect_os(&c.ua)),
        |c| format!("{:?}", c.os.as_deref()),
    );
}

#[test]
fn device_matches_ua_parser_js() {
    check(
        "device",
        |c| format!("{:?}", ua::device_type(&c.ua)),
        |c| format!("{:?}", c.device),
    );
}

/// Not a parity requirement (2.x is AGPL and not ported); guards against the
/// drift growing unnoticed when the fixtures are regenerated.
#[test]
fn device_drift_from_upstream_stays_small() {
    let cases = cases();
    let drift = cases
        .iter()
        .filter(|c| ua::device_type(&c.ua) != c.device_upstream)
        .count();

    assert!(
        drift * 100 <= cases.len(),
        "device type differs from ua-parser-js 2.x for {drift} of {} user agents",
        cases.len()
    );
}
