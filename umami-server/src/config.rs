use std::fmt;
use std::path::PathBuf;

use ipnet::IpNet;

use crate::crypto::{self, SaltRotation};

/// Server configuration, read from the same environment variables as upstream Umami
/// where an equivalent exists.
#[derive(Clone)]
pub struct Config {
    /// `DATABASE_URL` (required).
    pub database_url: String,
    /// `DIRECT_DATABASE_URL`: used instead of `DATABASE_URL` for migrations, e.g. to
    /// bypass a connection pooler.
    pub direct_database_url: Option<String>,
    /// Upstream's `secret()`: SHA-512 of `APP_SECRET`, or of `DATABASE_URL` if unset.
    pub secret: String,
    /// `HOST` (default `0.0.0.0`). Upstream's Next.js server reads `HOSTNAME`, but
    /// Docker sets that to the container ID, so it is deliberately not used.
    pub host: String,
    /// `PORT` (default `3000`).
    pub port: u16,
    /// `BASE_PATH`, normalized to `""` or `/segment[/...]` with no trailing slash.
    pub base_path: String,
    /// `SKIP_DB_CHECK`: skip the startup version check and migrations.
    pub skip_db_check: bool,
    /// `SKIP_DB_MIGRATION`: skip startup migrations.
    pub skip_db_migration: bool,
    /// `CORS_MAX_AGE` (default `86400`): `Access-Control-Max-Age` for API responses.
    pub cors_max_age: String,
    /// `TRACKER_SCRIPT_NAME`: comma-separated extra paths that serve `/script.js`.
    pub tracker_script_aliases: Vec<String>,
    /// `TWO_FACTOR_ENCRYPTION_KEY` (64 hex characters): encrypts TOTP secrets. As
    /// upstream, an invalid key means 2FA is not configured rather than an error.
    pub two_factor_key: Option<[u8; 32]>,
    pub collect: CollectConfig,
    pub public: PublicConfig,
}

/// Settings the dashboard reads from `GET /api/config`.
#[derive(Clone, Debug, Default)]
pub struct PublicConfig {
    /// `FAVICON_URL`
    pub favicon_url: Option<String>,
    /// `LINKS_URL`
    pub links_url: Option<String>,
    /// `PIXELS_URL`
    pub pixels_url: Option<String>,
    /// `PRIVATE_MODE`
    pub private_mode: bool,
    /// `DISABLE_TELEMETRY`
    pub telemetry_disabled: bool,
    /// `DISABLE_UPDATES`
    pub updates_disabled: bool,
    /// `TRACKER_SCRIPT_NAME`, as given.
    pub tracker_script_name: Option<String>,
}

/// Settings for event collection (`/api/send`).
#[derive(Clone, Debug)]
pub struct CollectConfig {
    /// `COLLECT_API_ENDPOINT`: an additional path that serves `/api/send`.
    pub endpoint_alias: Option<String>,
    /// `DISABLE_BOT_CHECK`
    pub disable_bot_check: bool,
    /// `IGNORE_IP`: comma-separated addresses and CIDR ranges to drop.
    pub ignore_ips: Vec<IgnoredIp>,
    /// `CLIENT_IP_HEADER`: header to read the client IP from, ahead of the defaults.
    pub client_ip_header: Option<String>,
    /// `SALT_ROTATION` (`day`, `week` or default `month`).
    pub salt_rotation: SaltRotation,
    /// `REMOVE_TRAILING_SLASH`
    pub remove_trailing_slash: bool,
    /// `SKIP_LOCATION_HEADERS`: ignore CDN geolocation headers.
    pub skip_location_headers: bool,
    /// `GEOLITE_DB_PATH`, or upstream's default `geo/GeoLite2-City.mmdb`.
    pub geo_database: PathBuf,
    /// Whether `GEOLITE_DB_PATH` was set (a missing explicit file is an error).
    pub geo_database_explicit: bool,
    /// `USE_UUIDV7`: generate v7 rather than v4 event IDs.
    pub use_uuidv7: bool,
}

