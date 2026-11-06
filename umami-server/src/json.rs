//! JSON conventions of upstream's responses.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serializer;

/// A timestamp as JavaScript's `Date#toJSON`: `2026-11-06T12:25:01.546Z`.
pub fn date(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Serde adapter for optional timestamps in [`date`] form.
pub fn serialize_date<S: Serializer>(
    value: &Option<DateTime<Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => serializer.serialize_str(&date(*value)),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_javascript() {
        let at: DateTime<Utc> = "2026-11-06T12:25:01.546789Z".parse().unwrap();
        assert_eq!(date(at), "2026-11-06T12:25:01.546Z");

        let whole: DateTime<Utc> = "2026-11-06T12:25:01Z".parse().unwrap();
        assert_eq!(date(whole), "2026-11-06T12:25:01.000Z");
    }
}
