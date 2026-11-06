//! Request filters, ported from upstream's `getQueryFilters` (`src/lib/request.ts`)
//! and `src/lib/params.ts`.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::date::{self, Compare, Unit};
use crate::error::{AppError, AppResult};
use crate::routes::params::Params;
use crate::zod;

/// Upstream's `EVENT_COLUMNS`: dimensions of events.
pub const EVENT_COLUMNS: &[&str] = &[
    "path",
    "fullPath",
    "entry",
    "exit",
    "referrer",
    "domain",
    "title",
    "query",
    "event",
    "tag",
    "hostname",
    "utmSource",
    "utmMedium",
    "utmCampaign",
    "utmContent",
    "utmTerm",
];

/// Upstream's `SESSION_COLUMNS`: dimensions of sessions.
pub const SESSION_COLUMNS: &[&str] = &[
    "browser",
    "os",
    "device",
    "screen",
    "language",
    "country",
    "city",
    "region",
    "distinctId",
];

/// Upstream's `FILTER_COLUMNS`: filter names to their columns.
pub fn filter_column(name: &str) -> Option<&'static str> {
    Some(match name {
        "path" | "entry" | "exit" => "url_path",
        "referrer" | "domain" => "referrer_domain",
        "hostname" => "hostname",
        "distinctId" => "distinct_id",
        "title" => "page_title",
        "query" => "url_query",
        "os" => "os",
        "browser" => "browser",
        "device" => "device",
        "country" => "country",
        "region" => "region",
        "city" => "city",
        "language" => "language",
        "event" => "event_name",
        "tag" => "tag",
        "eventType" => "event_type",
        "utmSource" => "utm_source",
        "utmMedium" => "utm_medium",
        "utmCampaign" => "utm_campaign",
        "utmContent" => "utm_content",
        "utmTerm" => "utm_term",
        _ => return None,
    })
}

/// The filters upstream's `filterParams` schema names (others must carry a
/// numeric suffix, like `browser1`, to be kept).
const FILTER_PARAMS: &[&str] = &[
    "path",
    "referrer",
    "title",
    "query",
    "os",
    "browser",
    "device",
    "country",
    "region",
    "city",
    "tag",
    "hostname",
    "distinctId",
    "language",
    "event",
    "utmSource",
    "utmMedium",
    "utmCampaign",
    "utmContent",
    "utmTerm",
    "eventType",
];

/// The name without a numeric suffix (`browser1` → `browser`).
pub fn base_name(key: &str) -> &str {
    key.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// Upstream's `OPERATORS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator {
    Equals,
    NotEquals,
    Set,
    NotSet,
    Contains,
    DoesNotContain,
    Regex,
    NotRegex,
    True,
    False,
    GreaterThan,
    LessThan,
    GreaterThanEquals,
    LessThanEquals,
    Before,
    After,
}

impl Operator {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "eq" => Self::Equals,
            "neq" => Self::NotEquals,
            "s" => Self::Set,
            "ns" => Self::NotSet,
            "c" => Self::Contains,
            "dnc" => Self::DoesNotContain,
            "re" => Self::Regex,
            "nre" => Self::NotRegex,
            "t" => Self::True,
            "f" => Self::False,
            "gt" => Self::GreaterThan,
            "lt" => Self::LessThan,
            "gte" => Self::GreaterThanEquals,
            "lte" => Self::LessThanEquals,
            "bf" => Self::Before,
            "af" => Self::After,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Equals => "eq",
            Self::NotEquals => "neq",
            Self::Set => "s",
            Self::NotSet => "ns",
            Self::Contains => "c",
            Self::DoesNotContain => "dnc",
            Self::Regex => "re",
            Self::NotRegex => "nre",
            Self::True => "t",
            Self::False => "f",
            Self::GreaterThan => "gt",
            Self::LessThan => "lt",
            Self::GreaterThanEquals => "gte",
            Self::LessThanEquals => "lte",
            Self::Before => "bf",
            Self::After => "af",
        }
    }
}

/// A filter as upstream's filters object holds it: an `operator.value` string
/// (or a bare value, meaning equals), or the numeric `eventType`.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterValue {
    Text(String),
    Number(i64),
}

/// The values a column filter compares with.
#[derive(Clone, Debug, PartialEq)]
pub enum Values {
    /// Equality operators: any of these.
    List(Vec<String>),
    Numbers(Vec<i64>),
    /// Search and regex operators: one pattern.
    One(String),
}

