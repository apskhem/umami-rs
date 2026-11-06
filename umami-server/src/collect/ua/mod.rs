//! User-agent classification, matching upstream's use of `detect-browser`
//! (browser, OS), `isbot` (bots) and `ua-parser-js` (device type).
//!
//! The rule tables in `rules.rs` are generated from those libraries by
//! `tools/ua-rules`; `tests/ua_parity.rs` checks the port against their output.

#[rustfmt::skip]
mod rules;

use std::sync::LazyLock;

use super::js;

/// A JS regular expression translated to Rust syntax by `tools/ua-rules`.
struct Pattern {
    source: &'static str,
    /// Uses lookaround or backreferences, which need `fancy-regex`.
    fancy: bool,
}

struct DeviceRule {
    patterns: &'static [Pattern],
    kind: DeviceKind,
}

/// How a matching device rule determines the device type.
enum DeviceKind {
    /// The rule matches but sets no type.
    Unknown,
    Const(&'static str),
    /// The text of a capture group.
    #[allow(
        dead_code,
        reason = "supported by the generator; unused by ua-parser-js 1.0.41"
    )]
    Group(usize),
    /// ua-parser-js `strMapper` over a capture group: the first entry with a needle
    /// contained (case-insensitively) in the group wins, else `fallback`.
    Map {
        group: usize,
        entries: &'static [(Option<&'static str>, &'static [&'static str])],
        fallback: Option<&'static str>,
    },
}

enum Regex {
    Plain(regex::Regex),
    Fancy(fancy_regex::Regex),
}

impl Regex {
    fn new(pattern: &Pattern) -> Self {
        let invalid = |e: &dyn std::fmt::Display| -> ! {
            panic!("invalid generated pattern {:?}: {e}", pattern.source)
        };

        if pattern.fancy {
            Self::Fancy(fancy_regex::Regex::new(pattern.source).unwrap_or_else(|e| invalid(&e)))
        } else {
            Self::Plain(regex::Regex::new(pattern.source).unwrap_or_else(|e| invalid(&e)))
        }
    }

    fn is_match(&self, haystack: &str) -> bool {
        match self {
            Self::Plain(re) => re.is_match(haystack),
            // A backtracking-limit error counts as no match.
            Self::Fancy(re) => re.is_match(haystack).unwrap_or(false),
        }
    }

    /// `None` if there is no match, otherwise the text of capture group `group`.
    fn group<'h>(&self, haystack: &'h str, group: usize) -> Option<Option<&'h str>> {
        match self {
            Self::Plain(re) => re
                .captures(haystack)
                .map(|caps| caps.get(group).map(|m| m.as_str())),
            Self::Fancy(re) => re
                .captures(haystack)
                .ok()
                .flatten()
                .map(|caps| caps.get(group).map(|m| m.as_str())),
        }
    }
}

struct Compiled {
    browsers: Vec<(&'static str, Regex)>,
    oses: Vec<(&'static str, Regex)>,
    /// The non-fancy bot patterns as a single alternation, plus the fancy ones.
    bots: (regex::Regex, Vec<Regex>),
    devices: Vec<(Vec<Regex>, &'static DeviceKind)>,
}

static COMPILED: LazyLock<Compiled> = LazyLock::new(|| {
    let named = |rules: &'static [(&'static str, Pattern)]| {
        rules
            .iter()
            .map(|(name, pattern)| (*name, Regex::new(pattern)))
            .collect()
    };

    let plain_bots = rules::BOT_PATTERNS
        .iter()
        .filter(|p| !p.fancy)
        .map(|p| format!("(?:{})", p.source))
        .collect::<Vec<_>>()
        .join("|");

    Compiled {
        browsers: named(rules::BROWSER_RULES),
        oses: named(rules::OS_RULES),
        bots: (
            regex::Regex::new(&plain_bots).expect("invalid generated bot pattern"),
            rules::BOT_PATTERNS
                .iter()
                .filter(|p| p.fancy)
                .map(Regex::new)
                .collect(),
        ),
        devices: rules::DEVICE_RULES
            .iter()
            .map(|rule| (rule.patterns.iter().map(Regex::new).collect(), &rule.kind))
            .collect(),
    }
});

/// Compiles every rule now rather than on the first request.
pub fn init() {
    LazyLock::force(&COMPILED);
}

/// isbot's `isbot(userAgent)`.
pub fn is_bot(user_agent: &str) -> bool {
    let (plain, fancy) = &COMPILED.bots;

    !user_agent.is_empty()
        && (plain.is_match(user_agent) || fancy.iter().any(|re| re.is_match(user_agent)))
}

/// detect-browser's `browserName(userAgent)`.
pub fn browser_name(user_agent: &str) -> Option<&'static str> {
    if user_agent.is_empty() {
        return None;
    }

