//! JavaScript semantics that upstream's ingestion code depends on, so values are
//! stored exactly as upstream stores them.

use std::borrow::Cow;

/// `str.substring(0, max)`: truncates to `max` UTF-16 code units. A surrogate pair
/// split by the cut is dropped entirely, since Rust strings can't hold half of one.
pub fn truncate(s: &str, max: usize) -> &str {
  let mut units = 0;

  for (index, c) in s.char_indices() {
    units += c.len_utf16();
    if units > max {
      return &s[..index];
    }
  }

  s
}

/// [`truncate`] for owned optional values.
pub fn truncate_opt(value: Option<String>, max: usize) -> Option<String> {
  value.map(|s| {
    match truncate(&s, max).len() {
      len if len == s.len() => s,
      len => s[..len].to_owned(),
    }
  })
}

/// `Number.prototype.toFixed(4)`.
pub fn to_fixed4(x: f64) -> String {
  if !x.is_finite() || x.abs() >= 1e21 {
    return number_to_string(x);
  }

  // Both round the exact binary value, but on an exact tie Rust picks the even
  // digit while JS picks the larger magnitude. x * 10^4 = k + 1/2 exactly only when
  // x is an odd multiple of 1/32 (20000 = 2^5 * 5^4), which multiplying by 32 tests
  // without rounding error.
  let thirty_seconds = x.abs() * 32.0;
  if thirty_seconds.fract() == 0.0 && thirty_seconds % 2.0 == 1.0 && thirty_seconds < 2f64.powi(48) {
    let rounded = ((x.abs() * 10_000.0).trunc() + 1.0) / 10_000.0;
    let sign = if x < 0.0 { "-" } else { "" };
    return format!("{sign}{rounded:.4}");
  }

  format!("{x:.4}")
}

/// `Number.prototype.toString()` for the cases upstream can hit.
pub fn number_to_string(x: f64) -> String {
  if x.is_nan() {
    return "NaN".to_owned();
  }
  if x.is_infinite() {
    return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
  }
  if x == 0.0 {
    return "0".to_owned();
  }

  let abs = x.abs();
  if (1e-6..1e21).contains(&abs) {
    return format!("{x}");
  }

  let exponential = format!("{x:e}");
  match exponential.split_once('e') {
    Some((mantissa, exponent)) if !exponent.starts_with('-') => {
      format!("{mantissa}e+{exponent}")
    }
    _ => exponential,
  }
}

/// `Number(string)`, as used by upstream for `+width`.
pub fn string_to_number(s: &str) -> f64 {
  let s = s.trim_matches(is_js_whitespace);

  if s.is_empty() {
    return 0.0;
  }

  for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
    if let Some(digits) = s.strip_prefix(prefix) {
      return u128::from_str_radix(digits, radix)
        .map(|n| n as f64)
        .unwrap_or(f64::NAN);
    }
  }

  let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
  if unsigned == "Infinity" {
    return if s.starts_with('-') {
      f64::NEG_INFINITY
    } else {
      f64::INFINITY
    };
  }

  // Rust also accepts "inf"/"nan"; JS only accepts decimal literals here.
  let is_decimal = !unsigned.is_empty()
    && unsigned
      .chars()
      .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    && unsigned.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '.');

  if is_decimal {
    s.parse().unwrap_or(f64::NAN)
  } else {
    f64::NAN
  }
}

/// JS `\s` / `String.prototype.trim` whitespace.
pub fn is_js_whitespace(c: char) -> bool {
  matches!(
      c,
      '\u{9}'..='\u{d}'
          | ' '
          | '\u{a0}'
          | '\u{1680}'
          | '\u{2000}'..='\u{200a}'
          | '\u{2028}'
          | '\u{2029}'
          | '\u{202f}'
          | '\u{205f}'
          | '\u{3000}'
          | '\u{feff}'
  )
}

/// Upstream's `safeDecodeURI`: `decodeURI`, or the input unchanged if it is malformed.
pub fn safe_decode_uri(s: &str) -> Cow<'_, str> {
  decode(s, ";/?:@&=+$,#").map_or(Cow::Borrowed(s), Cow::Owned)
}

/// Upstream's `safeDecodeURIComponent`.
pub fn safe_decode_uri_component(s: &str) -> Cow<'_, str> {
  decode(s, "").map_or(Cow::Borrowed(s), Cow::Owned)
}

