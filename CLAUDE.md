# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A headless Rust (axum + sqlx, Postgres) drop-in replacement for the **Umami v3.4.1** backend. The official tracker script, dashboard and existing Umami databases must work against it unchanged. The pinned upstream version lives in `Cargo.toml` under `[workspace.metadata.umami]` and in `umami_db::UMAMI_VERSION` (a test keeps them equal).

**Compatibility boundary:** match upstream's API (routes, request/response JSON shapes and types, status codes, error envelope) and DB schema exactly. Internal logic does *not* need to be bug-for-bug: where upstream 500s, writes partially, orphans rows or returns unstable lists, do the correct thing instead, reuse upstream's error messages where possible, cover the case in the parity tests, and document the difference in `README.md` (each feature section has a "Differences from upstream" list).

## Commands

```sh
docker compose up -d && cp .env.example .env   # local Postgres on :5432
cargo run -p umami-server                      # migrates on startup; default login admin / umami
cargo +nightly fmt                             # rustfmt.toml uses nightly-only options; stable fmt reverts them
cargo clippy --all-targets

cargo test                                     # unit + HTTP tests, no database needed
cargo test -p umami-server --test http <name>  # one integration test file / filter
TEST_DATABASE_URL=postgresql://umami:umami@localhost:5432/umami \
  cargo test -- --include-ignored              # also DB tests (#[ignore = "requires TEST_DATABASE_URL"])
```

Parity tests against real upstream (all `#[ignore]`d; need the parity stack, which uses its own Postgres on :5433 and servers on :3001–3004):

```sh
docker compose -f parity/compose.yaml up -d --build --wait
cargo test -p umami-parity --test analytics -- --ignored <filter>
docker compose -f parity/compose.yaml down -v
```

The `rs` services are built from the repo's `Dockerfile`, so rebuild (`--build`) after server changes before rerunning parity tests.

## Workspace

- `umami-server` — the axum server (lib + `umami-server` binary; `umami-server healthcheck` is the Docker healthcheck).
- `umami-db` — connection options, Postgres version check and the migrator. `migrations/` is a **verbatim copy** of upstream's Prisma migrations, applied with Prisma's `_prisma_migrations` bookkeeping (same checksums, same advisory lock) so either server can migrate a database the other created. Never edit or add migrations here.
- `crate-ua-rules` — standalone UA classification (detect-browser, isbot, ua-parser-js 1.x ports). `src/rules.rs` is `@generated` by `tools/ua-rules` — regenerate, don't hand-edit (see README).
- `parity` (`umami-parity`) — differential tests: `parity/src/lib.rs` provides `Stack` (`call_both`, `call_as`, `assert_same`, `assert_same_rows`, `mask` for random fields) to send identical requests to upstream and umami-rs and compare responses and DB rows; `parity/src/fixtures.rs` seeds identical teams, websites, links, pixels, boards and shares in both databases and mints share tokens.
- `tools/tracker/fetch.py` vendors upstream's `script.js` and `recorder.js` into `umami-server/assets/`; their hashes are pinned as `SCRIPT_SHA256` and `RECORDER_SHA256` in `umami-server/src/tracker.rs`.

## Server architecture

- `lib.rs::app` builds the router (`routes/mod.rs`, whose paths mirror upstream's `src/app/api/**/route.ts`), nests it under `BASE_PATH`, and wraps `/api/*` in `routes::api_headers` (upstream's permissive CORS + no-cache, answering `OPTIONS` directly).
- `AppState` = `PgPool` + `Arc<Config>` + optional GeoLite2 DB. `Config::from_lookup` lets tests build config without env vars; `tests/http.rs` uses a lazily-connecting pool to test routes that never hit the DB.
- `error.rs::AppError` renders upstream's envelope `{"error":{"message","code","status"}}`. Use its constructors (`bad_request`, `coded`, `taken` for unique violations → 400, `team_not_found`, …); `Internal` hides details from the client.
- Request validation ports upstream's zod schemas by hand rather than via serde derive: `zod.rs` (zod's exact `uuid`/number coercion rules), `routes/body.rs` (JSON body fields with zod's optional vs nullable semantics; malformed bodies → 400), `routes/params.rs` (query string, last repeated key wins; paging/search/sorting groups). Malformed path IDs → 404.
- `auth/` implements upstream's `checkAuth` as an extractor: session tokens (encrypted JWT keyed by `APP_SECRET`, bound to the password hash, interoperable with upstream), `umami_` API keys (blocked on credential/admin paths), and share tokens (`x-umami-share-token` + `x-umami-share-context`). Sessions are stateless (no Redis). `permissions.rs` ports upstream's role table and `can*` checks; ownership rules go through `Owner::permits` (owning user, or a team member whose role grants the permission). Routes answer a failed check with `can::require(...)?` (401).
- `queries/` — every Prisma-side query, one module per model (users, teams, websites, shares, segments, boards, API keys, 2FA state), mirroring upstream's `src/queries/prisma`. Routes don't write inline SQL. Models derive `Serialize` in schema field order (dates via `js::serialize_date`) and `to_json()` returns the Prisma JSON. Upstream uses Prisma `relationMode = "prisma"` (no FKs except `api_key`), so deletes here cascade manually inside transactions. `queries::now()` truncates to ms like Prisma; lists always add an ID tiebreaker. New record IDs come from `AppState::new_id()`.
- `analytics/` — dashboard queries ported from upstream's `src/queries/sql`. `analytics/sql.rs` renders upstream-style `{{name}}` / `{{name::type}}` placeholders to positional params and returns rows as JSON using **Prisma's types** (bigint → number, numeric → string); `filters.rs` implements upstream's filter language (`browser=neq.chrome`, `epf0`/`spf0`, segments, cohorts).
- `collect/` — `/api/send` and `/api/batch` ingestion: payload parsing, client IP/geo (CDN headers or MaxMind), UA via `crate-ua-rules`, session/visit IDs, cache tokens.
- `js.rs` reproduces JavaScript semantics (UTF-16 `substring`, `toFixed`, `Number()` coercion, truthiness, `Date#toJSON` via `js::date`) so stored and returned values are byte-identical to upstream's.

All SQL uses runtime `sqlx::query*` functions (no compile-time `query!` macros), so building needs no database.

## Conventions

- Doc comments cite the upstream file or function being ported (e.g. "Upstream's `sanitizeSortFilters`"); keep doing this, since it is how behaviour is traced back to upstream.
- A new endpoint or behaviour normally needs: the route, unit/HTTP tests, a parity test comparing both servers, and a README entry (plus a "Differences from upstream" bullet if it intentionally diverges).
- Environment variables follow upstream's names (see `.env.example`); `HOST` replaces upstream's `HOSTNAME`.