    COMPILED
        .browsers
        .iter()
        .find(|(_, re)| re.is_match(user_agent))
        .map(|(name, _)| *name)
}

/// detect-browser's `detectOS(userAgent)`.
pub fn detect_os(user_agent: &str) -> Option<&'static str> {
    COMPILED
        .oses
        .iter()
        .find(|(_, re)| re.is_match(user_agent))
        .map(|(name, _)| *name)
}

/// ua-parser-js's `UAParser(userAgent).device.type`.
pub fn device_type(user_agent: &str) -> Option<String> {
    let user_agent = clamp_length(user_agent);

    for (patterns, kind) in &COMPILED.devices {
        let group = match kind {
            DeviceKind::Group(group) | DeviceKind::Map { group, .. } => *group,
            DeviceKind::Unknown | DeviceKind::Const(_) => 0,
        };

        for re in patterns {
            let Some(text) = re.group(user_agent, group) else {
                continue;
            };

            // The first matching rule decides, even if it sets no type.
            return match kind {
                DeviceKind::Unknown => None,
                DeviceKind::Const(value) => Some((*value).to_owned()),
                DeviceKind::Group(_) => text.map(str::to_owned),
                DeviceKind::Map {
                    entries, fallback, ..
                } => text.and_then(|text| map_string(text, entries, *fallback)),
            };
        }
    }

    None
}

/// ua-parser-js only inspects the first 500 characters (after leading whitespace)
/// of longer user agents.
fn clamp_length(user_agent: &str) -> &str {
    const MAX: usize = 500;

    if user_agent.encode_utf16().nth(MAX).is_none() {
        user_agent
    } else {
        js::truncate(user_agent.trim_start_matches(js::is_js_whitespace), MAX)
    }
}

fn map_string(
    text: &str,
    entries: &[(Option<&'static str>, &'static [&'static str])],
    fallback: Option<&'static str>,
) -> Option<String> {
    let lower = text.to_lowercase();

    for (result, needles) in entries {
        if needles
            .iter()
            .any(|needle| lower.contains(&needle.to_lowercase()))
        {
            return result.map(str::to_owned);
        }
    }

    Some(fallback.map_or_else(|| text.to_owned(), str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_rules_compile() {
        init();
    }

    #[test]
    fn classifies_common_user_agents() {
        let chrome_mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

        assert_eq!(browser_name(chrome_mac), Some("chrome"));
        assert_eq!(detect_os(chrome_mac), Some("Mac OS"));
        assert_eq!(device_type(chrome_mac), None);
        assert!(!is_bot(chrome_mac));

        assert_eq!(browser_name(iphone), Some("ios"));
        assert_eq!(detect_os(iphone), Some("iOS"));
        assert_eq!(device_type(iphone).as_deref(), Some("mobile"));

        assert!(is_bot("Googlebot/2.1 (+http://www.google.com/bot.html)"));
        assert!(!is_bot(""));
        assert_eq!(browser_name(""), None);
    }
}
