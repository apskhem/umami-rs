//! Ports of the zod validators upstream's request schemas use.

use uuid::Uuid;

use crate::collect::js;

/// zod 4's `z.uuid()`: hyphenated, RFC 9562 variant, version 1–8 (or the nil/max UUIDs).
pub fn uuid(s: &str) -> Option<Uuid> {
  let bytes = s.as_bytes();
  let well_formed = bytes.len() == 36
    && bytes.iter().enumerate().all(|(i, b)| {
      match i {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_hexdigit(),
      }
    });

  if !well_formed {
    return None;
  }

  let lower = s.to_ascii_lowercase();
  let special = lower == "00000000-0000-0000-0000-000000000000" || lower == "ffffffff-ffff-ffff-ffff-ffffffffffff";
  let versioned = matches!(bytes[14], b'1'..=b'8') && matches!(lower.as_bytes()[19], b'8' | b'9' | b'a' | b'b');

  if special || versioned {
    Uuid::parse_str(s).ok()
  } else {
    None
  }
}

/// `z.coerce.number().int().positive()` applied to a query-string value.
pub fn positive_int(s: &str) -> Option<i64> {
  const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
  let number = js::string_to_number(s);

  (number.fract() == 0.0 && number > 0.0 && number <= MAX_SAFE).then_some(number as i64)
}

#[cfg(test)]
mod tests {
  use super::*;

  const WEBSITE: &str = "0d8e3a5c-1f2b-4c3d-9e4f-5a6b7c8d9e0f";

  #[test]
  fn uuid_format_matches_zod() {
    assert!(uuid(WEBSITE).is_some());
    assert!(uuid(&WEBSITE.to_uppercase()).is_some());
    assert!(uuid("00000000-0000-0000-0000-000000000000").is_some());
    // Version 0 and non-RFC variants are rejected.
    assert!(uuid("0d8e3a5c-1f2b-0c3d-9e4f-5a6b7c8d9e0f").is_none());
    assert!(uuid("0d8e3a5c-1f2b-4c3d-1e4f-5a6b7c8d9e0f").is_none());
    // Forms the uuid crate accepts but zod does not.
    assert!(uuid("0d8e3a5c1f2b4c3d9e4f5a6b7c8d9e0f").is_none());
    assert!(uuid("{0d8e3a5c-1f2b-4c3d-9e4f-5a6b7c8d9e0f}").is_none());
  }

  #[test]
  fn coerces_positive_ints_like_zod() {
    assert_eq!(positive_int("3"), Some(3));
    assert_eq!(positive_int(" 3 "), Some(3));
    assert_eq!(positive_int("0x10"), Some(16));
    assert_eq!(positive_int("2e1"), Some(20));
    assert_eq!(positive_int("2.0"), Some(2));
    for invalid in ["", "0", "-1", "1.5", "abc", "Infinity", "1e300"] {
      assert_eq!(positive_int(invalid), None, "{invalid}");
    }
  }
}
