//! Page and referrer URL handling for events, as in upstream's `/api/send` route.

use url::Url;

use super::js;

/// The URL-derived columns of a `website_event` row.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub url_path: String,
    pub url_query: String,
    pub hostname: String,
    pub referrer_path: Option<String>,
    pub referrer_query: Option<String>,
    pub referrer_domain: Option<String>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub utm_content: Option<String>,
    pub utm_term: Option<String>,
    pub gclid: Option<String>,
    pub fbclid: Option<String>,
    pub msclkid: Option<String>,
    pub ttclid: Option<String>,
    pub lifatid: Option<String>,
    pub twclid: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid page URL")]
pub struct InvalidUrl;

/// Resolves `url` against the reported `hostname`, as upstream's `new URL(url, base)`.
/// JS stringifies a missing URL to `"undefined"`; that quirk is reproduced so the
/// result (an empty path) matches.
fn resolve(url: Option<&str>, hostname: Option<&str>) -> Result<Url, InvalidUrl> {
    let base = match hostname.filter(|h| !h.is_empty()) {
        Some(hostname) => Url::parse(&format!("https://{hostname}")).map_err(|_| InvalidUrl)?,
        None => super::payload::localhost(),
    };

    Url::options()
        .base_url(Some(&base))
        .parse(url.unwrap_or("undefined"))
        .map_err(|_| InvalidUrl)
}

