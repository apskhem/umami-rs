//! Time-based one-time passwords, compatible with upstream's use of otplib 13:
//! SHA-1, 6 digits, 30-second steps, base32 secrets of 20 bytes.
//!
//! Upstream accepts only the current step, so a phone clock a few seconds off
//! fails. This accepts the adjacent steps too (RFC 6238's recommended tolerance);
//! upstream's 90-second replay window already covers a code's full validity.

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

const DIGITS: u32 = 6;
const PERIOD: i64 = 30;
const SECRET_BYTES: usize = 20;
/// otplib refuses shorter secrets.
const MIN_SECRET_BYTES: usize = 16;

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// otplib's `generateSecret()`: 20 random bytes, base32-encoded without padding.
pub fn generate_secret() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).expect("system randomness");
    base32_encode(&bytes)
}

/// otplib's `generateURI({ issuer: 'Umami', label, secret })`.
pub fn otpauth_uri(secret: &str, label: &str) -> String {
    format!(
        "otpauth://totp/Umami:{}?secret={secret}&issuer=Umami",
        encode_label(label)
    )
}

/// Steps either side of the current one that are also accepted.
const TOLERANCE_STEPS: i64 = 1;

/// Whether `token` is the code for the current time step or an adjacent one.
/// `None` where otplib throws: a malformed or too-short secret, or a token that
/// is not all digits.
pub fn verify(token: &str, secret: &str, now: i64) -> Option<bool> {
    if token.len() != DIGITS as usize || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let mut matched = false;
    for step in -TOLERANCE_STEPS..=TOLERANCE_STEPS {
        let expected = code_at(secret, now + step * PERIOD)?;
        // Constant-time comparison of the six digits, checking every step.
        let diff = expected
            .bytes()
            .zip(token.bytes())
            .fold(0, |diff, (a, b)| diff | (a ^ b));
        matched |= diff == 0;
    }

    Some(matched)
}

/// The code for the time step containing `epoch` (Unix seconds).
pub fn code_at(secret: &str, epoch: i64) -> Option<String> {
    let key = base32_decode(secret)?;
    if key.len() < MIN_SECRET_BYTES {
        return None;
    }

    let counter = epoch.div_euclid(PERIOD) as u64;
    let mut mac = Hmac::<Sha1>::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();

    // RFC 4226 dynamic truncation.
    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[offset],
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]) & 0x7fff_ffff;

    Some(format!("{:06}", binary % 10u32.pow(DIGITS)))
}

/// `encodeURIComponent`, keeping `:` as otplib does in labels.
fn encode_label(label: &str) -> String {
    let mut out = String::new();

    for byte in label.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')'
            | b':' => out.push(char::from(byte)),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }

    out
}

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0);

    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(BASE32[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(BASE32[((buffer << (5 - bits)) & 31) as usize]));
    }

    out
}

/// Decodes base32 case-insensitively, ignoring padding (as otplib's base32 plugin).
fn base32_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0);

    for c in input.trim_end_matches('=').bytes() {
        let value = BASE32.iter().position(|&a| a == c.to_ascii_uppercase())? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";

    // Codes and URIs produced by otplib 13.6.
    #[test]
    fn codes_match_otplib() {
        for (epoch, code) in [
            (59, "503347"),
            (1111111109, "088309"),
            (1234567890, "401544"),
            (2000000000, "978524"),
        ] {
            assert_eq!(code_at(SECRET, epoch).as_deref(), Some(code), "{epoch}");
        }
    }

    #[test]
    fn tolerates_one_step_of_clock_drift() {
        let now = 1234567890;
        let code = code_at(SECRET, now).unwrap();

        assert_eq!(verify(&code, SECRET, now), Some(true));
        assert_eq!(verify(&code, &SECRET.to_lowercase(), now), Some(true));
        assert_eq!(verify(&code, SECRET, now + 30), Some(true));
        assert_eq!(verify(&code, SECRET, now - 30), Some(true));
        assert_eq!(verify(&code, SECRET, now + 60), Some(false));
        assert_eq!(verify(&code, SECRET, now - 60), Some(false));
        assert_eq!(verify("abcdef", SECRET, now), None);
        assert_eq!(
            verify(&code, "JBSWY3DPEHPK3PXP", now),
            None,
            "10-byte secret"
        );
    }

    #[test]
    fn uris_match_otplib() {
        let uri = |label| otpauth_uri(SECRET, label);

        assert_eq!(
            uri("ana@example.com"),
            format!("otpauth://totp/Umami:ana%40example.com?secret={SECRET}&issuer=Umami")
        );
        assert_eq!(
            uri("a:b"),
            format!("otpauth://totp/Umami:a:b?secret={SECRET}&issuer=Umami")
        );
        assert_eq!(
            uri("ünï"),
            format!("otpauth://totp/Umami:%C3%BCn%C3%AF?secret={SECRET}&issuer=Umami")
        );
        assert_eq!(
            uri("o'neil&co/x?y#z"),
            format!("otpauth://totp/Umami:o'neil%26co%2Fx%3Fy%23z?secret={SECRET}&issuer=Umami")
        );
    }

    #[test]
    fn generates_twenty_byte_secrets() {
        let secret = generate_secret();

        assert_eq!(secret.len(), 32);
        assert_eq!(base32_decode(&secret).unwrap().len(), 20);
        assert_eq!(base32_encode(&base32_decode(SECRET).unwrap()), SECRET);
    }
}
