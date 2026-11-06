use sqlx::PgConnection;

/// Oldest Postgres release upstream supports (`server_version_num` form of 9.4.0).
const MIN_SERVER_VERSION_NUM: i32 = 90400;

#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("unable to determine database version (server_version_num = {0:?})")]
    UnknownVersion(String),
    #[error("database version {0} is not compatible; please upgrade to 9.4.0 or greater")]
    Unsupported(String),
}

/// Fails unless the server meets upstream's minimum Postgres version.
/// Returns the server's version string on success.
pub async fn check_server_version(conn: &mut PgConnection) -> Result<String, CheckError> {
    let (version, version_num): (String, String) = sqlx::query_as(
        "select current_setting('server_version'), current_setting('server_version_num')",
    )
    .fetch_one(conn)
    .await?;

    let num: i32 = version_num
        .trim()
        .parse()
        .map_err(|_| CheckError::UnknownVersion(version_num.clone()))?;

    if num < MIN_SERVER_VERSION_NUM {
        return Err(CheckError::Unsupported(version));
    }

    Ok(version)
}
