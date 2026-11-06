//! Two-factor authentication state: ports of upstream's `src/lib/two-factor`
//! (secret encryption, backup codes, rate limiting, replay prevention) and the
//! queries its routes run.

use aes_gcm::aead::{
  Aead,
  KeyInit,
};
use aes_gcm::{
  Aes256Gcm,
  Nonce,
};
use axum::http::StatusCode;
use chrono::{
  DateTime,
  Duration,
  Utc,
};
use serde_json::json;
use sqlx::{
  PgConnection,
  PgPool,
};
use uuid::Uuid;

use super::totp;
use crate::error::{
  AppError,
  AppResult,
};
use crate::json as js;

const MAX_ATTEMPTS: i32 = 5;
const LOCKOUT: Duration = Duration::minutes(15);
/// How long a used code stays blocked from reuse.
const REPLAY_WINDOW: Duration = Duration::seconds(90);
const BACKUP_CODE_COUNT: usize = 10;
const BACKUP_CODE_ROUNDS: u32 = 10;

/// The `app_setting` key for requiring 2FA from every user.
pub const GLOBAL_REQUIRED_KEY: &str = "twoFactorRequiredGlobal";

/// The 503 upstream returns while `TWO_FACTOR_ENCRYPTION_KEY` is unset or invalid.
pub fn not_configured() -> AppError {
  AppError::coded(
    StatusCode::SERVICE_UNAVAILABLE,
    "two-factor-error-not-configured",
    "TWO_FACTOR_ENCRYPTION_KEY is missing or invalid",
  )
}

/// A Prisma-style `cuid()` id for the 2FA tables, whose ids Prisma generates
/// client-side (the columns have no default).
fn cuid() -> String {
  const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
  let mut bytes = [0u8; 24];
  getrandom::fill(&mut bytes).expect("system randomness");

  std::iter::once('c')
    .chain(
      bytes
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()])),
    )
    .collect()
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

#[derive(Debug, sqlx::FromRow)]
pub struct TwoFactorAuth {
  pub secret: String,
  pub is_enabled: bool,
}

pub async fn get(db: &PgPool, user_id: Uuid) -> sqlx::Result<Option<TwoFactorAuth>> {
  sqlx::query_as("select secret, is_enabled from two_factor_auth where user_id = $1")
    .bind(user_id)
    .fetch_optional(db)
    .await
}

/// Stores a new, not yet enabled secret for the user.
pub async fn save_pending(db: &PgPool, user_id: Uuid, encrypted_secret: &str) -> sqlx::Result<()> {
  sqlx::query(
    "insert into two_factor_auth (id, user_id, secret, is_enabled, updated_at) \
         values ($1, $2, $3, false, now()) \
         on conflict (user_id) do update set secret = excluded.secret, is_enabled = false, updated_at = now()",
  )
  .bind(cuid())
  .bind(user_id)
  .bind(encrypted_secret)
  .execute(db)
  .await?;

  Ok(())
}

/// Abandons a setup that was never confirmed.
pub async fn delete_pending(db: &PgPool, user_id: Uuid) -> sqlx::Result<()> {
  sqlx::query("delete from two_factor_auth where user_id = $1 and not is_enabled")
    .bind(user_id)
    .execute(db)
    .await?;

  Ok(())
}

/// Confirms setup: enables 2FA, replaces the backup codes and burns the code used.
pub async fn enable(db: &PgPool, user_id: Uuid, backup_hashes: &[String], otp: &str) -> sqlx::Result<()> {
  let mut tx = db.begin().await?;

  sqlx::query("update two_factor_auth set is_enabled = true, updated_at = now() where user_id = $1")
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
  sqlx::query("delete from two_factor_backup_code where user_id = $1")
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
  for hash in backup_hashes {
    sqlx::query(
      "insert into two_factor_backup_code (id, user_id, code_hash, used, created_at) \
             values ($1, $2, $3, false, now())",
    )
    .bind(cuid())
    .bind(user_id)
    .bind(hash)
    .execute(&mut *tx)
    .await?;
  }
  mark_used(&mut tx, user_id, otp).await?;

  tx.commit().await
}

