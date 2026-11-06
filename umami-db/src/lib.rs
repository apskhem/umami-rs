//! Postgres access for umami-rs.
//!
//! The schema is owned by upstream Umami: `migrations/` is a verbatim copy of
//! upstream's Prisma migrations, applied through a migrator that shares Prisma's
//! `_prisma_migrations` bookkeeping. A database can therefore be migrated by either
//! this server or upstream Umami, and both see it as up to date.

mod check;
mod connect;
pub mod migrate;

pub use check::{CheckError, check_server_version};
pub use connect::{connect, connect_options};

/// Upstream Umami release whose schema the vendored migrations reproduce.
pub const UMAMI_VERSION: &str = "3.4.1";