/// An `IGNORE_IP` entry. Upstream compares the client IP to each entry as a string,
/// and also tests containment for entries that parse as CIDR ranges.
#[derive(Clone, Debug)]
pub struct IgnoredIp {
    pub entry: String,
    pub range: Option<IpNet>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("DATABASE_URL is not defined")]
    MissingDatabaseUrl,
    #[error("invalid {name}: {value:?}")]
    Invalid { name: &'static str, value: String },
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Builds a config from an arbitrary variable source.
    ///
    /// Empty values count as unset, and flags are on when set to anything at all
    /// (so `SKIP_DB_MIGRATION=0` still skips), matching upstream's JS truthiness.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |key: &str| lookup(key).filter(|value| !value.is_empty());

        let database_url = get("DATABASE_URL").ok_or(ConfigError::MissingDatabaseUrl)?;
        let secret = crypto::hash(&[get("APP_SECRET").as_deref().unwrap_or(&database_url)]);

        let port = match get("PORT") {
            Some(value) => value.parse().map_err(|_| ConfigError::Invalid {
                name: "PORT",
                value,
            })?,
            None => 3000,
        };

        let collect = CollectConfig {
            endpoint_alias: get("COLLECT_API_ENDPOINT")
                .map(|path| format!("/{}", path.trim_start_matches('/'))),
            disable_bot_check: get("DISABLE_BOT_CHECK").is_some(),
            ignore_ips: get("IGNORE_IP")
                .map(|value| {
                    value
                        .split(',')
                        .map(|entry| IgnoredIp {
                            entry: entry.trim().to_owned(),
                            range: entry
                                .trim()
                                .find('/')
                                .filter(|&slash| slash > 0)
                                .and_then(|_| entry.trim().parse().ok()),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            client_ip_header: get("CLIENT_IP_HEADER").map(|name| name.to_ascii_lowercase()),
            salt_rotation: get("SALT_ROTATION")
                .map(|value| SaltRotation::parse(&value))
                .unwrap_or_default(),
            remove_trailing_slash: get("REMOVE_TRAILING_SLASH").is_some(),
            skip_location_headers: get("SKIP_LOCATION_HEADERS").is_some(),
            geo_database_explicit: get("GEOLITE_DB_PATH").is_some(),
            geo_database: get("GEOLITE_DB_PATH")
                .map_or_else(|| PathBuf::from("geo/GeoLite2-City.mmdb"), PathBuf::from),
            use_uuidv7: get("USE_UUIDV7").is_some(),
        };

        let tracker_script_aliases = match get("TRACKER_SCRIPT_NAME") {
            Some(value) => parse_script_aliases(&value)?,
            None => Vec::new(),
        };

        if let Some(endpoint) = &collect.endpoint_alias
            && (endpoint == "/script.js" || tracker_script_aliases.contains(endpoint))
        {
            return Err(ConfigError::Invalid {
                name: "COLLECT_API_ENDPOINT",
                value: endpoint.clone(),
            });
        }

        Ok(Self {
            direct_database_url: get("DIRECT_DATABASE_URL"),
            secret,
            host: get("HOST").unwrap_or_else(|| "0.0.0.0".to_owned()),
            port,
            base_path: normalize_base_path(get("BASE_PATH").as_deref().unwrap_or("")),
            skip_db_check: get("SKIP_DB_CHECK").is_some(),
            skip_db_migration: get("SKIP_DB_MIGRATION").is_some(),
            cors_max_age: get("CORS_MAX_AGE").unwrap_or_else(|| "86400".to_owned()),
            tracker_script_aliases,
            two_factor_key: get("TWO_FACTOR_ENCRYPTION_KEY").and_then(|key| {
                let bytes = hex::decode(&key).ok()?;
                bytes.try_into().ok()
            }),
            public: PublicConfig {
                favicon_url: get("FAVICON_URL"),
                links_url: get("LINKS_URL"),
                pixels_url: get("PIXELS_URL"),
                private_mode: get("PRIVATE_MODE").is_some(),
                telemetry_disabled: get("DISABLE_TELEMETRY").is_some(),
                updates_disabled: get("DISABLE_UPDATES").is_some(),
                tracker_script_name: get("TRACKER_SCRIPT_NAME"),
            },
            collect,
            database_url,
        })
    }

    /// The URL migrations should run against.
    pub fn migration_database_url(&self) -> &str {
        self.direct_database_url
            .as_deref()
            .unwrap_or(&self.database_url)
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("database_url", &"<redacted>")
            .field(
                "direct_database_url",
                &self.direct_database_url.as_ref().map(|_| "<redacted>"),
            )
            .field("secret", &"<redacted>")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("base_path", &self.base_path)
            .field("skip_db_check", &self.skip_db_check)
            .field("skip_db_migration", &self.skip_db_migration)
            .field("cors_max_age", &self.cors_max_age)
            .field("tracker_script_aliases", &self.tracker_script_aliases)
            .field("two_factor_key", &self.two_factor_key.map(|_| "<redacted>"))
            .field("public", &self.public)
            .field("collect", &self.collect)
            .finish()
    }
}

/// Upstream's `TRACKER_SCRIPT_NAME` handling: each name becomes `/<name>` (leading
/// slashes trimmed). Names that are not plain paths, or that would shadow
/// `/script.js` or the API, are rejected.
fn parse_script_aliases(value: &str) -> Result<Vec<String>, ConfigError> {
    let mut aliases: Vec<String> = Vec::new();

    for name in value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let path = format!("/{}", name.trim_start_matches('/'));
        let valid = path.len() > 1
            && !path.starts_with("/api/")
            && path != "/script.js"
            && path
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c));

        if !valid {
            return Err(ConfigError::Invalid {
                name: "TRACKER_SCRIPT_NAME",
                value: name.to_owned(),
            });
        }
        if !aliases.contains(&path) {
            aliases.push(path);
        }
    }

    Ok(aliases)
}

