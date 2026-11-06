//! Tokens compatible with upstream's `src/lib/jwt.ts` (node `jsonwebtoken`) and the
//! `encrypt`/`decrypt` pair in `src/lib/crypto.ts`, so tokens issued by either
//! server are accepted by the other.

use aes_gcm::AesGcm;
use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{
  Aead,
  KeyInit,
};
use aes_gcm::aes::Aes256;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use jsonwebtoken::{
  Algorithm,
  DecodingKey,
  EncodingKey,
  Header,
  Validation,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Sha512;

/// `jwt.sign(claims, secret)`: HS256. Unlike node, `iat`/`exp` are not added here;
/// include them in `claims` where upstream would.
pub fn sign(claims: &impl Serialize, secret: &str) -> String {
  jsonwebtoken::encode(
    &Header::new(Algorithm::HS256),
    claims,
    &EncodingKey::from_secret(secret.as_bytes()),
  )
  .expect("serializing JWT claims cannot fail")
}

/// `jwt.verify(token, secret)`: `None` for any invalid token.
pub fn verify<T: DeserializeOwned>(token: &str, secret: &str) -> Option<T> {
  // node `jsonwebtoken` defaults for an HMAC secret: HS256/384/512, with `exp`
  // and `nbf` checked only when present.
  let mut validation = Validation::new(Algorithm::HS256);
  validation.algorithms = vec![Algorithm::HS256, Algorithm::HS384, Algorithm::HS512];
  validation.required_spec_claims.clear();
  validation.validate_exp = true;
  validation.validate_nbf = true;
  validation.leeway = 0;

  jsonwebtoken::decode::<T>(token, &DecodingKey::from_secret(secret.as_bytes()), &validation)
    .ok()
    .map(|data| data.claims)
}

/// Upstream's `createSecureToken`: a JWT encrypted with [`encrypt`].
pub fn sign_secure(claims: &impl Serialize, secret: &str) -> String {
  encrypt(&sign(claims, secret), secret)
}

/// Upstream's `parseSecureToken`.
pub fn verify_secure<T: DeserializeOwned>(token: &str, secret: &str) -> Option<T> {
  verify(&decrypt(token, secret)?, secret)
}

const SALT_LENGTH: usize = 64;
const IV_LENGTH: usize = 16;
const TAG_LENGTH: usize = 16;
const PBKDF2_ROUNDS: u32 = 10_000;

/// AES-256-GCM with upstream's 16-byte IV.
type Cipher = AesGcm<Aes256, U16>;

fn cipher(secret: &str, salt: &[u8]) -> Cipher {
  let mut key = [0u8; 32];
  pbkdf2::pbkdf2_hmac::<Sha512>(secret.as_bytes(), salt, PBKDF2_ROUNDS, &mut key);
  Cipher::new_from_slice(&key).expect("32-byte key")
}

/// Upstream's `encrypt(value, secret)`: base64 of `salt | iv | tag | ciphertext`,
/// keyed by PBKDF2-SHA512 of the secret.
pub fn encrypt(plaintext: &str, secret: &str) -> String {
  let mut salt = [0u8; SALT_LENGTH];
  let mut iv = [0u8; IV_LENGTH];
  getrandom::fill(&mut salt).expect("system randomness");
  getrandom::fill(&mut iv).expect("system randomness");

  let sealed = cipher(secret, &salt)
    .encrypt(&iv.into(), plaintext.as_bytes())
    .expect("AES-GCM encryption cannot fail");
  // aes-gcm appends the tag; upstream puts it before the ciphertext.
  let (ciphertext, tag) = sealed.split_at(sealed.len() - TAG_LENGTH);

  BASE64.encode([&salt[..], &iv, tag, ciphertext].concat())
}

/// Upstream's `decrypt(value, secret)`; `None` if the value was not produced by
/// [`encrypt`] with this secret.
pub fn decrypt(value: &str, secret: &str) -> Option<String> {
  let bytes = BASE64.decode(value).ok()?;
  if bytes.len() < SALT_LENGTH + IV_LENGTH + TAG_LENGTH {
    return None;
  }

  let (salt, rest) = bytes.split_at(SALT_LENGTH);
  let (iv, rest) = rest.split_at(IV_LENGTH);
  let (tag, ciphertext) = rest.split_at(TAG_LENGTH);
  let sealed = [ciphertext, tag].concat();
  let iv: [u8; IV_LENGTH] = iv.try_into().ok()?;

  let plaintext = cipher(secret, salt).decrypt(&iv.into(), sealed.as_slice()).ok()?;

  String::from_utf8(plaintext).ok()
}

#[cfg(test)]
mod tests {
  use serde_json::{
    Value,
    json,
  };

  use super::*;

  // Produced by upstream's createSecureToken / jwt.sign with
  // secret() = hash("test-app-secret").
  const SECRET: &str = "36292cdada49fa7480ddf48a7fcd1d33b93bd7d4cd1e8f6da766e1af6dcc22b959eccc78982c0c3d5d71d6c095acaf779a77cb01753bcbf260c66ce52423320f";
  const UPSTREAM_SESSION: &str = "omy3uALPULne4m4/vAFzw4gP8WkvBiop3nKeJZogt+mr1GYgbmItJ+t+yB7+9dGo6M24C3rZUK+QJ5atEzvr5vkfka71UQZBKB7/IvUlVZ2/iCoYeuvDEpRDMRXIvrSOJRDIEo+gQ8YCHveibzcb6NOO0bEIuraOr9uNGWyu5cBdOCs0uNwiMxmI9upS5kEGRy+APlJqsbvYspYIIhry9PG+r9aDAJrqeQ72nvPve9cr8Jj288iMGIKoVQJLa4V37U274ylk+IJ+cr+H5CYCGXG3Tz2kgurjztZjdXQX67RFCx2KnhJFxc229/Kx1NVlsiwGo6QfnrZIEzhimkwzHKyT4HnyrlM7Hc8Qe5QyCLBbC58cGka/aCqH0AKhm0GDBUbqy42sGcCmZ91HmPWtU1/mVEa9cuoEBcLzvAPFcvA3sY1hvs+lw4PtgYSHg6Pv0LZNwhFzh0cN0XbE6/n8EfItsUi1FEWPPw5R/ppz6MVTUoI+EMQFrCgY5IJv9kRpridVtdCBcCkWvGl+CVy4n823hKjltyzA6o6ALZj1uyO8ahE9o/VRbaMujB/tAig7IpbGPf/oLsR0KWQpDLOpqpQNIHKt85bTcHbh1XtE/M8w6cJH";
  const UPSTREAM_EXPIRED_PARTIAL: &str = "Q0HRaQ/mfaZWxi0Hbg1ESGa1IxBDMrQ1U9BBAidYEuuk5MfG6801K2DBOM8HW/dfS1E1MzqrIGbYIrkVEs9qqSnhdBjx5q9bwWDSAgoCQAp1/jxlhJT6Zo/ShxYLvzGZP0OX10aV0uWHJCdo8sMtEAGWx1YWA+8BcDt0rKtTCWoqrLAsR0dDwjxAx5lc5qLAhEgTbS+cgtJx17kHQ6idZ1HMZqwXV4cJ7DJ2P3oYxCKr3tfMshElJ3jlzqwIz4cWGk2R0YvqknP3NBnSrIjSl0hGI+Jqk7uAhLaNZq9QSFX+FoEad+G9KEcbN75fX1z20CGRN5ehcUTy6z91gzVT";
  const UPSTREAM_OTHER_SECRET: &str = "un4JM3ej9K74cix/EEnCvpR5YQiUx3x6unmx7hVc3CMZQLq+voBrrHIeuIZlKyeDlKP5to3LJ1wrXLQdZcp5n9V2o3FkksvNmsnM5gGvTkHR8wO89zh4Orax8Eb/DJ8wFzsQyFjB8+o9vG/p+7AHOYzZ7CbVKLCMHsS9inVJEAyqnlpKJOf7o5wURS+YYCcg4C1r89xy2nrnecYNyyOiqYEN6TgdmrgQh6ixcIPKKCSwPI2qE3cgRDE4OupF+ACgB2T5wnqFNtkHYM1plv9gsjKcnM7Y55bDp54s";

  #[test]
  fn reads_upstream_secure_tokens() {
    let claims: Value = verify_secure(UPSTREAM_SESSION, SECRET).unwrap();

    assert_eq!(claims["userId"], "41e2b680-648e-4b09-bcd7-3e2b10c06264");
    assert_eq!(claims["role"], "admin");
    assert_eq!(
      claims["pwd"],
      "253d12bc5a638b8d44dba606c2598836e7d9c88a7b4ce929351af5e8e98668d60a7c352f46c4ecf8ce3b86038251b1770b56fd7a002bc7b2c88baadbf19f198c"
    );
    assert!(claims["iat"].is_number());
  }

  #[test]
  fn rejects_expired_and_foreign_tokens() {
    assert_eq!(verify_secure::<Value>(UPSTREAM_EXPIRED_PARTIAL, SECRET), None);
    // Decrypts only with its own secret.
    assert_eq!(decrypt(UPSTREAM_OTHER_SECRET, SECRET), None);
    assert_eq!(verify_secure::<Value>("not base64!", SECRET), None);
    assert_eq!(verify_secure::<Value>("", SECRET), None);
  }

  #[test]
  fn round_trips() {
    let claims = json!({ "userId": "u", "iat": 1 });
    let token = sign_secure(&claims, SECRET);

    assert_eq!(verify_secure::<Value>(&token, SECRET), Some(claims));
    // Fresh salt and IV each time.
    assert_ne!(sign_secure(&json!({ "userId": "u", "iat": 1 }), SECRET), token);
  }
}
