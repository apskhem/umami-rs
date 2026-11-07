//! Request-body helpers mirroring upstream's zod schemas.

use axum::body::Bytes;
use serde_json::{
  Map,
  Value,
};
use uuid::Uuid;

use crate::error::AppError;
use crate::{
  js,
  zod,
};

/// The JSON body, whatever the content type (as upstream's `request.json()`);
/// malformed bodies are `null`, which no schema accepts.
pub fn parse(bytes: &Bytes) -> Value {
  serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

/// The body as an object, or upstream's 400.
pub fn object(body: &Value) -> Result<&Map<String, Value>, AppError> {
  body.as_object().ok_or_else(AppError::bad_request)
}

/// A required string field.
pub fn string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, AppError> {
  object
    .get(key)
    .and_then(Value::as_str)
    .ok_or_else(AppError::bad_request)
}

/// An optional string field (`z.string().optional()`): absent is `None`, but
/// `null` is invalid.
pub fn optional_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, AppError> {
  match object.get(key) {
    None => Ok(None),
    Some(Value::String(s)) => Ok(Some(s)),
    Some(_) => Err(AppError::bad_request()),
  }
}

/// A nullable, optional string field (`.nullable().optional()`): absent is
/// `None`, `null` is `Some(None)`.
pub fn nullable_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<Option<&'a str>>, AppError> {
  match object.get(key) {
    None => Ok(None),
    Some(Value::Null) => Ok(Some(None)),
    Some(Value::String(s)) => Ok(Some(Some(s))),
    Some(_) => Err(AppError::bad_request()),
  }
}

/// A nullable, optional UUID field (`z.uuid().nullable().optional()`); `null`
/// and absent are both `None`.
pub fn nullable_uuid(object: &Map<String, Value>, key: &str) -> Result<Option<Uuid>, AppError> {
  nullable_string(object, key)?.flatten().map(uuid).transpose()
}

/// An optional object field: `anyObjectParam` (`z.record(z.string(), z.any())`),
/// or `z.object(...)` before its fields are checked.
pub fn optional_object<'a>(
  object: &'a Map<String, Value>,
  key: &str,
) -> Result<Option<&'a Map<String, Value>>, AppError> {
  match object.get(key) {
    None => Ok(None),
    Some(Value::Object(value)) => Ok(Some(value)),
    Some(_) => Err(AppError::bad_request()),
  }
}

/// A required object field (see [`optional_object`]).
pub fn object_field<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a Map<String, Value>, AppError> {
  optional_object(object, key)?.ok_or_else(AppError::bad_request)
}

/// An optional array field (`z.array(...).optional()`).
pub fn optional_array<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a Vec<Value>>, AppError> {
  match object.get(key) {
    None => Ok(None),
    Some(Value::Array(items)) => Ok(Some(items)),
    Some(_) => Err(AppError::bad_request()),
  }
}

/// A `z.uuid()` value.
pub fn uuid(value: &str) -> Result<Uuid, AppError> {
  zod::uuid(value).ok_or_else(AppError::bad_request)
}

/// Fails unless `value` is at most `max` characters (`z.string().max(max)`).
pub fn max_len(value: &str, max: usize) -> Result<&str, AppError> {
  if js_len(value) > max {
    return Err(AppError::bad_request());
  }
  Ok(value)
}

/// Fails unless `value` is one of `allowed` (`z.enum([...])`).
pub fn one_of<'a>(value: &'a str, allowed: &[&str]) -> Result<&'a str, AppError> {
  if !allowed.contains(&value) {
    return Err(AppError::bad_request());
  }
  Ok(value)
}

/// zod's `.trim()`: JavaScript's `String.prototype.trim`.
pub fn trim(value: &str) -> &str {
  value.trim_matches(js::is_js_whitespace)
}

/// A required boolean field.
pub fn boolean(object: &Map<String, Value>, key: &str) -> Result<bool, AppError> {
  object
    .get(key)
    .and_then(Value::as_bool)
    .ok_or_else(AppError::bad_request)
}

/// String length as zod measures it (UTF-16 code units).
pub fn js_len(value: &str) -> usize {
  value.encode_utf16().count()
}
