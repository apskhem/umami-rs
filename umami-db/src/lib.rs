//! Postgres access for umami-rs.
//!
//! The schema is owned by upstream Umami: `migrations/` is a verbatim copy of
//! upstream's Prisma migrations, applied through a migrator that shares Prisma's
//! `_prisma_migrations` bookkeeping. A database can therefore be migrated by either
//! this server or upstream Umami, and both see it as up to date.

mod check;
mod connect;
pub mod migrate;

pub use check::{
  CheckError,
  check_server_version,
};
pub use connect::{
  connect,
  connect_options,
};

/// Upstream Umami release whose schema the vendored migrations reproduce.
pub const UMAMI_VERSION: &str = "3.4.1";

#[cfg(test)]
mod tests {
  /// The pinned upstream version in the workspace manifest
  /// (`[workspace.metadata.umami]`) and `UMAMI_VERSION` must agree.
  #[test]
  fn version_matches_workspace_metadata() {
    let manifest = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../Cargo.toml"));
    let version = manifest
      .split("[workspace.metadata.umami]")
      .nth(1)
      .and_then(|section| {
        section
          .lines()
          .take_while(|line| !line.starts_with('['))
          .find_map(|line| line.trim().strip_prefix("version = "))
      })
      .map(|value| value.trim_matches('"'))
      .expect("[workspace.metadata.umami] has a version");

    assert_eq!(version, super::UMAMI_VERSION);
  }
}
