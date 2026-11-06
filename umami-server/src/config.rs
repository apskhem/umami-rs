use std::fmt;

use crate::crypto;

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

        Ok(Self {
            direct_database_url: get("DIRECT_DATABASE_URL"),
            secret,
            host: get("HOST").unwrap_or_else(|| "0.0.0.0".to_owned()),
            port,
            base_path: normalize_base_path(get("BASE_PATH").as_deref().unwrap_or("")),
            skip_db_check: get("SKIP_DB_CHECK").is_some(),
            skip_db_migration: get("SKIP_DB_MIGRATION").is_some(),
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
            .finish()
    }
}

fn normalize_base_path(raw: &str) -> String {
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
