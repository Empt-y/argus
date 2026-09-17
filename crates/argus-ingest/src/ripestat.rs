//! Where on Earth an internet number is: the geolocation step that makes
//! BGP spatial.
//!
//! RIPEstat answers two questions keylessly. `geoloc` places an IP prefix —
//! country, city, coordinates, and what fraction of the prefix each place
//! covers — from the RIPE NCC's own geolocation and MaxMind's free tier.
//! `rir-stats-country` gives the country an AS number is registered in.
//! It does not geolocate an AS: asked for one, `geoloc` answers "unsupported
//! type ASN", so an AS-wide event is placed on its registration country by
//! the caller, and says so.
//!
//! Answers are kept in memory for the process's life. The hijack and
//! outage feeds name the same few hundred prefixes and ASNs poll after
//! poll, and RIPEstat's own cache makes a repeat fetch cheap anyway; what
//! matters is not asking twice a minute. RIPEstat asks for a `sourceapp`
//! parameter and no more than about eight requests a second; the client
//! is paced to four.

use crate::http::HttpClient;
use argus_core::source::SourceError;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;

const API: &str = "https://stat.ripe.net/data";
const SOURCEAPP: &str = "argus";
/// Half of what RIPEstat says it will take.
const REQUESTS_PER_SECOND: u32 = 4;
/// The in-memory answers are dropped wholesale past this, rather than
/// managed; a long-running daemon would otherwise hold every prefix a
/// year's hijacks named.
const MAX_REMEMBERED: usize = 50_000;

/// A prefix placed on the map.
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub lon: f64,
    pub lat: f64,
    pub country: String,
    /// Empty when RIPEstat knows the country and no more.
    pub city: String,
    /// How much of the prefix this place covers, 0–100.
    pub covered_percent: f64,
}

pub struct RipeStat {
    http: HttpClient,
    prefixes: Mutex<HashMap<String, Option<Located>>>,
    countries: Mutex<HashMap<u32, Option<String>>>,
}

impl RipeStat {
    pub fn new(http: HttpClient) -> Self {
        Self {
            http: http.with_rate_per_second(REQUESTS_PER_SECOND),
            prefixes: Mutex::new(HashMap::new()),
            countries: Mutex::new(HashMap::new()),
        }
    }

    /// The place covering most of the prefix, or `None` when RIPEstat
    /// cannot place it. A failed request is not remembered, so the next
    /// poll asks again; an honest "unknown" is.
    pub async fn prefix_point(&self, prefix: &str) -> Result<Option<Located>, SourceError> {
        if let Some(known) = self.prefixes.lock().expect("ripestat cache lock").get(prefix) {
            return Ok(known.clone());
        }
        let url = format!("{API}/geoloc/data.json?resource={prefix}&sourceapp={SOURCEAPP}");
        let bytes = self.http.get_bytes(&url).await?;
        let located = decode_geoloc(&bytes)?;
        let mut cache = self.prefixes.lock().expect("ripestat cache lock");
        if cache.len() >= MAX_REMEMBERED {
            cache.clear();
        }
        cache.insert(prefix.to_string(), located.clone());
        Ok(located)
    }

    /// The ISO country an AS number is registered in, per the RIR
    /// statistics files, or `None` when no RIR lists it.
    pub async fn asn_country(&self, asn: u32) -> Result<Option<String>, SourceError> {
        if let Some(known) = self.countries.lock().expect("ripestat cache lock").get(&asn) {
            return Ok(known.clone());
        }
        let url = format!("{API}/rir-stats-country/data.json?resource=AS{asn}&sourceapp={SOURCEAPP}");
        let bytes = self.http.get_bytes(&url).await?;
        let country = decode_rir_country(&bytes)?;
        let mut cache = self.countries.lock().expect("ripestat cache lock");
        if cache.len() >= MAX_REMEMBERED {
            cache.clear();
        }
        cache.insert(asn, country.clone());
        Ok(country)
    }
}

// --- wire format --------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    status: String,
    #[serde(default)]
    messages: Vec<serde_json::Value>,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct GeolocData {
    #[serde(default)]
    located_resources: Vec<LocatedResource>,
}

#[derive(Debug, Deserialize)]
struct LocatedResource {
    #[serde(default)]
    locations: Vec<Location>,
}

#[derive(Debug, Deserialize)]
struct Location {
    #[serde(default)]
    country: String,
    #[serde(default)]
    city: String,
    latitude: Option<f64>,
    longitude: Option<f64>,
    #[serde(default)]
    covered_percentage: f64,
}

