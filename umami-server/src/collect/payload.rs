//! The `/api/send` request body and its validation, mirroring upstream's zod schema.
//!
//! Deliberately lenient where zod is strict but the result is unambiguous: an
//! explicit JSON `null` is treated like an absent field.

use serde_json::{
  Map,
  Value,
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
  Event,
  Identify,
  Performance,
}

/// What the event is recorded against: exactly one of these is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
  Website(Uuid),
  Link(Uuid),
  Pixel(Uuid),
}

impl Source {
  pub fn id(self) -> Uuid {
    match self {
      Self::Website(id) | Self::Link(id) | Self::Pixel(id) => id,
    }
  }

  pub fn website(self) -> Option<Uuid> {
    match self {
      Self::Website(id) => Some(id),
      _ => None,
    }
  }
}

#[derive(Debug)]
pub struct SendBody {
  pub kind: Kind,
  pub payload: Payload,
}

#[derive(Debug, Default)]
pub struct Payload {
  pub source: Option<Source>,
  /// The source ID exactly as sent: upstream hashes and echoes this string, so
  /// its case matters for session IDs.
  pub source_text: String,
  pub data: Option<Map<String, Value>>,
  pub hostname: Option<String>,
  pub language: Option<String>,
  pub referrer: Option<String>,
  pub screen: Option<String>,
  pub title: Option<String>,
  pub url: Option<String>,
  pub name: Option<String>,
  pub tag: Option<String>,
  pub ip: Option<String>,
  pub user_agent: Option<String>,
  /// Unix seconds.
  pub timestamp: Option<i64>,
  pub id: Option<String>,
  pub browser: Option<String>,
  pub os: Option<String>,
  pub device: Option<String>,
  pub lcp: Option<f64>,
  pub inp: Option<f64>,
  pub cls: Option<f64>,
  pub fcp: Option<f64>,
  pub ttfb: Option<f64>,
}

/// Why a body was rejected (logged; the response is upstream's generic 400).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Invalid(String);

fn invalid(message: impl Into<String>) -> Invalid {
  Invalid(message.into())
}

impl SendBody {
  pub fn parse(body: &Value) -> Result<Self, Invalid> {
    let body = body.as_object().ok_or_else(|| invalid("body must be an object"))?;

    let kind = match body.get("type").and_then(Value::as_str) {
      Some("event") => Kind::Event,
      Some("identify") => Kind::Identify,
      Some("performance") => Kind::Performance,
      _ => return Err(invalid("type must be event, identify or performance")),
    };

    let payload = body
      .get("payload")
      .and_then(Value::as_object)
      .ok_or_else(|| invalid("payload must be an object"))?;

    Ok(Self {
      kind,
      payload: Payload::parse(payload)?,
    })
  }
}

impl Payload {
  fn parse(fields: &Map<String, Value>) -> Result<Self, Invalid> {
    let field = |name: &str| fields.get(name).filter(|value| !value.is_null());

    let string = |name: &str| -> Result<Option<String>, Invalid> {
      match field(name) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(invalid(format!("{name} must be a string"))),
      }
    };

    // Rejects spreadsheet formula triggers (CSV injection), as upstream does.
    let safe_string = |name: &str| -> Result<Option<String>, Invalid> {
      let value = string(name)?;
      if value
        .as_deref()
        .is_some_and(|s| s.starts_with(['=', '+', '-', '@', '\t', '\r']))
      {
        return Err(invalid(format!("{name} must not start with a formula character")));
      }
      Ok(value)
    };

    let url_or_path = |name: &str| -> Result<Option<String>, Invalid> {
      let value = string(name)?;
      if let Some(s) = &value {
        url::Url::options()
          .base_url(Some(&localhost()))
          .parse(s)
          .map_err(|_| invalid(format!("{name} must be a URL or path")))?;
      }
      Ok(value)
    };

    let uuid = |name: &str| -> Result<Option<Uuid>, Invalid> {
      string(name)?
        .map(|s| crate::zod::uuid(&s).ok_or_else(|| invalid(format!("{name} must be a UUID"))))
        .transpose()
    };

    let vital = |name: &str, max: f64| -> Result<Option<f64>, Invalid> {
      match field(name) {
        None => Ok(None),
        Some(Value::Number(n)) => {
          match n.as_f64() {
            Some(x) if (0.0..=max).contains(&x) => Ok(Some(x)),
            _ => Err(invalid(format!("{name} must be between 0 and {max}"))),
          }
        }
        Some(_) => Err(invalid(format!("{name} must be a number"))),
      }
    };

    let sources = [
      uuid("website")?.map(Source::Website),
      uuid("link")?.map(Source::Link),
      uuid("pixel")?.map(Source::Pixel),
    ];
    let mut given = sources.into_iter().flatten();
    let source = given.next();
    if source.is_none() || given.next().is_some() {
      return Err(invalid("exactly one of website, link, or pixel must be provided"));
    }

