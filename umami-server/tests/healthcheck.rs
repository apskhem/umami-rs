use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use umami_server::config::Config;
use umami_server::state::AppState;
use umami_server::{
  app,
  healthcheck,
};

/// Serves the app on an ephemeral port (with a database pool that never connects).
async fn serve(base_path: &str) -> u16 {
  let config = Config::from_lookup(|key| {
    match key {
      "DATABASE_URL" => Some("postgresql://umami@127.0.0.1:1/umami".to_owned()),
      "BASE_PATH" => Some(base_path.to_owned()),
      _ => None,
    }
  })
  .unwrap();
  let state = AppState {
    db: PgPoolOptions::new().connect_lazy_with(umami_db::connect_options(&config.database_url).unwrap()),
    config: Arc::new(config),
    geo: None,
  };

  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let port = listener.local_addr().unwrap().port();
  tokio::spawn(async move { axum::serve(listener, app(state)).await.unwrap() });

  port
}

#[tokio::test]
async fn passes_against_a_running_server() {
  let port = serve("").await;

  assert_eq!(healthcheck::check("127.0.0.1", port, "").await, Ok(()));
}

#[tokio::test]
async fn follows_base_path() {
  let port = serve("/analytics").await;

  assert_eq!(healthcheck::check("127.0.0.1", port, "/analytics").await, Ok(()));
  assert_eq!(
    healthcheck::check("127.0.0.1", port, "").await,
    Err("unhealthy: HTTP/1.1 404 Not Found".to_owned())
  );
}

#[tokio::test]
async fn fails_when_nothing_listens() {
  let port = TcpListener::bind("127.0.0.1:0")
    .await
    .unwrap()
    .local_addr()
    .unwrap()
    .port();

  let error = healthcheck::check("127.0.0.1", port, "").await.unwrap_err();
  assert!(error.starts_with("cannot connect"), "{error}");
}
