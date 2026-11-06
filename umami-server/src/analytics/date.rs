//! Date ranges and time units, ported from upstream's `src/lib/date.ts`.
//!
//! Upstream computes these with date-fns in the server's time zone, which is UTC
//! in its Docker image; these use UTC.

use std::collections::HashSet;

use chrono::{
  DateTime,
  Datelike,
  Duration,
  Months,
  NaiveDate,
  TimeZone,
  Timelike,
  Utc,
};
use sqlx::PgPool;
use tokio::sync::OnceCell;

/// A chart bucket size (upstream's `UNIT_TYPES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Unit {
  Minute,
  Hour,
  Day,
  Month,
  Year,
}

impl Unit {
  const ALL: [Unit; 5] = [Unit::Minute, Unit::Hour, Unit::Day, Unit::Month, Unit::Year];

  pub fn parse(value: &str) -> Option<Self> {
    Self::ALL.into_iter().find(|unit| unit.as_str() == value)
  }

  pub fn as_str(self) -> &'static str {
    match self {
      Unit::Minute => "minute",
      Unit::Hour => "hour",
      Unit::Day => "day",
      Unit::Month => "month",
      Unit::Year => "year",
    }
  }

  /// The `to_char` format of a bucket: always `Z`-suffixed, since the value is
  /// already shifted into the requested time zone.
  pub fn sql_format(self) -> &'static str {
    match self {
      Unit::Minute => r#"YYYY-MM-DD"T"HH24:MI:00"Z""#,
      Unit::Hour | Unit::Day => r#"YYYY-MM-DD"T"HH24:00:00"Z""#,
      Unit::Month => r#"YYYY-MM-01"T"HH24:00:00"Z""#,
      Unit::Year => r#"YYYY-01-01"T"HH24:00:00"Z""#,
    }
  }
}

/// date-fns `differenceInMinutes`: whole minutes, truncated toward zero.
pub fn minutes_between(start: DateTime<Utc>, end: DateTime<Utc>) -> i64 {
  (end - start).num_milliseconds() / 60_000
}

fn calendar_months_between(start: DateTime<Utc>, end: DateTime<Utc>) -> i64 {
  i64::from(end.year() - start.year()) * 12 + i64::from(end.month()) - i64::from(start.month())
}

/// Upstream's `getMinimumUnit`: the finest unit that keeps a chart readable.
pub fn minimum_unit(start: DateTime<Utc>, end: DateTime<Utc>, is_date_range: bool) -> Unit {
  let millis = (end - start).num_milliseconds();
  let hours = millis / 3_600_000;
  let days = millis / 86_400_000;

  if minutes_between(start, end) <= 60 {
    Unit::Minute
  } else if (is_date_range && hours <= 48) || (!is_date_range && days <= 30) {
    Unit::Hour
  } else if calendar_months_between(start, end) <= 7 {
    Unit::Day
  } else if calendar_months_between(start, end) <= 24 {
    Unit::Month
  } else {
    Unit::Year
  }
}

/// Upstream's `getRequestDateRange` unit: the requested unit if the range allows
/// it (it may be no finer than the minimum, and year ranges allow months), else
/// the minimum.
pub fn request_unit(requested: Option<Unit>, start: DateTime<Utc>, end: DateTime<Utc>) -> Unit {
  let minimum = minimum_unit(start, end, false);
  let finest = if minimum == Unit::Year { Unit::Month } else { minimum };

  match requested {
    Some(unit) if unit >= finest => unit,
    _ => minimum,
  }
}

/// The comparison period of `compare` (upstream's `getCompareDate`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compare {
  /// The same length of time immediately before.
  Prev,
  /// The same dates a year earlier.
  Yoy,
}

impl Compare {
  pub fn parse(value: &str) -> Option<Self> {
    match value {
      "prev" => Some(Self::Prev),
      "yoy" => Some(Self::Yoy),
      _ => None,
    }
  }

  pub fn range(self, start: DateTime<Utc>, end: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    match self {
      Compare::Yoy => (sub_months(start, 12), sub_months(end, 12)),
      Compare::Prev => {
        let shift = Duration::minutes(minutes_between(start, end));
        (start - shift, end - shift)
      }
    }
  }
}

/// date-fns `subMonths`/`subYears`: the same day of an earlier month, clamped to
/// that month's last day.
fn sub_months(date: DateTime<Utc>, months: u32) -> DateTime<Utc> {
  date.checked_sub_months(Months::new(months)).unwrap_or(date)
}

fn start_of_day(date: DateTime<Utc>) -> DateTime<Utc> {
  date
    .with_hour(0)
    .and_then(|d| d.with_minute(0))
    .and_then(|d| d.with_second(0))
    .and_then(|d| d.with_nanosecond(0))
    .unwrap_or(date)
}