/// Turns 2FA off, burning the code used.
pub async fn disable(db: &PgPool, user_id: Uuid, otp: &str) -> sqlx::Result<()> {
  let mut tx = db.begin().await?;

  mark_used(&mut tx, user_id, otp).await?;
  sqlx::query("delete from two_factor_auth where user_id = $1")
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
  sqlx::query("delete from two_factor_backup_code where user_id = $1")
    .bind(user_id)
    .execute(&mut *tx)
    .await?;

  tx.commit().await
}

/// Counts of the 2FA rows an admin reset removed.
pub struct ResetCounts {
  pub two_factor_auth: u64,
  pub backup_codes: u64,
  pub otp_used: u64,
  pub rate_limit: u64,
}

/// Removes all of a user's 2FA state (an admin reset).
pub async fn reset(db: &PgPool, user_id: Uuid) -> sqlx::Result<ResetCounts> {
  let mut tx = db.begin().await?;
  let mut delete = async |table: &'static str| {
    sqlx::query(sqlx::AssertSqlSafe(format!("delete from {table} where user_id = $1")))
      .bind(user_id)
      .execute(&mut *tx)
      .await
      .map(|result| result.rows_affected())
  };

  let counts = ResetCounts {
    two_factor_auth: delete("two_factor_auth").await?,
    backup_codes: delete("two_factor_backup_code").await?,
    otp_used: delete("two_factor_otp_used").await?,
    rate_limit: delete("two_factor_rate_limit").await?,
  };

  tx.commit().await?;
  Ok(counts)
}

#[derive(Debug, sqlx::FromRow)]
pub struct BackupCode {
  pub id: String,
  pub code_hash: String,
}

