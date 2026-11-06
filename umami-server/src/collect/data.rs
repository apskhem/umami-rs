//! Flattening of custom event/session data into typed rows: port of upstream's
//! `src/lib/data.ts`.

use std::sync::LazyLock;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use regex::Regex;
use serde_json::{Map, Value};

use super::js;

/// Upstream's `DATA_TYPE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum DataType {
    String = 1,
    Number = 2,
    Boolean = 3,
    Date = 4,
    Array = 5,
}

/// One `event_data` / `session_data` row's values.
#[derive(Debug, PartialEq)]
pub struct Field {
    pub key: String,
    pub string_value: Option<String>,
    /// Bound as text and cast to `numeric`, like Prisma's `Decimal`.
    pub number_value: Option<String>,
    pub date_value: Option<DateTime<Utc>>,
    pub data_type: DataType,
}

const KEY_LENGTH: usize = 500;
const STRING_LENGTH: usize = 500;

static DATETIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{3})?(Z|\+[0-9]{2}:[0-9]{2})?$")
        .unwrap()
});

/// Upstream's `flattenJSON` plus the per-row value handling of `saveEventData` /
/// `saveSessionData`. Nested objects become dotted keys; arrays and `null` are
/// stored as JSON text (type "array"), as upstream does.
pub fn flatten(data: &Map<String, Value>) -> Vec<Field> {
    let mut fields = Vec::new();
    flatten_into(data, "", &mut fields);
    fields
}

fn flatten_into(object: &Map<String, Value>, parent: &str, fields: &mut Vec<Field>) {
    for (key, value) in object {
        let key = if parent.is_empty() {
            key.clone()
        } else {
            format!("{parent}.{key}")
        };

        match value {
            Value::Object(nested) => flatten_into(nested, &key, fields),
            _ => fields.push(field(key, value)),
        }
    }
}

fn field(key: String, value: &Value) -> Field {
    let key = js::truncate(&key, KEY_LENGTH).to_owned();
    let string = |s: String| Some(js::truncate(&s, STRING_LENGTH).to_owned());

    match value {
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            Field {
                key,
                string_value: string(js::to_fixed4(x)),
                number_value: Some(js::number_to_string(x)),
                date_value: None,
                data_type: DataType::Number,
            }
        }
        Value::Bool(b) => Field {
            key,
            string_value: Some(b.to_string()),
            number_value: None,
            date_value: None,
            data_type: DataType::Boolean,
        },
        Value::String(s) => match parse_date(s) {
            Some(date) => Field {
                key,
                string_value: Some(date.to_rfc3339_opts(SecondsFormat::Millis, true)),
                number_value: None,
                date_value: Some(date),
                data_type: DataType::Date,
            },
            None => Field {
                key,
                string_value: string(s.clone()),
                number_value: None,
                date_value: None,
                data_type: DataType::String,
            },
        },
        // Arrays, and `null` (JS `typeof null === 'object'`).
        Value::Array(_) | Value::Null | Value::Object(_) => {
            let json = value.to_string();
            Field {
                key,
                // Too-long arrays are dropped rather than truncated into invalid JSON.
                string_value: (json.encode_utf16().count() <= STRING_LENGTH).then_some(json),
                number_value: None,
                date_value: None,
                data_type: DataType::Array,
            }
        }
    }
}

/// Upstream's `DATETIME_REGEX` test plus `new Date(value)`. A value without an
/// offset is local time in JS; upstream's Docker image runs in UTC. A string that
/// matches the pattern but is not a real date (where JS would throw) is kept as a
/// plain string instead.
fn parse_date(s: &str) -> Option<DateTime<Utc>> {
    if !DATETIME.is_match(s) {
        return None;
    }

    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|naive| naive.and_utc())
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn date(s: &str) -> Option<DateTime<Utc>> {
        Some(s.parse().unwrap())
    }

    // Expected rows computed with upstream's data.ts in Node.js (TZ=UTC).
    #[test]
    fn matches_upstream() {
        let data = json!({
            "str": "hello",
            "num": 42.5,
            "int": 7,
            "neg": -0.03125,
            "bool": true,
            "f": false,
            "nil": null,
            "arr": [1, "two", { "three": 3 }],
            "nested": { "a": { "b": "deep" }, "d": "2024-01-02T03:04:05Z" },
            "date": "2024-01-02T03:04:05.678+02:00",
            "datelocal": "2024-01-02T03:04:05",
            "empty": "",
            "long": "x".repeat(600),
            "longarr": vec!["abc"; 200],
            "obj": {},
        });

        let rows: Vec<_> = flatten(data.as_object().unwrap())
            .into_iter()
            .map(|f| {
                (
                    f.key,
                    f.string_value,
                    f.number_value,
                    f.date_value,
                    f.data_type,
                )
            })
            .collect();

        let s = |v: &str| Some(v.to_owned());
        assert_eq!(
            rows,
            vec![
                ("str".into(), s("hello"), None, None, DataType::String),
                (
                    "num".into(),
                    s("42.5000"),
                    s("42.5"),
                    None,
                    DataType::Number
                ),
                ("int".into(), s("7.0000"), s("7"), None, DataType::Number),
                (
                    "neg".into(),
                    s("-0.0313"),
                    s("-0.03125"),
                    None,
                    DataType::Number
                ),
                ("bool".into(), s("true"), None, None, DataType::Boolean),
                ("f".into(), s("false"), None, None, DataType::Boolean),
                ("nil".into(), s("null"), None, None, DataType::Array),
                (
                    "arr".into(),
                    s(r#"[1,"two",{"three":3}]"#),
                    None,
                    None,
                    DataType::Array
                ),
                ("nested.a.b".into(), s("deep"), None, None, DataType::String),
                (
                    "nested.d".into(),
                    s("2024-01-02T03:04:05.000Z"),
                    None,
                    date("2024-01-02T03:04:05.000Z"),
                    DataType::Date
                ),
                (
                    "date".into(),
                    s("2024-01-02T01:04:05.678Z"),
                    None,
                    date("2024-01-02T01:04:05.678Z"),
                    DataType::Date
                ),
                (
                    "datelocal".into(),
                    s("2024-01-02T03:04:05.000Z"),
                    None,
                    date("2024-01-02T03:04:05.000Z"),
                    DataType::Date
                ),
                ("empty".into(), s(""), None, None, DataType::String),
                (
                    "long".into(),
                    s(&"x".repeat(500)),
                    None,
                    None,
                    DataType::String
                ),
                ("longarr".into(), None, None, None, DataType::Array),
            ]
        );
    }

    #[test]
    fn impossible_dates_stay_strings() {
        let fields = flatten(json!({ "d": "2024-13-45T00:00:00Z" }).as_object().unwrap());

        assert_eq!(fields[0].data_type, DataType::String);
    }
}
