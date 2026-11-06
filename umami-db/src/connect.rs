use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

/// Builds connection options from a `DATABASE_URL`, accepting the same URLs as upstream.
///
/// Upstream reads the Prisma-style `schema` query parameter and pins `search_path`
/// to it. Postgres doesn't know that parameter, so it is removed from the URL and
/// applied as a startup `search_path` option instead.
pub fn connect_options(database_url: &str) -> Result<PgConnectOptions, sqlx::Error> {
    let mut url =
        url::Url::parse(database_url).map_err(|e| sqlx::Error::Configuration(Box::new(e)))?;

    let schema = url
        .query_pairs()
        .find(|(key, _)| key == "schema")
        .map(|(_, value)| value.into_owned());

    if schema.is_some() {
        let rest: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key != "schema")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();

        if rest.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut().clear().extend_pairs(rest);
        }
    }

    let options = PgConnectOptions::from_str(url.as_str())?;

    Ok(match schema.filter(|s| !s.is_empty()) {
        // Quoted like upstream's `SET search_path TO "<schema>"`.
        Some(schema) => options.options([(
            "search_path",
            format!("\"{}\"", schema.replace('"', "\"\"")),
        )]),
        None => options,
    })
}

/// Opens a connection pool for `database_url`.
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .connect_with(connect_options(database_url)?)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_url_has_no_search_path() {
        let options = connect_options("postgresql://u:p@localhost:5432/umami").unwrap();

        assert_eq!(options.get_database(), Some("umami"));
        assert_eq!(options.get_options(), None);
    }

    #[test]
    fn schema_param_becomes_search_path() {
        let options =
            connect_options("postgresql://u:p@localhost:5432/umami?schema=analytics").unwrap();

        assert_eq!(options.get_database(), Some("umami"));
        let startup = options.get_options().unwrap();
        assert!(startup.contains("search_path"), "{startup}");
        assert!(startup.contains("analytics"), "{startup}");
    }

    #[test]
    fn schema_param_is_removed_and_other_params_kept() {
        let options = connect_options(
            "postgresql://u:p@localhost:5432/umami?sslmode=disable&schema=analytics",
        )
        .unwrap();

        assert!(options.get_options().unwrap().contains("analytics"));
    }

    #[test]
    fn invalid_url_is_a_configuration_error() {
        assert!(matches!(
            connect_options("not a url"),
            Err(sqlx::Error::Configuration(_))
        ));
    }
}