pub fn decode_geoloc(bytes: &[u8]) -> Result<Option<Located>, SourceError> {
    let env: Envelope<GeolocData> = serde_json::from_slice(bytes).map_err(|e| SourceError::Decode(format!("RIPEstat geoloc: {e}")))?;
    if env.status != "ok" {
        return Err(SourceError::Decode(format!("RIPEstat geoloc: {}", summarise(&env.messages))));
    }
    let Some(data) = env.data else { return Ok(None) };
    let best = data
        .located_resources
        .iter()
        .flat_map(|r| r.locations.iter())
        .filter(|l| l.latitude.is_some() && l.longitude.is_some())
        .max_by(|a, b| a.covered_percentage.total_cmp(&b.covered_percentage));
    Ok(best.map(|l| Located {
        lon: l.longitude.unwrap_or_default(),
        lat: l.latitude.unwrap_or_default(),
        country: l.country.clone(),
        city: l.city.clone(),
        covered_percent: l.covered_percentage,
    }))
}

#[derive(Debug, Deserialize)]
struct RirCountryData {
    #[serde(default)]
    located_resources: Vec<RirLocated>,
}

#[derive(Debug, Deserialize)]
struct RirLocated {
    #[serde(default)]
    location: String,
}

pub fn decode_rir_country(bytes: &[u8]) -> Result<Option<String>, SourceError> {
    let env: Envelope<RirCountryData> = serde_json::from_slice(bytes).map_err(|e| SourceError::Decode(format!("RIPEstat rir-stats-country: {e}")))?;
    if env.status != "ok" {
        return Err(SourceError::Decode(format!("RIPEstat rir-stats-country: {}", summarise(&env.messages))));
    }
    Ok(env
        .data
        .and_then(|d| d.located_resources.into_iter().next())
        .map(|r| r.location.trim().to_uppercase())
        .filter(|c| c.len() == 2))
}

fn summarise(messages: &[serde_json::Value]) -> String {
    let parts: Vec<String> = messages.iter().map(|m| m.to_string()).collect();
    if parts.is_empty() { "status not ok".into() } else { parts.join("; ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEOLOC: &str = r#"{"messages":[],"see_also":[],"version":"1.0","data_call_name":"geoloc","data_call_status":"supported","cached":false,"status":"ok","status_code":200,"time":"2026-09-17T12:19:03.006315","data":{"located_resources":[{"resource":"193.0.0.0/21","locations":[{"country":"NL","city":"","resources":["193.0.0.0/20"],"latitude":52.3824,"longitude":4.8995,"covered_percentage":100.0}],"unknown_percentage":0}],"unknown_percentage":{"v4":0.0},"resource":"193.0.0.0/21","result_time":"2026-09-17T12:00:00","parameters":{"resource":"193.0.0.0/21"}}}"#;
    const GEOLOC_SPLIT: &str = r#"{"status":"ok","messages":[],"data":{"located_resources":[{"resource":"10.0.0.0/8","locations":[{"country":"US","city":"Ashburn","latitude":39.0,"longitude":-77.5,"covered_percentage":30.0},{"country":"DE","city":"Frankfurt","latitude":50.1,"longitude":8.7,"covered_percentage":70.0},{"country":"XX","city":"","latitude":null,"longitude":null,"covered_percentage":95.0}]}]}}"#;
    const ASN_REFUSED: &str = r#"{"messages":[["error","AS44559 is of unsupported type ASN. It should be one of: IP prefix."]],"see_also":[],"version":"1.0","data_call_name":"geoloc","status":"error","status_code":400,"data":{}}"#;
    const RIR: &str = r#"{"messages":[],"status":"ok","data":{"located_resources":[{"resource":"44559","location":"CY"}],"result_time":"2026-09-16T00:00:00","parameters":{"resource":"44559"}}}"#;

    #[test]
    fn a_prefix_is_placed_where_most_of_it_lives_and_a_placeless_entry_does_not_win() {
        let l = decode_geoloc(GEOLOC.as_bytes()).unwrap().unwrap();
        assert_eq!(l, Located { lon: 4.8995, lat: 52.3824, country: "NL".into(), city: String::new(), covered_percent: 100.0 });
        let l = decode_geoloc(GEOLOC_SPLIT.as_bytes()).unwrap().unwrap();
        assert_eq!(l.city, "Frankfurt", "70% beats 30%, and the 95% with no coordinates is not a place");
    }

    #[test]
    fn ripestats_own_error_is_an_error_not_an_unknown() {
        let err = decode_geoloc(ASN_REFUSED.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("unsupported type ASN"), "{err}");
        assert_eq!(decode_geoloc(br#"{"status":"ok","data":{"located_resources":[]}}"#).unwrap(), None);
    }

    #[test]
    fn an_as_numbers_registration_country_is_two_letters_upper() {
        assert_eq!(decode_rir_country(RIR.as_bytes()).unwrap().as_deref(), Some("CY"));
        assert_eq!(decode_rir_country(br#"{"status":"ok","data":{"located_resources":[]}}"#).unwrap(), None);
        assert_eq!(decode_rir_country(br#"{"status":"ok","data":{"located_resources":[{"resource":"1","location":""}]}}"#).unwrap(), None);
    }
}