fn utc_date(date: NaiveDate) -> DateTime<Utc> {
  Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight exists"))
}

/// One millisecond before `next`, as date-fns `endOf*` returns.
fn end_before(next: DateTime<Utc>) -> DateTime<Utc> {
  next - Duration::milliseconds(1)
}

/// Parses a relative or explicit date range as upstream's `parseDateRange` (for
/// cohorts): `range:<startMs>:<endMs>`, or `<n><unit>` meaning the last `n`
/// units up to the end of the current one (`0day` is today). `None` if the
/// value is malformed, where upstream fails with a 500.
pub fn parse_date_range(value: &str, now: DateTime<Utc>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
  if let Some(rest) = value.strip_prefix("range") {
    let mut parts = rest.split(':').skip(1);
    let start = millis(parts.next()?)?;
    let end = millis(parts.next()?)?;
    return Some((start, end));
  }

  let unit_start = value.find(|c: char| !(c.is_ascii_digit() || c == '-'))?;
  let (digits, unit) = value.split_at(unit_start);
  // JS `+num`: anything that is not a plain integer is NaN, which counts as 0.
  let num: i64 = digits.parse().unwrap_or(0);
  if digits.is_empty() {
    return None;
  }

  let today = start_of_day(now);
  let (start, next) = match unit {
    "hour" => {
      let hour = today + Duration::hours(i64::from(now.hour()));
      (hour, hour + Duration::hours(1))
    }
    "day" => (today, today + Duration::days(1)),
    "week" => {
      // en-US weeks start on Sunday.
      let sunday = today - Duration::days(i64::from(now.weekday().num_days_from_sunday()));
      (sunday, sunday + Duration::weeks(1))
    }
    "month" => {
      let first = utc_date(NaiveDate::from_ymd_opt(now.year(), now.month(), 1)?);
      (first, first.checked_add_months(Months::new(1))?)
    }
    "year" => {
      let first = utc_date(NaiveDate::from_ymd_opt(now.year(), 1, 1)?);
      (first, first.checked_add_months(Months::new(12))?)
    }
    _ => return None,
  };

  let back = |n: i64| -> Option<DateTime<Utc>> {
    Some(match unit {
      "hour" => start - Duration::hours(n),
      "day" => start - Duration::days(n),
      "week" => start - Duration::weeks(n),
      "month" | "year" => {
        let months = if unit == "year" { n * 12 } else { n };
        if months >= 0 {
          start.checked_sub_months(Months::new(u32::try_from(months).ok()?))?
        } else {
          start.checked_add_months(Months::new(u32::try_from(-months).ok()?))?
        }
      }
      _ => return None,
    })
  };

  Some((back(num)?, end_before(next)))
}

/// A JavaScript timestamp (`new Date(+value)`); `None` for values JS cannot
/// represent.
pub fn millis(value: &str) -> Option<DateTime<Utc>> {
  let number = crate::collect::js::string_to_number(value);
  from_js_millis(number)
}

/// `new Date(number)`: whole milliseconds, within JavaScript's date range.
pub fn from_js_millis(number: f64) -> Option<DateTime<Utc>> {
  const MAX: f64 = 8.64e15;
  if !number.is_finite() || number.abs() > MAX {
    return None;
  }
  DateTime::from_timestamp_millis(number.trunc() as i64)
}

/// zod's `z.coerce.date()` for the date strings the API documents: an ISO date
/// (`2026-11-06`, UTC midnight as in JavaScript) or date-time, with or without
/// an offset (without one, the server's zone, UTC).
pub fn parse_date(value: &str) -> Option<DateTime<Utc>> {
  if let Ok(date) = DateTime::parse_from_rfc3339(value) {
    return Some(date.with_timezone(&Utc));
  }
  for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
    if let Ok(date) = chrono::NaiveDateTime::parse_from_str(value, format) {
      return Some(Utc.from_utc_datetime(&date));
    }
  }
  NaiveDate::parse_from_str(value, "%Y-%m-%d").ok().map(utc_date)
}

/// Time zone names Postgres knows (lowercased), loaded once. Requests may only
/// use these, so every accepted zone works in `at time zone`.
static TIME_ZONES: OnceCell<HashSet<String>> = OnceCell::const_new();