/// A column filter, parsed (upstream's `filtersObjectToArray` entries).
#[derive(Clone, Debug)]
pub struct ColumnFilter {
    /// The key it was given under (`browser1`), which names its SQL parameter.
    pub key: String,
    /// The filter name (`browser`).
    pub name: String,
    pub operator: Operator,
    pub values: Values,
}

impl ColumnFilter {
    /// Upstream's `parseFilterValue`. Operators that have no meaning for a column
    /// are a 400 (upstream emits broken SQL and fails with a 500).
    fn parse(key: &str, value: &FilterValue) -> AppResult<Self> {
        let name = base_name(key).to_owned();
        let (operator, values) = match value {
            FilterValue::Number(n) => (Operator::Equals, Values::Numbers(vec![*n])),
            FilterValue::Text(text) => {
                let (operator, rest) = split_operator(text);
                let values = match operator {
                    Operator::Equals | Operator::NotEquals => {
                        Values::List(rest.split(',').map(str::to_owned).collect())
                    }
                    Operator::Contains
                    | Operator::DoesNotContain
                    | Operator::Regex
                    | Operator::NotRegex => Values::One(rest.to_owned()),
                    _ => return Err(AppError::bad_request()),
                };
                (operator, values)
            }
        };

        Ok(Self {
            key: key.to_owned(),
            name,
            operator,
            values,
        })
    }
}

/// `operator.value`, or the whole text as an equality value.
fn split_operator(text: &str) -> (Operator, &str) {
    if let Some((operator, rest)) = text.split_once('.')
        && let Some(operator) = Operator::parse(operator)
        // JS `(.*)` stops at line breaks; such values are taken literally.
        && !rest.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
    {
        return (operator, rest);
    }
    (Operator::Equals, text)
}

/// Upstream's `DATA_TYPE` values a property filter may name.
const DATA_TYPES: std::ops::RangeInclusive<i32> = 1..=5;
pub const DATA_TYPE_NUMBER: i32 = 2;
pub const DATA_TYPE_DATE: i32 = 4;
pub const DATA_TYPE_ARRAY: i32 = 5;

/// An event- or session-data filter (`epfN`/`spfN`, or a segment's).
#[derive(Clone, Debug)]
pub struct PropertyFilter {
    pub name: String,
    pub data_type: i32,
    pub operator: Operator,
    pub value: String,
}

impl PropertyFilter {
    /// `dataType.operator.encodedName.value`; malformed values are ignored, as
    /// upstream does.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.splitn(4, '.');
        let data_type = parts.next()?;
        let operator = parts.next()?;
        let name = parts.next()?;
        let value = parts.next()?;

        if data_type.is_empty() || !data_type.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let data_type: i32 = data_type.parse().ok().filter(|t| DATA_TYPES.contains(t))?;
        if operator.is_empty() || name.is_empty() || value.contains(['\n', '\r']) {
            return None;
        }

        Some(Self {
            name: crate::collect::js::safe_decode_uri_component(name).into_owned(),
            data_type,
            operator: Operator::parse(operator)?,
            value: value.to_owned(),
        })
    }

    /// A filter stored in a segment's parameters.
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            name: value["propertyName"].as_str()?.to_owned(),
            data_type: value["dataType"]
                .as_i64()
                .and_then(|t| i32::try_from(t).ok())
                .filter(|t| DATA_TYPES.contains(t))?,
            operator: Operator::parse(value["operator"].as_str()?)?,
            value: match &value["value"] {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            },
        })
    }
}

/// A cohort: the sessions that did something within a date range.
#[derive(Clone, Debug)]
pub struct Cohort {
    /// Filters on those sessions' events, keyed `cohort_<name>`.
    pub filters: Vec<(String, FilterValue)>,
    /// The key of the action every cohort member performed (always required).
    pub action_key: String,
    pub match_any: bool,
    pub start_date: DateTime<Utc>,
    pub end_date: DateTime<Utc>,
}

impl Cohort {
    pub fn column_filters(&self) -> AppResult<Vec<ColumnFilter>> {
        self.filters
            .iter()
            .map(|(key, value)| ColumnFilter::parse(key, value))
            .collect()
    }
}