fn strip_www(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

/// The page fields of an `event` payload.
pub fn event_page(
    url: Option<&str>,
    hostname: Option<&str>,
    referrer: Option<&str>,
    remove_trailing_slash: bool,
) -> Result<Page, InvalidUrl> {
    let current = resolve(url, hostname)?;

    let mut url_path = match current.path() {
        "/undefined" => String::new(),
        path => match current.fragment().filter(|f| !f.is_empty()) {
            Some(fragment) => format!("{path}#{fragment}"),
            None => path.to_owned(),
        },
    };

    if remove_trailing_slash {
        url_path = remove_trailing(&url_path);
    }

    let url_domain = strip_www(current.host_str().unwrap_or("")).to_owned();
    let param = |name: &str| {
        current
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };

    let mut page = Page {
        url_path: js::safe_decode_uri(&url_path).into_owned(),
        url_query: current.query().unwrap_or("").to_owned(),
        hostname: hostname
            .filter(|h| !h.is_empty())
            .map_or_else(|| url_domain.clone(), str::to_owned),
        utm_source: param("utm_source"),
        utm_medium: param("utm_medium"),
        utm_campaign: param("utm_campaign"),
        utm_content: param("utm_content"),
        utm_term: param("utm_term"),
        gclid: param("gclid"),
        fbclid: param("fbclid"),
        msclkid: param("msclkid"),
        ttclid: param("ttclid"),
        lifatid: param("li_fat_id"),
        twclid: param("twclid"),
        ..Page::default()
    };

    if let Some(referrer) = referrer.filter(|r| !r.is_empty()) {
        // The event's own domain, canonicalized (lowercase, punycode, no port) so
        // self-referrals compare equal.
        let event_domain = match hostname.filter(|h| !h.is_empty()) {
            Some(hostname) => Url::parse(&format!("https://{hostname}"))
                .ok()
                .and_then(|u| u.host_str().map(|h| strip_www(h).to_owned()))
                .unwrap_or_else(|| strip_www(hostname).to_owned()),
            None => url_domain.clone(),
        };

        let base = if event_domain.is_empty() {
            None
        } else {
            Url::parse(&format!("https://{event_domain}")).ok()
        };
        let referrer_url = Url::options()
            .base_url(Some(&base.unwrap_or_else(super::payload::localhost)))
            .parse(referrer)
            .map_err(|_| InvalidUrl)?;

        let domain = strip_www(referrer_url.host_str().unwrap_or("")).to_owned();

        page.referrer_path = Some(js::safe_decode_uri(referrer_url.path()).into_owned());
        page.referrer_query = Some(referrer_url.query().unwrap_or("").to_owned());
        page.referrer_domain = (domain != event_domain).then_some(domain);
    }

    Ok(page)
}

/// The path of a `performance` payload (no fragment, not decoded).
pub fn performance_path(url: Option<&str>, hostname: Option<&str>) -> Result<String, InvalidUrl> {
    let current = resolve(url, hostname)?;

    Ok(match current.path() {
        "/undefined" => String::new(),
        path => path.to_owned(),
    })
}

/// `REMOVE_TRAILING_SLASH`: upstream's `path.replace(/(?!^)\/(?=(#.*)?$)/, '')`,
/// which removes the first non-leading slash that is followed by the end of the path
/// or by a fragment.
fn remove_trailing(path: &str) -> String {
    let slash = path.match_indices('/').map(|(i, _)| i).find(|&i| {
        let rest = &path[i + 1..];
        i > 0 && (rest.is_empty() || rest.starts_with('#'))
    });

    match slash {
        Some(i) => format!("{}{}", &path[..i], &path[i + 1..]),
        None => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values computed with upstream's route logic in Node.js.

    #[test]
    fn utm_params_and_referrer() {
        let page = event_page(
            Some("/docs?utm_source=news+letter&utm_campaign=a%20b&gclid=x"),
            Some("www.Example.com"),
            Some("https://www.google.com/search?q=umami"),
            false,
        )
        .unwrap();

        assert_eq!(page.url_path, "/docs");
        assert_eq!(
            page.url_query,
            "utm_source=news+letter&utm_campaign=a%20b&gclid=x"
        );
        assert_eq!(page.hostname, "www.Example.com");
        assert_eq!(page.referrer_path.as_deref(), Some("/search"));
        assert_eq!(page.referrer_query.as_deref(), Some("q=umami"));
        assert_eq!(page.referrer_domain.as_deref(), Some("google.com"));
        assert_eq!(page.utm_source.as_deref(), Some("news letter"));
        assert_eq!(page.utm_campaign.as_deref(), Some("a b"));
        assert_eq!(page.gclid.as_deref(), Some("x"));
        assert_eq!(page.utm_medium, None);
    }

    #[test]
    fn absolute_url_with_fragment_and_self_referral() {
        let page = event_page(
            Some("https://www.example.com/a/b/#section"),
            Some(""),
            Some("/internal?x=1"),
            false,
        )
        .unwrap();

        assert_eq!(page.url_path, "/a/b/#section");
        assert_eq!(page.url_query, "");
        assert_eq!(page.hostname, "example.com");
        assert_eq!(page.referrer_path.as_deref(), Some("/internal"));
        assert_eq!(page.referrer_query.as_deref(), Some("x=1"));
        assert_eq!(page.referrer_domain, None);
    }

    #[test]
    fn missing_url_and_empty_referrer() {
        let page = event_page(None, Some("example.com"), Some(""), false).unwrap();

        assert_eq!(page.url_path, "");
        assert_eq!(page.hostname, "example.com");
        assert_eq!(page.referrer_path, None);
        assert_eq!(page.referrer_domain, None);
    }

    #[test]
    fn decodes_path_and_canonicalizes_domains() {
        let page = event_page(
            Some("/caf%C3%A9/%2F?"),
            Some("example.com"),
            Some("https://EXAMPLE.com:8443/x"),
            false,
        )
        .unwrap();

        assert_eq!(page.url_path, "/café/%2F");
        assert_eq!(page.url_query, "");
        assert_eq!(page.referrer_path.as_deref(), Some("/x"));
        assert_eq!(page.referrer_domain, None);
    }

    #[test]
    fn hostname_with_port_and_app_referrer() {
        let page = event_page(
            Some("/p?x=1&x=2"),
            Some("example.com:8080"),
            Some("android-app://com.google.android.gm/"),
            false,
        )
        .unwrap();

        assert_eq!(page.url_path, "/p");
        assert_eq!(page.url_query, "x=1&x=2");
        assert_eq!(page.hostname, "example.com:8080");
        assert_eq!(page.referrer_path.as_deref(), Some("/"));
        assert_eq!(
            page.referrer_domain.as_deref(),
            Some("com.google.android.gm")
        );
    }

    #[test]
    fn removes_trailing_slash_except_root() {
        assert_eq!(remove_trailing("/docs/"), "/docs");
        assert_eq!(remove_trailing("/docs/#top"), "/docs#top");
        assert_eq!(remove_trailing("/"), "/");
        assert_eq!(remove_trailing("/#top"), "/#top");
        assert_eq!(remove_trailing("/docs"), "/docs");
        assert_eq!(remove_trailing("/a#b/"), "/a#b");
        assert_eq!(remove_trailing("/docs/#top/"), "/docs#top/");
    }

    #[test]
    fn performance_path_has_no_fragment() {
        assert_eq!(
            performance_path(Some("/a%20b#x"), Some("example.com")).unwrap(),
            "/a%20b"
        );
        assert_eq!(performance_path(None, None).unwrap(), "");
    }
}