/// `BASE_PATH` normalized to `""` or `/segment[/...]` with no trailing slash.
pub fn normalize_base_path(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('/');

    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        Config::from_lookup(|key| vars.get(key).cloned())
    }

    #[test]
    fn requires_database_url() {
        assert!(matches!(config(&[]), Err(ConfigError::MissingDatabaseUrl)));
        assert!(matches!(
            config(&[("DATABASE_URL", "")]),
            Err(ConfigError::MissingDatabaseUrl)
        ));
    }

    #[test]
    fn defaults() {
        let config = config(&[("DATABASE_URL", "postgresql://localhost/umami")]).unwrap();

        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 3000);
        assert_eq!(config.base_path, "");
        assert!(!config.skip_db_check);
        assert!(!config.skip_db_migration);
        assert_eq!(
            config.migration_database_url(),
            "postgresql://localhost/umami"
        );
    }

    #[test]
    fn secret_falls_back_to_database_url() {
        let url = "postgresql://localhost/umami";
        let without = config(&[("DATABASE_URL", url)]).unwrap();
        let with = config(&[("DATABASE_URL", url), ("APP_SECRET", "s3cret")]).unwrap();

        assert_eq!(without.secret, crypto::hash(&[url]));
        assert_eq!(with.secret, crypto::hash(&["s3cret"]));
    }

    #[test]
    fn flags_are_truthy_when_set_to_anything() {
        let config = config(&[
            ("DATABASE_URL", "postgresql://localhost/umami"),
            ("SKIP_DB_CHECK", "1"),
            ("SKIP_DB_MIGRATION", "false"),
        ])
        .unwrap();

        assert!(config.skip_db_check);
        assert!(config.skip_db_migration);
    }

    #[test]
    fn direct_url_is_used_for_migrations() {
        let config = config(&[
            ("DATABASE_URL", "postgresql://pooler/umami"),
            ("DIRECT_DATABASE_URL", "postgresql://primary/umami"),
        ])
        .unwrap();

        assert_eq!(
            config.migration_database_url(),
            "postgresql://primary/umami"
        );
    }

    #[test]
    fn rejects_invalid_port() {
        assert!(matches!(
            config(&[("DATABASE_URL", "x"), ("PORT", "http")]),
            Err(ConfigError::Invalid { name: "PORT", .. })
        ));
    }

    #[test]
    fn collect_settings() {
        let config = config(&[
            ("DATABASE_URL", "postgresql://localhost/umami"),
            ("IGNORE_IP", "203.0.113.7, 10.0.0.0/8,bad/cidr"),
            ("CLIENT_IP_HEADER", "X-Client-IP"),
            ("SALT_ROTATION", "week"),
            ("COLLECT_API_ENDPOINT", "collect"),
        ])
        .unwrap();
        let collect = &config.collect;

        assert_eq!(collect.ignore_ips.len(), 3);
        assert_eq!(collect.ignore_ips[0].entry, "203.0.113.7");
        assert!(collect.ignore_ips[0].range.is_none());
        assert_eq!(
            collect.ignore_ips[1].range.unwrap().to_string(),
            "10.0.0.0/8"
        );
        assert!(collect.ignore_ips[2].range.is_none());
        assert_eq!(collect.client_ip_header.as_deref(), Some("x-client-ip"));
        assert_eq!(collect.salt_rotation, SaltRotation::Week);
        assert_eq!(collect.endpoint_alias.as_deref(), Some("/collect"));
        assert_eq!(
            collect.geo_database,
            PathBuf::from("geo/GeoLite2-City.mmdb")
        );
        assert!(!collect.geo_database_explicit);
        assert_eq!(config.cors_max_age, "86400");
    }

    #[test]
    fn two_factor_key_must_be_256_bits_of_hex() {
        let key = |value: &str| {
            config(&[("DATABASE_URL", "x"), ("TWO_FACTOR_ENCRYPTION_KEY", value)])
                .unwrap()
                .two_factor_key
        };

        assert_eq!(key(&"ab".repeat(32)), Some([0xab; 32]));
        assert_eq!(key(&"ab".repeat(31)), None);
        assert_eq!(key(&"zz".repeat(32)), None);
    }

    #[test]
    fn tracker_script_aliases() {
        let config = config(&[
            ("DATABASE_URL", "x"),
            ("TRACKER_SCRIPT_NAME", "umami.js, /js/t.js,umami.js,"),
        ])
        .unwrap();

        assert_eq!(config.tracker_script_aliases, ["/umami.js", "/js/t.js"]);

        for bad in ["script.js", "api/send", "a b.js", "{x}.js", "/"] {
            assert!(
                matches!(
                    super::parse_script_aliases(bad),
                    Err(ConfigError::Invalid {
                        name: "TRACKER_SCRIPT_NAME",
                        ..
                    })
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn normalizes_base_path() {
        for (raw, expected) in [
            ("", ""),
            ("/", ""),
            ("analytics", "/analytics"),
            ("/analytics/", "/analytics"),
            ("/a/b", "/a/b"),
        ] {
            assert_eq!(normalize_base_path(raw), expected, "{raw:?}");
        }
    }

    #[test]
    fn debug_redacts_secrets() {
        let config = config(&[
            ("DATABASE_URL", "postgresql://user:hunter2@localhost/umami"),
            ("APP_SECRET", "s3cret"),
        ])
        .unwrap();
        let debug = format!("{config:?}");

        assert!(!debug.contains("hunter2"));
        assert!(!debug.contains(&config.secret));
    }
}
