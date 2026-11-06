//! Request-body helpers mirroring upstream's zod schemas.

use axum::body::Bytes;
use serde_json::{Map, Value};

use crate::error::AppError;

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