pub async fn unused_backup_codes(db: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<BackupCode>> {
  sqlx::query_as("select id, code_hash from two_factor_backup_code where user_id = $1 and not used")
    .bind(user_id)
    .fetch_all(db)
    .await
}

/// Marks a backup code used; `false` if it was used concurrently.
pub async fn consume_backup_code(db: &PgPool, id: &str) -> sqlx::Result<bool> {
  let result = sqlx::query("update two_factor_backup_code set used = true where id = $1 and not used")
    .bind(id)
    .execute(db)
    .await?;

  Ok(result.rows_affected() > 0)
}

/// Upstream's `checkRateLimit`: an error while the user is locked out.
pub async fn check_rate_limit(db: &PgPool, user_id: Uuid) -> AppResult<()> {
  let locked_until: Option<Option<DateTime<Utc>>> =
    sqlx::query_scalar("select locked_until from two_factor_rate_limit where user_id = $1")
      .bind(user_id)
      .fetch_optional(db)
      .await?;

  match locked_until.flatten() {
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

/// Upstream's `recordFailedAttempt`: counts a failure, locking the user out at
/// five. Returns the lockout end if this failure triggered one.
pub async fn record_failure(db: &PgPool, user_id: Uuid) -> sqlx::Result<Option<DateTime<Utc>>> {
  let attempts: i32 = sqlx::query_scalar(
    "insert into two_factor_rate_limit (id, user_id, attempts, updated_at) values ($1, $2, 1, now()) \
         on conflict (user_id) do update set attempts = two_factor_rate_limit.attempts + 1, updated_at = now() \
         returning attempts",
  )
  .bind(cuid())
  .bind(user_id)
  .fetch_one(db)
  .await?;

  if attempts < MAX_ATTEMPTS {
    return Ok(None);
  }

  let locked_until = Utc::now() + LOCKOUT;
  sqlx::query("update two_factor_rate_limit set locked_until = $2, updated_at = now() where user_id = $1")
    .bind(user_id)
    .bind(locked_until)
    .execute(db)
    .await?;

  Ok(Some(locked_until))
}

pub async fn reset_rate_limit(db: &PgPool, user_id: Uuid) -> sqlx::Result<()> {
  sqlx::query("delete from two_factor_rate_limit where user_id = $1")
    .bind(user_id)
    .execute(db)
    .await?;

  Ok(())
}

/// Upstream's `isOtpReplayed`; expired records are cleaned up on the way.
async fn is_replayed(db: &PgPool, user_id: Uuid, otp: &str) -> sqlx::Result<bool> {
  let expires_at: Option<DateTime<Utc>> =
    sqlx::query_scalar("select expires_at from two_factor_otp_used where user_id = $1 and otp = $2")
      .bind(user_id)
      .bind(otp)
      .fetch_optional(db)
      .await?;

  match expires_at {
    Some(expires_at) if expires_at < Utc::now() => {
      sqlx::query("delete from two_factor_otp_used where user_id = $1 and otp = $2")
        .bind(user_id)
        .bind(otp)
        .execute(db)
        .await?;
      Ok(false)
    }
    Some(_) => Ok(true),
    None => Ok(false),
  }
}

/// Upstream's `markOtpUsed`.
pub async fn mark_used(db: &mut PgConnection, user_id: Uuid, otp: &str) -> sqlx::Result<()> {
  sqlx::query(
    "insert into two_factor_otp_used (id, user_id, otp, expires_at) values ($1, $2, $3, $4) \
         on conflict (user_id, otp) do update set expires_at = excluded.expires_at",
  )
  .bind(cuid())
  .bind(user_id)
  .bind(otp)
  .bind(Utc::now() + REPLAY_WINDOW)
  .execute(db)
  .await?;

  Ok(())
}

/// A 400 for a wrong code, with the lockout end if this failure caused one.
async fn invalid(db: &PgPool, user_id: Uuid, code: &'static str, message: &str) -> AppError {
  let locked_until = match record_failure(db, user_id).await {
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
  if is_replayed(db, user_id, token).await? {
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
  let codes = unused_backup_codes(db, user_id).await?;
  let hashes = codes.iter().map(|c| c.code_hash.clone()).collect();
  let consumed = match match_backup_code(code, hashes).await {
    Some(index) => consume_backup_code(db, &codes[index].id).await?,
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

/// Why 2FA is required for a user, if it is (upstream's precedence: global, then
/// the user, then any of their teams).
pub async fn requirement(db: &PgPool, user_id: Uuid) -> sqlx::Result<Option<&'static str>> {
  if global_required(db).await? {
    return Ok(Some("global"));
  }

  let (user, team): (bool, bool) = sqlx::query_as(
    r#"select
             coalesce((select two_factor_required from "user" where user_id = $1), false),
             exists(select 1 from team t join team_user tu on tu.team_id = t.team_id
                    where tu.user_id = $1 and t.two_factor_required)"#,
  )
  .bind(user_id)
  .fetch_one(db)
  .await?;

  Ok(if user {
    Some("user")
  } else if team {
    Some("team")
  } else {
    None
  })
}

pub async fn global_required(db: &PgPool) -> sqlx::Result<bool> {
  let value: Option<String> = sqlx::query_scalar("select value from app_setting where key = $1")
    .bind(GLOBAL_REQUIRED_KEY)
    .fetch_optional(db)
    .await?;

  Ok(value.as_deref() == Some("true"))
}

pub async fn set_global_required(db: &PgPool, required: bool) -> sqlx::Result<()> {
  sqlx::query(
    "insert into app_setting (key, value) values ($1, $2) \
         on conflict (key) do update set value = excluded.value",
  )
  .bind(GLOBAL_REQUIRED_KEY)
  .bind(required.to_string())
  .execute(db)
  .await?;

  Ok(())
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

  #[test]
  fn cuids_look_like_prisma_ids() {
    let id = cuid();
    assert_eq!(id.len(), 25);
    assert!(id.starts_with('c'));
  }
}
