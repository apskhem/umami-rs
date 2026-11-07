//! Two-factor authentication: ports of upstream's `src/lib/two-factor` (secret
//! encryption, backup codes, and checking codes against the lockout and replay
//! rules). The stored state is in [`crate::queries::two_factor`].

use aes_gcm::aead::{
  Aead,
  KeyInit,
};
use aes_gcm::{
  Aes256Gcm,
  Nonce,
};
use axum::http::StatusCode;
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::totp;
use crate::error::{
  AppError,
  AppResult,
};
use crate::js;
use crate::queries::two_factor;

const BACKUP_CODE_COUNT: usize = 10;
const BACKUP_CODE_ROUNDS: u32 = 10;

/// The 503 upstream returns while `TWO_FACTOR_ENCRYPTION_KEY` is unset or invalid.
pub fn not_configured() -> AppError {
  AppError::coded(
    StatusCode::SERVICE_UNAVAILABLE,
    "two-factor-error-not-configured",
    "TWO_FACTOR_ENCRYPTION_KEY is missing or invalid",
  )
}

/// Upstream's `encryptSecret`: AES-256-GCM, stored as `ciphertext:iv:tag` in hex.
pub fn encrypt_secret(key: &[u8; 32], plaintext: &str) -> String {
  let mut iv = [0u8; 12];
  getrandom::fill(&mut iv).expect("system randomness");

  let sealed = Aes256Gcm::new(key.into())
    .encrypt(&Nonce::from(iv), plaintext.as_bytes())
    .expect("AES-GCM encryption cannot fail");
  let (ciphertext, tag) = sealed.split_at(sealed.len() - 16);

  format!("{}:{}:{}", hex::encode(ciphertext), hex::encode(iv), hex::encode(tag))
}

/// Upstream's `decryptSecret`.
pub fn decrypt_secret(key: &[u8; 32], stored: &str) -> Option<String> {
  let mut parts = stored.split(':');
  let (ciphertext, iv, tag) = (parts.next()?, parts.next()?, parts.next()?);
  let iv: [u8; 12] = hex::decode(iv).ok()?.try_into().ok()?;
  let sealed = [hex::decode(ciphertext).ok()?, hex::decode(tag).ok()?].concat();

  let plaintext = Aes256Gcm::new(key.into())
    .decrypt(&Nonce::from(iv), sealed.as_slice())
    .ok()?;

  String::from_utf8(plaintext).ok()
}

/// Upstream's `generateBackupCodes`: ten `XXXXXXXXXXXXXXXX-XXXXXXXXXXXXXXXX` codes
/// (hex), returned in plaintext and bcrypt-hashed.
pub async fn generate_backup_codes() -> (Vec<String>, Vec<String>) {
  let plaintext: Vec<String> = (0..BACKUP_CODE_COUNT)
    .map(|_| {
      let mut bytes = [0u8; 16];
      getrandom::fill(&mut bytes).expect("system randomness");
      format!("{}-{}", hex::encode_upper(&bytes[..8]), hex::encode_upper(&bytes[8..]))
    })
    .collect();

  let codes = plaintext.clone();
  let hashed = tokio::task::spawn_blocking(move || {
    codes
      .iter()
      .map(|code| bcrypt::hash(code, BACKUP_CODE_ROUNDS).expect("bcrypt hashing"))
      .collect()
  })
  .await
  .expect("bcrypt task");

  (plaintext, hashed)
}

/// Upstream's `verifyBackupCode`: the index of the hash matching the code
/// (compared uppercased), if any.
pub async fn match_backup_code(code: &str, hashes: Vec<String>) -> Option<usize> {
  let code = code.to_uppercase();

  tokio::task::spawn_blocking(move || {
    hashes
      .iter()
      .position(|hash| bcrypt::verify(&code, hash).unwrap_or(false))
  })
  .await
  .ok()
  .flatten()
}

