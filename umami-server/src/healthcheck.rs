//! `umami-server healthcheck`: checks a running server's `/api/heartbeat`, for
//! container healthchecks in images without curl.

use std::time::Duration;

use tokio::io::{
  AsyncReadExt,
  AsyncWriteExt,
};
use tokio::net::TcpStream;

use crate::config;

const TIMEOUT: Duration = Duration::from_secs(3);

/// Where the local server listens, from the same `HOST`, `PORT` and `BASE_PATH`
/// variables it reads (without needing the rest of the configuration).
pub fn target_from_env() -> (String, u16, String) {
  let var = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());

  let host = match var("HOST").as_deref() {
    None | Some("0.0.0.0") | Some("::") | Some("[::]") => "127.0.0.1".to_owned(),
    Some(host) => host.to_owned(),
  };
  let port = var("PORT").and_then(|port| port.parse().ok()).unwrap_or(3000);
  let base_path = config::normalize_base_path(var("BASE_PATH").as_deref().unwrap_or(""));

  (host, port, base_path)
}

/// Requests `{base_path}/api/heartbeat` and succeeds on a 200 response.
pub async fn check(host: &str, port: u16, base_path: &str) -> Result<(), String> {
  tokio::time::timeout(TIMEOUT, request(host, port, base_path))
    .await
    .map_err(|_| format!("no response from {host}:{port} within {TIMEOUT:?}"))?
}

async fn request(host: &str, port: u16, base_path: &str) -> Result<(), String> {
  let mut stream = TcpStream::connect((host, port))
    .await
    .map_err(|e| format!("cannot connect to {host}:{port}: {e}"))?;

  let request = format!("GET {base_path}/api/heartbeat HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
  stream
    .write_all(request.as_bytes())
    .await
    .map_err(|e| format!("cannot send request: {e}"))?;

  let mut response = Vec::new();
  stream
    .read_to_end(&mut response)
    .await
    .map_err(|e| format!("cannot read response: {e}"))?;

  let status_line = response.split(|&b| b == b'\n').next().unwrap_or_default();
  let status_line = String::from_utf8_lossy(status_line);

  match status_line.split_whitespace().nth(1) {
    Some("200") => Ok(()),
    _ => Err(format!("unhealthy: {}", status_line.trim())),
  }
}
