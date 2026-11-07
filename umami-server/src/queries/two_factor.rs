//! Stored two-factor state: secrets, backup codes, the failed-attempt lockout,
//! used codes and the 2FA requirements (the queries behind upstream's
//! `src/lib/two-factor` and its routes).

use chrono::{
  DateTime,
  Duration,
  Utc,
};
use sqlx::{
  PgConnection,
  PgPool,
};
use uuid::Uuid;

const MAX_ATTEMPTS: i32 = 5;
const LOCKOUT: Duration = Duration::minutes(15);
/// How long a used code stays blocked from reuse.
const REPLAY_WINDOW: Duration = Duration::seconds(90);

/// The `app_setting` key for requiring 2FA from every user.
pub const GLOBAL_REQUIRED_KEY: &str = "twoFactorRequiredGlobal";

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

/// Whether the user has completed 2FA setup.
pub async fn enabled(db: &PgPool, user_id: Uuid) -> sqlx::Result<bool> {
  sqlx::query_scalar("select exists(select 1 from two_factor_auth where user_id = $1 and is_enabled)")
    .bind(user_id)
    .fetch_one(db)
    .await
}

/// When the user's lockout ends, if they have one (expired or not).
pub async fn locked_until(db: &PgPool, user_id: Uuid) -> sqlx::Result<Option<DateTime<Utc>>> {
  let locked_until: Option<Option<DateTime<Utc>>> =
    sqlx::query_scalar("select locked_until from two_factor_rate_limit where user_id = $1")
      .bind(user_id)
      .fetch_optional(db)
      .await?;

  Ok(locked_until.flatten())
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
pub async fn is_replayed(db: &PgPool, user_id: Uuid, otp: &str) -> sqlx::Result<bool> {
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

/// Sets whether the user must use 2FA; `false` if the user does not exist.
pub async fn set_user_required(db: &PgPool, user_id: Uuid, required: bool) -> sqlx::Result<bool> {
  let updated = sqlx::query(r#"update "user" set two_factor_required = $2, updated_at = now() where user_id = $1"#)
    .bind(user_id)
    .bind(required)
    .execute(db)
    .await?;

  Ok(updated.rows_affected() > 0)
}

/// Sets whether the team's members must use 2FA; `false` if the team does not
/// exist.
pub async fn set_team_required(db: &PgPool, team_id: Uuid, required: bool) -> sqlx::Result<bool> {
  let updated = sqlx::query("update team set two_factor_required = $2, updated_at = now() where team_id = $1")
    .bind(team_id)
    .bind(required)
    .execute(db)
    .await?;

  Ok(updated.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cuids_look_like_prisma_ids() {
    let id = cuid();
    assert_eq!(id.len(), 25);
    assert!(id.starts_with('c'));
  }
}