/// Upstream's `checkRateLimit`: an error while the user is locked out.
pub async fn check_rate_limit(db: &PgPool, user_id: Uuid) -> AppResult<()> {
  match two_factor::locked_until(db, user_id).await? {
    Some(until) if until > Utc::now() => {
      Err(AppError::Custom {
        status: StatusCode::TOO_MANY_REQUESTS,
        body: json!({
            "error": {
                "code": "two-factor-error-too-many-attempts",
                "message": "Too many failed attempts",
                "lockedUntil": js::date(until),
            }
        }),
      })
    }
    _ => Ok(()),
  }
}

/// A 400 for a wrong code, with the lockout end if this failure caused one.
async fn invalid(db: &PgPool, user_id: Uuid, code: &'static str, message: &str) -> AppError {
  let locked_until = match two_factor::record_failure(db, user_id).await {
    Ok(locked_until) => locked_until,
    Err(error) => return error.into(),
  };

  let mut error = json!({ "message": message, "code": code, "status": 400 });
  if let Some(until) = locked_until {
    error["lockedUntil"] = json!(js::date(until));
  }

  AppError::Custom {
    status: StatusCode::BAD_REQUEST,
    body: json!({ "error": error }),
  }
}

/// Upstream's TOTP checks, in its order: replay, then the code itself (a wrong
/// code counts towards the lockout). Callers check the rate limit first, and burn
/// the code and reset the rate limit on success.
pub async fn verify_code(
  db: &PgPool,
  key: &[u8; 32],
  user_id: Uuid,
  encrypted_secret: &str,
  token: &str,
) -> AppResult<()> {
  if two_factor::is_replayed(db, user_id, token).await? {
    return Err(AppError::coded(
      StatusCode::BAD_REQUEST,
      "two-factor-error-code-used",
      "Code already used",
    ));
  }

  let secret = decrypt_secret(key, encrypted_secret)
    .ok_or_else(|| anyhow::anyhow!("cannot decrypt the TOTP secret for user {user_id}"))?;

  match totp::verify(token, &secret, Utc::now().timestamp()) {
    Some(true) => Ok(()),
    _ => {
      Err(
        invalid(
          db,
          user_id,
          "two-factor-error-invalid-code",
          "Invalid verification code",
        )
        .await,
      )
    }
  }
}

/// Checks a backup code and consumes it (counting a wrong code towards the
/// lockout), as upstream's `/api/2fa/verify`.
pub async fn verify_backup_code(db: &PgPool, user_id: Uuid, code: &str) -> AppResult<()> {
  let codes = two_factor::unused_backup_codes(db, user_id).await?;
  let hashes = codes.iter().map(|c| c.code_hash.clone()).collect();
  let consumed = match match_backup_code(code, hashes).await {
    Some(index) => two_factor::consume_backup_code(db, &codes[index].id).await?,
    None => false,
  };

  if consumed {
    Ok(())
  } else {
    Err(
      invalid(
        db,
        user_id,
        "two-factor-error-invalid-backup-code",
        "Invalid backup code",
      )
      .await,
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const KEY: [u8; 32] = [7; 32];

  #[test]
  fn secrets_round_trip_in_upstream_format() {
    let stored = encrypt_secret(&KEY, "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP");
    let parts: Vec<&str> = stored.split(':').collect();

    assert_eq!(parts.len(), 3);
    assert_eq!(parts[1].len(), 24, "12-byte IV");
    assert_eq!(parts[2].len(), 32, "16-byte tag");
    assert_eq!(
      decrypt_secret(&KEY, &stored).as_deref(),
      Some("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP")
    );
    assert_eq!(decrypt_secret(&[8; 32], &stored), None);
    assert_eq!(decrypt_secret(&KEY, "garbage"), None);
  }

  #[tokio::test]
  async fn backup_codes_verify_case_insensitively() {
    let (plaintext, hashed) = generate_backup_codes().await;

    assert_eq!(plaintext.len(), 10);
    assert!(plaintext.iter().all(|c| c.len() == 33 && c.as_bytes()[16] == b'-'));
    assert_eq!(
      match_backup_code(&plaintext[3].to_lowercase(), hashed.clone()).await,
      Some(3)
    );
    assert_eq!(match_backup_code("nope", hashed).await, None);
  }
}
