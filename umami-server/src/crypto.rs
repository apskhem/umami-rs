//! Ports of upstream's `src/lib/crypto.ts`.

use chrono::{
  DateTime,
  Datelike,
  Duration,
  TimeZone,
  Timelike,
  Utc,
};
use sha2::{
  Digest,
  Sha512,
};
use uuid::Uuid;

/// Upstream's `hash(...args)`: SHA-512 (lowercase hex) of the arguments concatenated.
pub fn hash(parts: &[&str]) -> String {
  let mut hasher = Sha512::new();

  for part in parts {
    hasher.update(part.as_bytes());
  }

  hex::encode(hasher.finalize())
}

/// Upstream's `uuid(...args)` with arguments: a v5 UUID (DNS namespace) of the
/// arguments hashed together with the app secret. JS joins `undefined` as an empty
/// string, so pass `""` for missing values.
pub fn deterministic_uuid(parts: &[&str], secret: &str) -> Uuid {
  let mut all = parts.to_vec();
  all.push(secret);

  Uuid::new_v5(&Uuid::NAMESPACE_DNS, hash(&all).as_bytes())
}

/// Upstream's `uuid()` without arguments.
pub fn random_uuid(use_v7: bool) -> Uuid {
  if use_v7 { Uuid::now_v7() } else { Uuid::new_v4() }
}

/// `SALT_ROTATION`: how often session IDs change for the same visitor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SaltRotation {
  Day,
  Week,
  #[default]
  Month,
}

impl SaltRotation {
  /// Upstream treats anything other than `day` or `week` as `month`.
  pub fn parse(value: &str) -> Self {
    match value {
      "day" => Self::Day,
      "week" => Self::Week,
      _ => Self::Month,
    }
  }
}

/// Upstream's `getSalt(rotation, createdAt)`.
///
/// Upstream computes the period start in the server's local time zone; this uses
/// UTC, which matches upstream's Docker image (no `TZ` set).
pub fn session_salt(rotation: SaltRotation, at: DateTime<Utc>) -> String {
  let day = Utc.with_ymd_and_hms(at.year(), at.month(), at.day(), 0, 0, 0).unwrap();

  let start = match rotation {
    SaltRotation::Day => day,
    // date-fns `startOfWeek` defaults to Sunday.
    SaltRotation::Week => day - Duration::days(at.weekday().num_days_from_sunday().into()),
    SaltRotation::Month => Utc.with_ymd_and_hms(at.year(), at.month(), 1, 0, 0, 0).unwrap(),
  };

  hash(&[&utc_string(start)])
}

/// Upstream's visit salt: `hash(startOfHour(createdAt).toUTCString())`.
pub fn visit_salt(at: DateTime<Utc>) -> String {
  let hour = at
    .with_minute(0)
    .and_then(|t| t.with_second(0))
    .and_then(|t| t.with_nanosecond(0))
    .unwrap();

  hash(&[&utc_string(hour)])
}

/// `Date.prototype.toUTCString()`, e.g. `Fri, 06 Nov 2026 13:00:00 GMT`.
fn utc_string(at: DateTime<Utc>) -> String {
  at.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Upstream's `getRandomChars`: alphanumeric characters from a CSPRNG.
pub fn random_chars(n: usize) -> String {
  const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
  let mut bytes = vec![0u8; n * 4];
  getrandom::fill(&mut bytes).expect("system randomness");

  bytes
    .chunks_exact(4)
    .map(|chunk| {
      let value = u32::from_le_bytes(chunk.try_into().unwrap());
      char::from(CHARS[value as usize % CHARS.len()])
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  // Expected values computed with upstream's crypto.ts (Node.js, TZ=UTC,
  // APP_SECRET=test-app-secret).
  const SECRET: &str = "36292cdada49fa7480ddf48a7fcd1d33b93bd7d4cd1e8f6da766e1af6dcc22b959eccc78982c0c3d5d71d6c095acaf779a77cb01753bcbf260c66ce52423320f";
  const WEBSITE: &str = "0d8e3a5c-1f2b-4c3d-9e4f-5a6b7c8d9e0f";
  const USER_AGENT: &str = "Mozilla/5.0 Test";

  fn created_at() -> DateTime<Utc> {
    "2026-11-06T13:45:30.123Z".parse().unwrap()
  }

  #[test]
  fn hash_matches_sha512_of_concatenation() {
    let abc = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
                   2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";

    assert_eq!(hash(&["abc"]), abc);
    assert_eq!(hash(&["a", "bc"]), abc);
    assert_eq!(hash(&["test-app-secret"]), SECRET);
  }

  #[test]
  fn salts_match_upstream() {
    let month = "86748d432a74e8d8b0c6c3c430303f0a25bd976d3f76a47cbbde45ce4ace324c894561d949e261be44c5b5e1f94c14d5c9af8c14ea21b85dc75381299ea28a4d";

    assert_eq!(
      session_salt(SaltRotation::Day, created_at()),
      "6c48f5b48cdcea64ad62ace60bed7d46ff12be44cf65006a40186522ee4c3882adfbd4cd43f95240a07b0cb585803abfc7bbb8166ada671aafe4e996b7347179"
    );
    // 2026-11-01 is a Sunday, so the week and month salts coincide.
    assert_eq!(session_salt(SaltRotation::Week, created_at()), month);
    assert_eq!(session_salt(SaltRotation::Month, created_at()), month);
    assert_eq!(utc_string(created_at()), "Fri, 06 Nov 2026 13:45:30 GMT");
  }

  #[test]
  fn session_and_visit_ids_match_upstream() {
    let salt = session_salt(SaltRotation::Month, created_at());
    let session = |ip: &str, distinct_id: &str| {
      deterministic_uuid(&[WEBSITE, ip, USER_AGENT, &salt, distinct_id], SECRET).to_string()
    };

    let session_id = session("203.0.113.7", "");
    assert_eq!(session_id, "ef98a066-967b-5a5f-aa43-3c1492f5d34e");
    assert_eq!(session("", ""), "ece26e63-b060-54a6-9d8e-77374d491255");
    assert_eq!(
      session("203.0.113.7", "user-42"),
      "a808d387-bb6d-5dbd-9b88-f21e95a849fd"
    );

    let visit_id = deterministic_uuid(&[&session_id, &visit_salt(created_at())], SECRET);
    assert_eq!(visit_id.to_string(), "4357da39-efbd-5647-a20c-8896a5379030");
  }

  #[test]
  fn week_starts_on_sunday() {
    let saturday: DateTime<Utc> = "2026-11-07T23:59:59Z".parse().unwrap();
    let sunday: DateTime<Utc> = "2026-11-08T00:00:00Z".parse().unwrap();

    assert_eq!(
      session_salt(SaltRotation::Week, saturday),
      session_salt(SaltRotation::Week, created_at())
    );
    assert_ne!(
      session_salt(SaltRotation::Week, sunday),
      session_salt(SaltRotation::Week, saturday)
    );
  }
}
