//! Client geolocation: port of upstream's `getLocation` (`src/lib/detect.ts`).

use std::net::IpAddr;
use std::path::Path;

use axum::http::HeaderMap;
use maxminddb::{Reader, geoip2};

use super::client;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Location {
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
}

/// CDN geolocation headers (country, region, city), in upstream's order.
const PROVIDER_HEADERS: &[[&str; 3]] = &[
    ["cf-ipcountry", "cf-region-code", "cf-ipcity"],
    [
        "x-vercel-ip-country",
        "x-vercel-ip-country-region",
        "x-vercel-ip-city",
    ],
    [
        "cloudfront-viewer-country",
        "cloudfront-viewer-country-region",
        "cloudfront-viewer-city",
    ],
    ["eo-ipcountry", "eo-region-code", "eo-ipcity"],
];

/// A MaxMind GeoLite2/GeoIP2 City database.
pub struct GeoDatabase(Reader<Vec<u8>>);

impl GeoDatabase {
    pub fn open(path: &Path) -> Result<Self, maxminddb::MaxMindDbError> {
        Reader::open_readfile(path).map(Self)
    }

    fn lookup(&self, addr: IpAddr) -> Option<Location> {
        let city: geoip2::City<'_> = self.0.lookup(addr).ok()?.decode().ok()??;

        let country = city
            .country
            .iso_code
            .or(city.registered_country.iso_code)
            .map(str::to_owned);
        let region = city.subdivisions.first().and_then(|s| s.iso_code);

        Some(Location {
            region: region_code(country.as_deref(), region),
            city: city.city.names.english.map(str::to_owned),
            country,
        })
    }
}

/// Upstream's `getLocation(ip, headers, skipHeaders)`.
pub fn locate(
    ip: Option<&str>,
    headers: &HeaderMap,
    use_headers: bool,
    database: Option<&GeoDatabase>,
) -> Option<Location> {
    let addr: IpAddr = client::strip_port(ip?).parse().ok()?;

    if client::is_local(addr) {
        return None;
    }

    if use_headers {
        // Upstream re-decodes these Latin-1 header strings as UTF-8 (`decodeHeader`),
        // which is reading the raw bytes as UTF-8.
        let header = |name: &str| {
            headers
                .get(name)
                .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        };

        for [country, region, city] in PROVIDER_HEADERS {
            if let Some(country) = header(country).filter(|c| !c.is_empty()) {
                let region = header(region);

                return Some(Location {
                    region: region_code(Some(&country), region.as_deref()),
                    city: header(city),
                    country: Some(country),
                });
            }
        }
    }

    database?.lookup(addr)
}

/// Upstream's `getRegionCode`: ISO 3166-2 style `<country>-<region>`.
fn region_code(country: Option<&str>, region: Option<&str>) -> Option<String> {
    match (country, region) {
        (Some(country), Some(region)) if !country.is_empty() && !region.is_empty() => {
            Some(if region.contains('-') {
                region.to_owned()
            } else {
                format!("{country}-{region}")
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &[u8])]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    name.parse().unwrap(),
                    HeaderValue::from_bytes(value).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn reads_cdn_headers() {
        let headers = headers(&[
            ("cf-ipcountry", b"US"),
            ("cf-region-code", b"CA"),
            ("cf-ipcity", "San José".as_bytes()),
        ]);

        assert_eq!(
            locate(Some("8.8.8.8"), &headers, true, None),
            Some(Location {
                country: Some("US".into()),
                region: Some("US-CA".into()),
                city: Some("San José".into()),
            })
        );
        assert_eq!(locate(Some("8.8.8.8"), &headers, false, None), None);
    }

    #[test]
    fn first_provider_with_a_country_wins() {
        let headers = headers(&[
            ("x-vercel-ip-country", b"DE"),
            ("x-vercel-ip-country-region", b"DE-BE"),
            ("cloudfront-viewer-country", b"FR"),
        ]);
        let location = locate(Some("8.8.8.8"), &headers, true, None).unwrap();

        assert_eq!(location.country.as_deref(), Some("DE"));
        assert_eq!(location.region.as_deref(), Some("DE-BE"));
        assert_eq!(location.city, None);
    }

    #[test]
    fn skips_local_and_invalid_addresses() {
        let headers = headers(&[("cf-ipcountry", b"US")]);

        assert_eq!(locate(Some("192.168.1.10"), &headers, true, None), None);
        assert_eq!(locate(Some("not-an-ip"), &headers, true, None), None);
        assert_eq!(locate(None, &headers, true, None), None);
        assert!(locate(Some("8.8.8.8:443"), &headers, true, None).is_some());
    }
}