/// Upstream's `QueryFilters` for analytics queries.
#[derive(Clone, Debug)]
pub struct QueryFilters {
    pub start_date: DateTime<Utc>,
    pub end_date: DateTime<Utc>,
    /// A validated time zone, if one was given.
    pub timezone: Option<String>,
    pub unit: Unit,
    /// Column filters by key, in the order given.
    entries: Vec<(String, FilterValue)>,
    pub match_any: bool,
    pub event_property_filters: Vec<PropertyFilter>,
    pub session_property_filters: Vec<PropertyFilter>,
    pub cohort: Option<Cohort>,
    pub exclude_bounce: bool,
    pub search: Option<String>,
    pub compare: Option<Compare>,
    /// `pagingParams`, for lists.
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    pub max_results: Option<i64>,
    /// The segment and cohort to apply, resolved by [`QueryFilters::for_website`].
    segment: Option<Uuid>,
    cohort_id: Option<Uuid>,
}

impl QueryFilters {
    /// Adds or replaces the filter under `key`.
    pub fn set(&mut self, key: &str, value: FilterValue) {
        match self.entries.iter_mut().find(|(k, _)| k == key) {
            Some((_, existing)) => *existing = value,
            None => self.entries.push((key.to_owned(), value)),
        }
    }

    /// The filter names given (without numeric suffixes).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| base_name(key))
    }

    /// Whether any event dimension (or event data) is filtered.
    pub fn has_event_filters(&self) -> bool {
        self.names().any(|name| EVENT_COLUMNS.contains(&name))
            || !self.event_property_filters.is_empty()
    }

    /// The column filters, parsed.
    pub fn column_filters(&self) -> AppResult<Vec<ColumnFilter>> {
        self.entries
            .iter()
            .filter(|(key, _)| filter_column(base_name(key)).is_some())
            .map(|(key, value)| ColumnFilter::parse(key, value))
            .collect()
    }

    /// The same filters over another period.
    pub fn with_range(&self, start_date: DateTime<Utc>, end_date: DateTime<Utc>) -> Self {
        Self {
            start_date,
            end_date,
            ..self.clone()
        }
    }
}

/// Which optional parameter groups a route's schema has.
#[derive(Clone, Copy, Default)]
pub struct Options {
    /// `searchParams`.
    pub search: bool,
    /// `filterParams` (column filters, segments, cohorts and the like).
    pub filters: bool,
    /// `pagingParams`.
    pub paging: bool,
    /// How the date range is given.
    pub range: Range,
    /// Whether `timezone` is required.
    pub timezone_required: bool,
}

impl Options {
    /// A date range only.
    pub const NONE: Options = Options {
        search: false,
        filters: false,
        paging: false,
        range: Range::Dates,
        timezone_required: false,
    };
}

/// How a route takes its date range.
#[derive(Clone, Copy, Default)]
pub enum Range {
    /// `withDateRange`: `startAt`/`endAt` timestamps or `startDate`/`endDate`.
    #[default]
    Dates,
    /// Required integer `startAt`/`endAt` timestamps.
    Timestamps,
    /// Set by the route.
    Fixed(DateTime<Utc>, DateTime<Utc>),
}

