//! Website analytics: the queries behind the dashboard's charts and tables.
//!
//! Ported from upstream's Postgres queries (`src/queries/sql`), with its filter
//! language (`src/lib/prisma.ts`, `src/lib/params.ts`). Lists break ties in a
//! fixed order, which upstream leaves to the database.

pub mod charts;
pub mod data;
pub mod date;
pub mod events;
pub mod filters;
pub mod queries;
pub mod realtime;
pub mod sessions;
pub mod sql;