/// Upstream's `timezoneParam`: a known IANA time zone (any case), normalized as
/// upstream does. `None` if unknown.
pub async fn time_zone(db: &PgPool, value: &str) -> sqlx::Result<Option<String>> {
  let zones = TIME_ZONES
    .get_or_try_init(|| {
      async {
        let names: Vec<String> = sqlx::query_scalar("select lower(name) from pg_timezone_names")
          .fetch_all(db)
          .await?;
        Ok::<_, sqlx::Error>(names.into_iter().collect())
      }
    })
    .await?;

  let normalized = match value {
    "Asia/Calcutta" => "Asia/Kolkata",
    other => other,
  };

  Ok(
    zones
      .contains(&normalized.to_lowercase())
      .then(|| normalized.to_owned()),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  fn at(value: &str) -> DateTime<Utc> {
    value.parse().unwrap()
  }

  #[test]
  fn picks_minimum_units_like_upstream() {
    let start = at("2026-01-01T00:00:00Z");
    let unit = |end: &str| minimum_unit(start, at(end), false);

    assert_eq!(unit("2026-01-01T01:00:00Z"), Unit::Minute);
    // Whole minutes: 60m59s is still 60 minutes.
    assert_eq!(unit("2026-01-01T01:00:59Z"), Unit::Minute);
    assert_eq!(unit("2026-01-01T01:01:00Z"), Unit::Hour);
    assert_eq!(unit("2026-01-31T23:59:59Z"), Unit::Hour);
    assert_eq!(unit("2026-02-01T00:00:00Z"), Unit::Day);
    assert_eq!(unit("2026-08-31T00:00:00Z"), Unit::Day);
    assert_eq!(unit("2026-09-01T00:00:00Z"), Unit::Month);
    assert_eq!(unit("2028-01-31T00:00:00Z"), Unit::Month);
    assert_eq!(unit("2028-02-01T00:00:00Z"), Unit::Year);
  }

  #[test]
  fn keeps_requested_units_the_range_allows() {
    let start = at("2026-01-01T00:00:00Z");
    let day = at("2026-01-02T00:00:00Z");
    let years = at("2029-01-01T00:00:00Z");

    assert_eq!(request_unit(Some(Unit::Day), start, day), Unit::Day);
    assert_eq!(request_unit(Some(Unit::Minute), start, day), Unit::Hour);
    assert_eq!(request_unit(None, start, day), Unit::Hour);
    assert_eq!(request_unit(Some(Unit::Month), start, years), Unit::Month);
    assert_eq!(request_unit(Some(Unit::Day), start, years), Unit::Year);
  }

  #[test]
  fn compares_like_date_fns() {
    let (start, end) = (at("2026-03-01T00:00:00Z"), at("2026-03-01T00:30:30Z"));
    assert_eq!(
      Compare::Prev.range(start, end),
      (at("2026-02-28T23:30:00Z"), at("2026-03-01T00:00:30Z"))
    );
    assert_eq!(
      Compare::Yoy.range(at("2028-02-29T10:00:00Z"), end),
      (at("2027-02-28T10:00:00Z"), at("2025-03-01T00:30:30Z"))
    );
  }

  #[test]
  fn parses_cohort_date_ranges() {
    let now = at("2026-11-06T13:45:30Z"); // a Friday

    assert_eq!(
      parse_date_range("7day", now),
      Some((at("2026-10-30T00:00:00Z"), at("2026-11-06T23:59:59.999Z")))
    );
    assert_eq!(
      parse_date_range("0week", now),
      Some((at("2026-11-01T00:00:00Z"), at("2026-11-07T23:59:59.999Z")))
    );
    assert_eq!(
      parse_date_range("1month", now),
      Some((at("2026-10-01T00:00:00Z"), at("2026-11-30T23:59:59.999Z")))
    );
    assert_eq!(
      parse_date_range("24hour", now),
      Some((at("2026-11-05T13:00:00Z"), at("2026-11-06T13:59:59.999Z")))
    );
    assert_eq!(
      parse_date_range("range:1000:2000", now),
      Some((at("1970-01-01T00:00:01Z"), at("1970-01-01T00:00:02Z")))
    );
    assert_eq!(parse_date_range("soon", now), None);
    assert_eq!(parse_date_range("7days", now), None);
  }

  #[test]
  fn parses_dates_like_javascript() {
    assert_eq!(parse_date("2026-11-06"), Some(at("2026-11-06T00:00:00Z")));
    assert_eq!(parse_date("2026-11-06T10:00"), Some(at("2026-11-06T10:00:00Z")));
    assert_eq!(
      parse_date("2026-11-06T10:00:00+07:00"),
      Some(at("2026-11-06T03:00:00Z"))
    );
    assert_eq!(parse_date("tomorrow"), None);
    assert_eq!(millis("1000.9"), Some(at("1970-01-01T00:00:01Z")));
    assert_eq!(millis("abc"), None);
    assert_eq!(millis("9e15"), None);
  }
}