/// Upstream's `withDateRange` schema (with `filterParams` and `searchParams` as
/// the route has them): the date range, time zone, unit and filters. Invalid
/// parameters are a 400. Website-specific filters are applied afterwards, by
/// [`QueryFilters::for_website`], as upstream checks permissions in between.
pub async fn from_request(
    db: &PgPool,
    params: &Params,
    options: Options,
) -> AppResult<QueryFilters> {
    let bad = AppError::bad_request;

    // The date range: timestamps, or else dates (which upstream accepts, then
    // fails on with a 500).
    let timestamp = |key: &str| -> AppResult<Option<DateTime<Utc>>> {
        params
            .get(key)
            .map(|v| date::millis(v).ok_or_else(bad))
            .transpose()
    };
    let date = |key: &str| -> AppResult<Option<DateTime<Utc>>> {
        params
            .get(key)
            .map(|v| date::parse_date(v).ok_or_else(bad))
            .transpose()
    };
    let integer = |key: &str| -> AppResult<DateTime<Utc>> {
        let number = crate::collect::js::string_to_number(params.get(key).ok_or_else(bad)?);
        if number.fract() != 0.0 {
            return Err(bad());
        }
        date::from_js_millis(number).ok_or_else(bad)
    };
    let (start_date, end_date) = match options.range {
        Range::Fixed(start, end) => (start, end),
        Range::Timestamps => (integer("startAt")?, integer("endAt")?),
        Range::Dates => {
            let (start_at, end_at) = (timestamp("startAt")?, timestamp("endAt")?);
            let (start_date, end_date) = (date("startDate")?, date("endDate")?);
            match (start_at, end_at, start_date, end_date) {
                (Some(start), Some(end), _, _) | (_, _, Some(start), Some(end)) => (start, end),
                _ => return Err(bad()),
            }
        }
    };

    let timezone = match params.get("timezone") {
        Some(value) => Some(date::time_zone(db, value).await?.ok_or_else(bad)?),
        None if options.timezone_required => return Err(bad()),
        None => None,
    };
    let positive = |key: &str| -> AppResult<Option<i64>> {
        if !options.paging {
            return Ok(None);
        }
        params
            .get(key)
            .map(|value| zod::positive_int(value).ok_or_else(bad))
            .transpose()
    };
    let (page, page_size, max_results) = (
        positive("page")?,
        positive("pageSize")?,
        positive("maxResults")?,
    );
    let unit = params
        .get("unit")
        .map(|value| Unit::parse(value).ok_or_else(bad))
        .transpose()?;
    let compare = params
        .get("compare")
        .map(|value| Compare::parse(value).ok_or_else(bad))
        .transpose()?;

    let mut filters = QueryFilters {
        unit: date::request_unit(unit, start_date, end_date),
        start_date,
        end_date,
        timezone,
        entries: Vec::new(),
        match_any: false,
        event_property_filters: Vec::new(),
        session_property_filters: Vec::new(),
        cohort: None,
        exclude_bounce: false,
        search: options
            .search
            .then(|| params.get("search").map(str::to_owned))
            .flatten(),
        compare,
        page,
        page_size,
        max_results,
        segment: None,
        cohort_id: None,
    };

    if !options.filters {
        return Ok(filters);
    }

    for (key, value) in params.iter() {
        let name = base_name(key);
        if filter_column(name).is_none() || (key == name && !FILTER_PARAMS.contains(&key)) {
            continue;
        }
        let value = if name == "eventType" {
            match zod::positive_int(value) {
                Some(n) => FilterValue::Number(n),
                None if key == name => return Err(bad()),
                // Suffixed copies bypass upstream's schema and are kept as text.
                None => FilterValue::Text(value.to_owned()),
            }
        } else {
            FilterValue::Text(value.to_owned())
        };
        filters.set(key, value);
    }

    filters.match_any = match params.get("match") {
        None | Some("all") => false,
        Some("any") => true,
        Some(_) => return Err(bad()),
    };

    for (key, value) in params.iter() {
        let is_numbered = |prefix: &str| {
            key.strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        };
        if is_numbered("epf") {
            filters
                .event_property_filters
                .extend(PropertyFilter::parse(value));
        } else if is_numbered("spf") {
            filters
                .session_property_filters
                .extend(PropertyFilter::parse(value));
        }
    }

    let uuid = |key: &str| {
        params
            .get(key)
            .map(|value| zod::uuid(value).ok_or_else(bad))
            .transpose()
    };
    filters.segment = uuid("segment")?;
    filters.cohort_id = uuid("cohort")?;

    // Upstream treats any value as true, including `false`.
    filters.exclude_bounce = params
        .get("excludeBounce")
        .is_some_and(|value| !matches!(value, "" | "false" | "0"));

    Ok(filters)
}

impl QueryFilters {
    /// Upstream's website-specific `getQueryFilters` steps: data before the
    /// website's reset is excluded, and its segment and cohort are applied.
    pub async fn for_website(mut self, db: &PgPool, website_id: Uuid) -> AppResult<Self> {
        let reset_at: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
            "select reset_at from website where website_id = $1 and deleted_at is null",
        )
        .bind(website_id)
        .fetch_optional(db)
        .await?;
        if let Some(Some(reset_at)) = reset_at {
            self.start_date = self.start_date.max(reset_at);
        }

        if let Some(segment) = self.segment {
            let parameters = segment_parameters(db, website_id, segment).await?;
            for (key, value) in filters_array_to_object(&parameters["filters"], "") {
                self.set(&key, value);
            }
            if let Some(extra) = parameters["sessionPropertyFilters"].as_array() {
                self.session_property_filters
                    .extend(extra.iter().filter_map(PropertyFilter::from_json));
            }
            if let Some(matching) = parameters["match"].as_str() {
                self.match_any = matching == "any";
            }
        }

        if let Some(cohort) = self.cohort_id {
            let parameters = segment_parameters(db, website_id, cohort).await?;
            self.cohort = Some(cohort_from(&parameters)?);
        }

        Ok(self)
    }
}