    let source_text = match source {
      Some(Source::Website(_)) => string("website")?,
      Some(Source::Link(_)) => string("link")?,
      Some(Source::Pixel(_)) => string("pixel")?,
      None => None,
    }
    .unwrap_or_default();

    let data = match field("data") {
      None => None,
      Some(Value::Object(map)) => Some(map.clone()),
      Some(_) => return Err(invalid("data must be an object")),
    };

    Ok(Self {
      source,
      source_text,
      data,
      hostname: string("hostname")?,
      language: string("language")?,
      referrer: url_or_path("referrer")?,
      screen: string("screen")?,
      title: string("title")?,
      url: url_or_path("url")?,
      name: safe_string("name")?,
      tag: safe_string("tag")?,
      ip: string("ip")?,
      user_agent: string("userAgent")?,
      timestamp: field("timestamp").map(coerce_int).transpose()?,
      id: string("id")?,
      browser: string("browser")?,
      os: string("os")?,
      device: string("device")?,
      lcp: vital("lcp", 60_000.0)?,
      inp: vital("inp", 60_000.0)?,
      cls: vital("cls", 100.0)?,
      fcp: vital("fcp", 60_000.0)?,
      ttfb: vital("ttfb", 60_000.0)?,
    })
  }
}

pub fn localhost() -> url::Url {
  url::Url::parse("https://localhost").expect("valid URL")
}

/// `z.coerce.number().int()`: `Number(value)` must be a safe integer.
fn coerce_int(value: &Value) -> Result<i64, Invalid> {
  let number = match value {
    Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
    Value::String(s) => crate::js::string_to_number(s),
    Value::Bool(b) => f64::from(u8::from(*b)),
    _ => f64::NAN,
  };

  const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
  if number.fract() == 0.0 && number.abs() <= MAX_SAFE {
    Ok(number as i64)
  } else {
    Err(invalid("timestamp must be an integer"))
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  const WEBSITE: &str = "0d8e3a5c-1f2b-4c3d-9e4f-5a6b7c8d9e0f";

  fn parse(body: Value) -> Result<SendBody, Invalid> {
    SendBody::parse(&body)
  }

  #[test]
  fn parses_a_pageview() {
    let body = parse(json!({
        "type": "event",
        "payload": {
            "website": WEBSITE,
            "hostname": "example.com",
            "url": "/docs?x=1",
            "referrer": "https://google.com/",
            "screen": "1920x1080",
            "language": "en-US",
            "title": "Docs",
            "unknown": "ignored",
        }
    }))
    .unwrap();

    assert_eq!(body.kind, Kind::Event);
    assert_eq!(body.payload.source, Some(Source::Website(WEBSITE.parse().unwrap())));
    assert_eq!(body.payload.url.as_deref(), Some("/docs?x=1"));
    assert_eq!(body.payload.title.as_deref(), Some("Docs"));
  }

  #[test]
  fn requires_exactly_one_source() {
    assert!(parse(json!({ "type": "event", "payload": {} })).is_err());
    assert!(parse(json!({ "type": "event", "payload": { "website": WEBSITE, "pixel": WEBSITE } })).is_err());
    assert!(parse(json!({ "type": "event", "payload": { "link": WEBSITE } })).is_ok());
  }

  #[test]
  fn validates_types() {
    let with = |extra: Value| {
      let mut payload = json!({ "website": WEBSITE });
      payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
      parse(json!({ "type": "event", "payload": payload }))
    };

    assert!(parse(json!({ "type": "record", "payload": { "website": WEBSITE } })).is_err());
    assert!(parse(json!([])).is_err());
    assert!(with(json!({ "website": "not-a-uuid" })).is_err());
    assert!(with(json!({ "title": 5 })).is_err());
    assert!(with(json!({ "data": [1, 2] })).is_err());
    assert!(with(json!({ "name": "=SUM(A1)" })).is_err());
    assert!(with(json!({ "tag": "-x" })).is_err());
    assert!(with(json!({ "url": "https://exa mple.com" })).is_err());
    assert!(with(json!({ "lcp": -1 })).is_err());
    assert!(with(json!({ "cls": 101 })).is_err());
    assert!(with(json!({ "timestamp": 1.5 })).is_err());
    assert!(with(json!({ "timestamp": "abc" })).is_err());

    assert!(with(json!({ "title": null, "data": {}, "name": "signup" })).is_ok());
  }

  #[test]
  fn coerces_timestamps_like_zod() {
    assert_eq!(coerce_int(&json!(1700000000)).unwrap(), 1700000000);
    assert_eq!(coerce_int(&json!("1700000000")).unwrap(), 1700000000);
    assert_eq!(coerce_int(&json!(" 12 ")).unwrap(), 12);
    assert_eq!(coerce_int(&json!(true)).unwrap(), 1);
    assert_eq!(coerce_int(&json!(5.0)).unwrap(), 5);
    assert!(coerce_int(&json!(1e300)).is_err());
  }
}
