//! Differential testing against upstream Umami.
//!
//! `compose.yaml` runs upstream and umami-rs side by side on separate databases.
//! Tests send both the same requests, then compare the responses and the rows each
//! wrote. Values that are random by design (event IDs, token issue times) are
//! excluded; everything else must match.

use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use umami_server::collect::CacheToken;
use umami_server::crypto;
use uuid::Uuid;

/// The `APP_SECRET` both servers run with (see `compose.yaml`).
pub const APP_SECRET: &str = "parity-secret";

/// A server under test and its database.
pub struct Target {
    pub name: &'static str,
    pub url: String,
    pub db: PgPool,
}

pub struct Stack {
    pub upstream: Target,
    pub rs: Target,
    http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Value,
}

/// The tables `/api/send` writes, with the columns that are random by design.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    Session,
    WebsiteEvent,
    EventData,
    SessionData,
    SessionLink,
    Revenue,
}

impl Table {
    pub const ALL: [Table; 6] = [
        Table::Session,
        Table::WebsiteEvent,
        Table::EventData,
        Table::SessionData,
        Table::SessionLink,
        Table::Revenue,
    ];

    fn query(self) -> (&'static str, &'static [&'static str], &'static str) {
        match self {
            Table::Session => ("session", &[], "session_id"),
            Table::WebsiteEvent => (
                "website_event",
                &["event_id"],
                "created_at, url_path, event_type, coalesce(event_name, ''), visit_id",
            ),
            Table::EventData => (
                "event_data",
                &["event_data_id", "website_event_id"],
                "created_at, data_key, string_value",
            ),
            Table::SessionData => ("session_data", &["session_data_id"], "session_id, data_key"),
            Table::SessionLink => ("session_link", &[], "session_id, distinct_id"),
            Table::Revenue => (
                "revenue",
                &["revenue_id", "event_id"],
                "created_at, event_name",
            ),
        }
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

impl Target {
    async fn connect(name: &'static str, url: String, database_url: &str) -> Self {
        let db = PgPool::connect(database_url)
            .await
            .unwrap_or_else(|e| panic!("{name}: cannot connect to {database_url}: {e}"));

        Self { name, url, db }
    }

    /// Rows of `table` for `website`, as JSON objects without the random columns.
    pub async fn rows(&self, table: Table, website: Uuid) -> Vec<Value> {
        let (name, random, order) = table.query();
        let sql = format!(
            "select coalesce(jsonb_agg(to_jsonb(t) - $2::text[] order by {order}), '[]') \
             from {name} t where website_id = $1"
        );
        let random: Vec<&str> = random.to_vec();

        let rows: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(website)
            .bind(random)
            .fetch_one(&self.db)
            .await
            .unwrap();

        rows.as_array().cloned().unwrap_or_default()
    }
}

/// Waits until `url` answers `/api/heartbeat`.
pub async fn wait_for(http: &reqwest::Client, name: &str, url: &str) {
    for _ in 0..120 {
        if let Ok(response) = http.get(format!("{url}/api/heartbeat")).send().await
            && response.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    panic!(
        "{name} at {url} is not up. Start the stack with \
         `docker compose -f parity/compose.yaml up -d --build --wait` \
         and check `docker compose -f parity/compose.yaml logs {name}`"
    );
}

impl Stack {
    /// Connects to the stack from `compose.yaml` (URLs overridable via `PARITY_*`).
    pub async fn connect() -> Self {
        let http = reqwest::Client::new();
        let postgres = env_or("PARITY_POSTGRES", "postgresql://umami:umami@localhost:5433");
        let upstream_url = env_or("PARITY_UPSTREAM_URL", "http://localhost:3001");
        let rs_url = env_or("PARITY_RS_URL", "http://localhost:3002");

        wait_for(&http, "upstream", &upstream_url).await;
        wait_for(&http, "rs", &rs_url).await;

        Self {
            upstream: Target::connect("upstream", upstream_url, &format!("{postgres}/upstream"))
                .await,
            rs: Target::connect("rs", rs_url, &format!("{postgres}/rs")).await,
            http,
        }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Creates the same website in both databases.
    pub async fn website(&self) -> Uuid {
        let id = Uuid::new_v4();

        for target in [&self.upstream, &self.rs] {
            sqlx::query("insert into website (website_id, name, domain) values ($1, 'Parity', 'example.com')")
                .bind(id)
                .execute(&target.db)
                .await
                .unwrap();
        }

        id
    }

    /// POSTs `body` to `path` on one server.
    pub async fn post(
        &self,
        target: &Target,
        path: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Response {
        let mut request = self
            .http
            .post(format!("{}{path}", target.url))
            .header("content-type", "application/json")
            .body(body.to_string());

        for (name, value) in headers {
            request = request.header(*name, *value);
        }

        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));

        Response { status, body }
    }

    /// Sends the same request to both servers (upstream first, then umami-rs) and
    /// asserts the responses are equivalent. Returns upstream's response.
    pub async fn send(&self, path: &str, headers: &[(&str, &str)], body: &Value) -> Response {
        let upstream = self.post(&self.upstream, path, headers, body).await;
        let rs = self.post(&self.rs, path, headers, body).await;

        assert_same_response(&upstream, &rs, body);

        upstream
    }

    /// Asserts both databases hold the same rows for `website`.
    pub async fn assert_same_rows(&self, website: Uuid) {
        self.assert_same_rows_except(website, &[]).await;
    }

    /// [`Stack::assert_same_rows`] for all tables but `skip`.
    pub async fn assert_same_rows_except(&self, website: Uuid, skip: &[Table]) {
        let mut problems = Vec::new();

        for table in Table::ALL.into_iter().filter(|table| !skip.contains(table)) {
            let upstream = self.upstream.rows(table, website).await;
            let rs = self.rs.rows(table, website).await;

            if upstream != rs {
                problems.push(diff_rows(table, &upstream, &rs));
            }
        }

        assert!(
            problems.is_empty(),
            "rows differ for website {website}:\n{}",
            problems.join("\n")
        );
    }
}

/// Claims of a cache token, minus its issue time (which depends on when each server
/// handled the request).
pub fn token_claims(token: &str) -> Option<CacheToken> {
    let secret = crypto::hash(&[APP_SECRET]);

    CacheToken::parse(token, &secret).map(|claims| CacheToken {
        iat: None,
        ..claims
    })
}

fn assert_same_response(upstream: &Response, rs: &Response, request: &Value) {
    let context = || format!("request: {request}\nupstream: {upstream:?}\nrs: {rs:?}");

    assert_eq!(upstream.status, rs.status, "status differs\n{}", context());

    if let Some(token) = upstream.body.get("cache").and_then(Value::as_str) {
        assert_eq!(
            upstream.body["sessionId"],
            rs.body["sessionId"],
            "sessionId differs\n{}",
            context()
        );
        assert_eq!(
            upstream.body["visitId"],
            rs.body["visitId"],
            "visitId differs\n{}",
            context()
        );

        let rs_token = rs
            .body
            .get("cache")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(
            token_claims(token),
            token_claims(rs_token),
            "cache token differs\n{}",
            context()
        );
    } else if let Some(error) = upstream.body.get("error").filter(|e| e.is_object()) {
        // Upstream adds zod's error tree to 400s; the envelope must match.
        for field in ["message", "code", "status"] {
            assert_eq!(
                error[field],
                rs.body["error"][field],
                "error.{field} differs\n{}",
                context()
            );
        }
    } else {
        assert_eq!(upstream.body, rs.body, "body differs\n{}", context());
    }
}

fn diff_rows(table: Table, upstream: &[Value], rs: &[Value]) -> String {
    let mut out = format!(
        "  {table:?}: upstream has {} rows, rs has {}",
        upstream.len(),
        rs.len()
    );

    for (index, pair) in upstream.iter().zip(rs).enumerate() {
        let (Some(u), Some(r)) = (pair.0.as_object(), pair.1.as_object()) else {
            continue;
        };

        for (key, value) in u {
            if r.get(key) != Some(value) {
                out.push_str(&format!(
                    "\n    row {index} {key}: upstream {value} vs rs {}",
                    r.get(key).unwrap_or(&Value::Null)
                ));
            }
        }
    }

    for (label, extra) in [
        ("upstream", upstream.get(rs.len()..)),
        ("rs", rs.get(upstream.len()..)),
    ] {
        for row in extra.unwrap_or_default() {
            out.push_str(&format!("\n    only in {label}: {row}"));
        }
    }

    out
}
