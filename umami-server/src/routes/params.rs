//! Query-string helpers mirroring upstream's `parseRequest` for GET routes and
//! the `pagingParams`, `searchParams` and `sortingParams` schemas.

use std::collections::HashMap;

use crate::error::{
  AppError,
  AppResult,
};
use crate::queries::Filters;
use crate::zod;

/// The query string as upstream reads it (`Object.fromEntries(url.searchParams)`):
/// the last value of a repeated key wins.
pub struct Params(HashMap<String, String>);

/// Which of upstream's shared parameter groups a route's schema includes
/// (`pagingParams` always). Parameters outside the schema are ignored.
#[derive(Clone, Copy)]
pub struct Schema {
  pub search: bool,
  pub sorting: bool,
}

pub const PAGING: Schema = Schema {
  search: false,
  sorting: false,
};
pub const PAGING_SORTING: Schema = Schema {
  search: false,
  sorting: true,
};
pub const PAGING_SEARCH: Schema = Schema {
  search: true,
  sorting: false,
};
pub const PAGING_SEARCH_SORTING: Schema = Schema {
  search: true,
  sorting: true,
};

impl Params {
  pub fn parse(query: Option<&str>) -> Self {
    let pairs = url::form_urlencoded::parse(query.unwrap_or_default().as_bytes());

    Self(pairs.into_owned().collect())
  }

  pub fn get(&self, key: &str) -> Option<&str> {
    self.0.get(key).map(String::as_str)
  }

  /// Every parameter, in no particular order.
  pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
    self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
  }

  /// Validates the shared parameters (a 400 on failure) and returns them as
  /// list filters.
  pub fn filters(&self, schema: Schema) -> AppResult<Filters> {
    let positive = |key: &str| -> AppResult<Option<i64>> {
      self
        .get(key)
        .map(|value| zod::positive_int(value).ok_or_else(AppError::bad_request))
        .transpose()
    };

    let page = positive("page")?;
    let page_size = positive("pageSize")?;
    positive("maxResults")?;

    let (order_by, sort_descending) = if schema.sorting {
      let descending = match self.get("sortDescending") {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => return Err(AppError::bad_request()),
      };
      (self.get("orderBy").map(str::to_owned), descending)
    } else {
      (None, None)
    };

    Ok(Filters {
      page,
      page_size,
      order_by,
      sort_descending,
      search: schema.search.then(|| self.get("search").map(str::to_owned)).flatten(),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn last_value_wins() {
    let params = Params::parse(Some("page=1&page=3&search=a+b%21"));

    assert_eq!(params.get("page"), Some("3"));
    assert_eq!(params.get("search"), Some("a b!"));
  }

  #[test]
  fn validates_like_zod() {
    let filters = |query: &str, schema| Params::parse(Some(query)).filters(schema);

    let parsed = filters(
      "page=2&pageSize=5&orderBy=name&sortDescending=true&search=x",
      PAGING_SEARCH_SORTING,
    )
    .unwrap();
    assert_eq!(parsed.page, Some(2));
    assert_eq!(parsed.page_size, Some(5));
    assert_eq!(parsed.order_by.as_deref(), Some("name"));
    assert_eq!(parsed.sort_descending, Some(true));
    assert_eq!(parsed.search.as_deref(), Some("x"));

    for invalid in ["page=0", "pageSize=-1", "maxResults=x", "page="] {
      assert!(filters(invalid, PAGING_SEARCH).is_err(), "{invalid}");
    }
    assert!(filters("sortDescending=yes", PAGING_SORTING).is_err());

    // Parameters outside the schema are stripped, not validated.
    let unsorted = filters("sortDescending=yes&orderBy=name", PAGING_SEARCH).unwrap();
    assert_eq!(unsorted.order_by, None);
    let unsearched = filters("search=x", PAGING_SORTING).unwrap();
    assert_eq!(unsearched.search, None);
  }
}