/// The ECMAScript `Decode` operation. Escapes that decode to a character in
/// `reserved` are kept verbatim. Returns `None` where JS throws `URIError`.
fn decode(s: &str, reserved: &str) -> Option<String> {
  if !s.contains('%') {
    return Some(s.to_owned());
  }

  let bytes = s.as_bytes();
  let mut out = Vec::with_capacity(bytes.len());
  let mut i = 0;

  let hex_byte = |at: usize| -> Option<u8> {
    if bytes.get(at) != Some(&b'%') {
      return None;
    }
    let hex = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
    u8::from_str_radix(hex, 16).ok()
  };

  while i < bytes.len() {
    if bytes[i] != b'%' {
      out.push(bytes[i]);
      i += 1;
      continue;
    }

    let lead = hex_byte(i)?;

    if lead < 0x80 {
      if reserved.as_bytes().contains(&lead) {
        out.extend_from_slice(&bytes[i..i + 3]);
      } else {
        out.push(lead);
      }
      i += 3;
      continue;
    }

    let len = match lead.leading_ones() {
      n @ 2..=4 => n as usize,
      _ => return None,
    };

    let mut sequence = vec![lead];
    for k in 1..len {
      let byte = hex_byte(i + 3 * k)?;
      if byte & 0xC0 != 0x80 {
        return None;
      }
      sequence.push(byte);
    }

    // Rejects overlong forms, surrogates and out-of-range code points, as JS does.
    std::str::from_utf8(&sequence).ok()?;
    out.extend_from_slice(&sequence);
    i += 3 * len;
  }

  String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  // Expected values computed with Node.js.

  #[test]
  fn to_fixed4_matches_js() {
    let cases = [
      (0.0, "0.0000"),
      (1.0, "1.0000"),
      (-1.0, "-1.0000"),
      (0.1, "0.1000"),
      (1.23456, "1.2346"),
      (0.03125, "0.0313"),
      (-0.03125, "-0.0313"),
      (1.00005, "1.0001"),
      (2.5e-5, "0.0000"),
      (123456789012.34567, "123456789012.3457"),
      (1e20, "100000000000000000000.0000"),
      (1e21, "1e+21"),
      (-1e21, "-1e+21"),
      (5e-324, "0.0000"),
      (0.5, "0.5000"),
      (1.0 / 3.0, "0.3333"),
      (99999.99995, "99999.9999"),
      (0.00015, "0.0001"),
    ];

    for (x, expected) in cases {
      assert_eq!(to_fixed4(x), expected, "{x}");
    }
  }

  #[test]
  fn decode_uri_matches_js() {
    let cases = [
      ("/a%20b", "/a b", "/a b"),
      ("/%E2%82%AC", "/€", "/€"),
      (
        "/a%2Fb%3F%23%26%3D%2B%24%2C%3B%3A%40",
        "/a%2Fb%3F%23%26%3D%2B%24%2C%3B%3A%40",
        "/a/b?#&=+$,;:@",
      ),
      ("/%zz", "/%zz", "/%zz"),
      ("/%E2%82", "/%E2%82", "/%E2%82"),
      ("/%C0%AF", "/%C0%AF", "/%C0%AF"),
      ("/%ED%A0%80", "/%ED%A0%80", "/%ED%A0%80"),
      ("/%41%42", "/AB", "/AB"),
      ("%", "%", "%"),
      ("plain", "plain", "plain"),
      ("/caf%C3%A9?x=%26", "/café?x=%26", "/café?x=&"),
      ("%F0%9F%98%80", "😀", "😀"),
    ];

    for (input, uri, component) in cases {
      assert_eq!(safe_decode_uri(input), uri, "decodeURI({input})");
      assert_eq!(
        safe_decode_uri_component(input),
        component,
        "decodeURIComponent({input})"
      );
    }
  }

  #[test]
  fn truncate_counts_utf16_units() {
    assert_eq!(truncate("hello", 3), "hel");
    assert_eq!(truncate("hello", 10), "hello");
    assert_eq!(truncate("é€x", 2), "é€");
    // 😀 is two UTF-16 units: cutting inside it drops it.
    assert_eq!(truncate("a😀b", 2), "a");
    assert_eq!(truncate("a😀b", 3), "a😀");
  }

  #[test]
  fn string_to_number_matches_js() {
    assert_eq!(string_to_number("1366"), 1366.0);
    assert_eq!(string_to_number(" 1366 "), 1366.0);
    assert_eq!(string_to_number(""), 0.0);
    assert_eq!(string_to_number("0x10"), 16.0);
    assert_eq!(string_to_number("1e3"), 1000.0);
    assert_eq!(string_to_number("-Infinity"), f64::NEG_INFINITY);
    assert!(string_to_number("inf").is_nan());
    assert!(string_to_number("12px").is_nan());
  }

  #[test]
  fn number_to_string_matches_js() {
    assert_eq!(number_to_string(1e21), "1e+21");
    assert_eq!(number_to_string(1.5e300), "1.5e+300");
    assert_eq!(number_to_string(1e-7), "1e-7");
    assert_eq!(number_to_string(42.5), "42.5");
    assert_eq!(number_to_string(-0.0), "0");
  }
}
