//! Password hashing, compatible with upstream's bcryptjs hashes.

/// Checks `password` against a bcrypt hash, off the async runtime (bcrypt is
/// deliberately slow). A malformed hash never matches.
pub async fn verify(password: &str, hash: &str) -> bool {
  let (password, hash) = (password.to_owned(), hash.to_owned());

  tokio::task::spawn_blocking(move || bcrypt::verify(password, &hash).unwrap_or(false))
    .await
    .unwrap_or(false)
}

/// Hashes a new password as upstream does (bcrypt, cost 10).
pub async fn hash(password: &str) -> String {
  let password = password.to_owned();

  tokio::task::spawn_blocking(move || bcrypt::hash(password, 10).expect("bcrypt hashing"))
    .await
    .expect("bcrypt task")
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Upstream's seeded `admin` user (password `umami`), hashed by bcryptjs.
  const UMAMI: &str = "$2b$10$BUli0c.muyCW1ErNJc3jL.vFRFtFJWrT8/GcR4A.sUdCznaXiqFXa";

  #[tokio::test]
  async fn verifies_bcryptjs_hashes() {
    assert!(verify("umami", UMAMI).await);
    assert!(!verify("Umami", UMAMI).await);
    assert!(!verify("umami", "not a hash").await);
  }

  #[tokio::test]
  async fn hashes_in_bcryptjs_format() {
    let hashed = hash("correct horse").await;

    assert!(hashed.starts_with("$2b$10$"), "{hashed}");
    assert!(verify("correct horse", &hashed).await);
  }
}