/// The parameters of the website's segment or cohort `id`. Unknown ones are a
/// 400 (upstream fails with a 500).
async fn segment_parameters(db: &PgPool, website_id: Uuid, id: Uuid) -> AppResult<Value> {
    let parameters: Option<Value> = sqlx::query_scalar(
        "select parameters from segment where segment_id = $1 and website_id = $2",
    )
    .bind(id)
    .bind(website_id)
    .fetch_optional(db)
    .await?;

    parameters
        .filter(Value::is_object)
        .ok_or_else(|| AppError::BadRequest("Segment not found.".to_owned()))
}

/// Upstream's `filtersArrayToObject`: `[{ name, operator, value }]` as keyed
/// `operator.value` filters, numbering repeated names (`path`, `path1`).
fn filters_array_to_object(filters: &Value, prefix: &str) -> Vec<(String, FilterValue)> {
    let mut counts = std::collections::HashMap::<String, usize>::new();
    let mut entries = Vec::new();

    for filter in filters.as_array().into_iter().flatten() {
        let Some(name) = filter["name"].as_str() else {
            continue;
        };
        let name = format!("{prefix}{name}");
        let operator = filter["operator"].as_str().unwrap_or("eq");
        let value = match &filter["value"] {
            Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
            other => js_string(other),
        };

        let count = counts.entry(name.clone()).or_default();
        let key = if *count == 0 {
            name.clone()
        } else {
            format!("{name}{count}")
        };
        *count += 1;

        entries.push((key, FilterValue::Text(format!("{operator}.{value}"))));
    }

    entries
}

/// A JSON value as JavaScript's `String()` writes it in a template.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_owned(),
        other => other.to_string(),
    }
}

/// A cohort's filters, action and date range, from its parameters.
fn cohort_from(parameters: &Value) -> AppResult<Cohort> {
    let bad = || AppError::BadRequest("Invalid cohort.".to_owned());
    let range = parameters["dateRange"].as_str().ok_or_else(bad)?;
    let (start_date, end_date) = date::parse_date_range(range, Utc::now()).ok_or_else(bad)?;
    let action_type = parameters["action"]["type"].as_str().ok_or_else(bad)?;

    let mut list = parameters["filters"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    list.push(serde_json::json!({
        "name": action_type,
        "operator": "eq",
        "value": parameters["action"]["value"],
    }));

    Ok(Cohort {
        filters: filters_array_to_object(&Value::Array(list), "cohort_"),
        action_key: format!("cohort_{action_type}"),
        match_any: parameters["match"].as_str() == Some("any"),
        start_date,
        end_date,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_filter_values_like_upstream() {
        let parse = |text: &str| ColumnFilter::parse("browser1", &FilterValue::Text(text.into()));

        let plain = parse("chrome,firefox").unwrap();
        assert_eq!(plain.name, "browser");
        assert_eq!(plain.operator, Operator::Equals);
        assert_eq!(
            plain.values,
            Values::List(vec!["chrome".into(), "firefox".into()])
        );

        let negated = parse("neq.safari").unwrap();
        assert_eq!(negated.operator, Operator::NotEquals);

        let search = parse("c.fox,ie").unwrap();
        assert_eq!(search.values, Values::One("fox,ie".into()));

        // Not an operator: the whole text is the value.
        let dotted = parse("v1.2").unwrap();
        assert_eq!(dotted.values, Values::List(vec!["v1.2".into()]));

        // Operators without meaning for a column.
        assert!(parse("gt.5").is_err());
        assert!(parse("s.x").is_err());
    }

    #[test]
    fn parses_property_filters() {
        let filter = PropertyFilter::parse("2.gte.order%2Etotal.10.5").unwrap();
        assert_eq!(filter.name, "order.total");
        assert_eq!(filter.data_type, 2);
        assert_eq!(filter.operator, Operator::GreaterThanEquals);
        assert_eq!(filter.value, "10.5");

        assert!(PropertyFilter::parse("9.eq.name.x").is_none());
        assert!(PropertyFilter::parse("1.zz.name.x").is_none());
        assert!(PropertyFilter::parse("1.eq.name").is_none());
    }

    #[test]
    fn numbers_repeated_segment_filters() {
        let entries = filters_array_to_object(
            &json!([
                { "name": "path", "operator": "eq", "value": ["/a", "/b"] },
                { "name": "path", "operator": "c", "value": "x" },
                { "name": "browser", "operator": "neq", "value": "ie" },
            ]),
            "",
        );

        assert_eq!(
            entries,
            vec![
                ("path".into(), FilterValue::Text("eq./a,/b".into())),
                ("path1".into(), FilterValue::Text("c.x".into())),
                ("browser".into(), FilterValue::Text("neq.ie".into())),
            ]
        );
    }
}
