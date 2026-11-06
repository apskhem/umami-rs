# umami-rs server

A headless Rust backend that is a drop-in replacement for [Umami](https://umami.is)
**v3.4.1** on PostgreSQL: same database schema, same JSON API, so existing Umami
databases, the official tracker script and the official dashboard work against it.

## Layout

| Crate | Purpose |
|---|---|
| `umami-db` | Connection handling and the migrator. `migrations/` is a verbatim copy of upstream's Prisma migrations, applied with Prisma's `_prisma_migrations` bookkeeping so upstream and umami-rs can migrate the same database interchangeably. |
| `umami-server` | The axum HTTP server (`umami-server` binary). |

## Running

```sh
docker compose up -d          # local Postgres
cp .env.example .env
cargo run -p umami-server
curl localhost:3000/api/heartbeat   # {"ok":true}
```

On startup the server checks the Postgres version and applies pending migrations, as
upstream's `check-db` script does. A fresh database gets upstream's default login,
`admin` / `umami`.

## Configuration

Environment variables follow upstream where an equivalent exists; see `.env.example`.
`HOST` replaces upstream's `HOSTNAME`, which Docker sets to the container ID.

## Tests

```sh
cargo test                                   # unit + HTTP tests, no database needed
TEST_DATABASE_URL=postgresql://umami:umami@localhost:5432/umami \
  cargo test -- --include-ignored            # also run migrations against Postgres
```
