//! Where the DNS root lives: every anycast site of the thirteen root
//! servers, from root-servers.org.
//!
//! The root of the DNS is thirteen named servers, A to M, run by twelve
//! operators, and each is really hundreds of identical instances announced
//! from the same address by anycast, so a query goes to the nearest.
//! root-servers.org keeps the list: 1,573 sites in 2026, each a town, a
//! letter and how many instances are there. The site's `root.json` is
//! gone (404); the data is embedded in `map-data.js` as two JavaScript
//! constants — `const roots = new Map([...])` with the operators and
//! `const sites = [...]` with the places — both valid JSON once the
//! brackets are found, which is what the decoder does rather than run
//! JavaScript. Weekly, as features: the sites change by a handful a
//! month. A town that hosts several letters — F and D both in Abidjan —
//! is several features, one per letter.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;

const URL: &str = "https://root-servers.org/map-data.js";
const CADENCE_SECS: u64 = 7 * 24 * 3600;

pub struct RootServers {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl RootServers {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("root-servers"),
                layer_id: LayerId::new("root-servers"),
                display_name: "DNS root server sites (root-servers.org)".into(),
                kind: EntityKind::Feature,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "Root Server Operators, root-servers.org".into(),
                    url: "https://root-servers.org/".into(),
                    license: "Published by the root server operators for public use".into(),
                    notice: Some("Root server site data from root-servers.org".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for RootServers {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let bytes = self.http.get_bytes(URL).await?;
        let text = std::str::from_utf8(&bytes).map_err(|e| SourceError::Decode(format!("map-data.js is not UTF-8: {e}")))?;
        let decoded = decode(text, Utc::now(), &self.descriptor.id)?;
        tracing::info!(source = %self.descriptor.id, roots = decoded.roots, sites = decoded.observations.len(), unplaced = decoded.unplaced, "root server sites read");
        Ok(decoded.observations)
    }
}

// --- wire format --------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Root {
    #[serde(default)]
    operator: String,
    #[serde(default)]
    ipv4: String,
    #[serde(default)]
    ipv6: String,
    #[serde(default)]
    asn: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Site {
    #[serde(default)]
    root: String,
    #[serde(default)]
    town: String,
    #[serde(default)]
    country: String,
    #[serde(default)]
    ipv4: bool,
    #[serde(default)]
    ipv6: bool,
    #[serde(default)]
    lat: Option<f64>,
    #[serde(default)]
    lon: Option<f64>,
    #[serde(default)]
    instances: u64,
}

#[derive(Debug)]
pub struct Decoded {
    pub observations: Vec<Observation>,
    pub roots: usize,
    pub unplaced: usize,
}

/// The JSON array that follows `const <name> = ` in the script, found by
/// matching brackets rather than trusting a line ending.
fn constant<'a>(text: &'a str, name: &str) -> Result<&'a str, SourceError> {
    let marker = format!("const {name} = ");
    let start = text.find(&marker).ok_or_else(|| SourceError::Decode(format!("map-data.js has no `{marker}`")))?;
    let rest = &text[start + marker.len()..];
    // `new Map([` for the roots, a bare `[` for the sites.
    let open = rest.find('[').ok_or_else(|| SourceError::Decode(format!("`{name}` is not followed by an array")))?;
    let body = &rest[open..];
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, c) in body.char_indices() {
        if in_string {
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => in_string = false,
                _ => escaped = false,
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&body[..=i]);
                }
            }
            _ => {}
        }
    }
    Err(SourceError::Decode(format!("`{name}` array never closes")))
}

pub fn decode(text: &str, now: DateTime<Utc>, source_id: &SourceId) -> Result<Decoded, SourceError> {
    let roots: Vec<(String, Root)> = serde_json::from_str(constant(text, "roots")?).map_err(|e| SourceError::Decode(format!("roots map: {e}")))?;
    let roots: HashMap<String, Root> = roots.into_iter().collect();
    let sites: Vec<Site> = serde_json::from_str(constant(text, "sites")?).map_err(|e| SourceError::Decode(format!("sites array: {e}")))?;
    if sites.is_empty() || roots.is_empty() {
        return Err(SourceError::Decode(format!("{} roots and {} sites", roots.len(), sites.len())));
    }
    // The list repeats a site once per instance in places — J-root in
    // Amsterdam is five rows at one coordinate, one instance each — and
    // elsewhere a letter has several distinct sites in one town. Rows at
    // the same coordinate (to about a hundred metres) merge with their
    // instances summed; distinct sites in one town are numbered.
    let mut merged: Vec<Site> = Vec::with_capacity(sites.len());
    let mut unplaced = 0;
    for s in sites {
        let (Some(lat), Some(lon)) = (s.lat, s.lon) else {
            unplaced += 1;
            continue;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || (lat == 0.0 && lon == 0.0) || s.root.is_empty() {
            unplaced += 1;
            continue;
        }
        // Town names are typed by hand upstream: "Chicago" and "CHICAGO",
        // "Dar es Salaam" and "Dar Es Salaam" are one town.
        let same = |m: &Site| m.root.eq_ignore_ascii_case(&s.root) && m.country == s.country && m.town.eq_ignore_ascii_case(&s.town) && m.lat.is_some_and(|l| (l - lat).abs() < 1e-3) && m.lon.is_some_and(|l| (l - lon).abs() < 1e-3);
        match merged.iter_mut().find(|m| same(m)) {
            Some(m) => {
                m.instances += s.instances.max(1);
                m.ipv4 |= s.ipv4;
                m.ipv6 |= s.ipv6;
            }
            None => merged.push(Site { instances: s.instances.max(1), ..s }),
        }
    }
    let mut seen_in_town: HashMap<(String, String, String), usize> = HashMap::new();
    let mut observations = Vec::with_capacity(merged.len());
    for s in &merged {
        let (lat, lon) = (s.lat.unwrap_or_default(), s.lon.unwrap_or_default());
        let letter = s.root.to_uppercase();
        let root = roots.get(&letter);
        let nth = seen_in_town.entry((letter.clone(), s.country.clone(), s.town.to_lowercase())).or_insert(0);
        *nth += 1;
        let key = match *nth {
            1 => format!("root:{}:{}:{}", letter, s.country, s.town.to_lowercase().replace(' ', "-")),
            n => format!("root:{}:{}:{}:{n}", letter, s.country, s.town.to_lowercase().replace(' ', "-")),
        };
        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("letter", serde_json::json!(letter));
        put("operator", serde_json::json!(root.map(|r| r.operator.clone()).filter(|o| !o.is_empty())));
        put("asn", serde_json::json!(root.and_then(|r| r.asn)));
        put("address_v4", serde_json::json!(root.map(|r| r.ipv4.clone()).filter(|a| !a.is_empty())));
        put("address_v6", serde_json::json!(root.map(|r| r.ipv6.clone()).filter(|a| !a.is_empty())));
        put("town", serde_json::json!(s.town));
        put("country", serde_json::json!(s.country));
        put("instances", serde_json::json!(s.instances));
        put("ipv4", serde_json::json!(s.ipv4));
        put("ipv6", serde_json::json!(s.ipv6));
        put("url", serde_json::json!(format!("https://root-servers.org/root/{}.html", letter)));
        observations.push(
            Observation::new(source_id.clone(), EntityId::new(EntityKind::Feature, key), now, Quality::Live)
                .with_position(Position { lon, lat, alt_m: None, datum: AltitudeDatum::Geoid })
                .with_label(format!("{letter}-root, {}", s.town))
                .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    Ok(Decoded { observations, roots: roots.len(), unplaced })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = r#"// generated
const roots = new Map([["A", {"letter": "A", "operator": "Verisign, Inc.", "ipv4": "198.41.0.4", "ipv6": "2001:503:ba3e::2:30", "asn": 7342}], ["K", {"letter": "K", "operator": "RIPE NCC", "ipv4": "193.0.14.129", "ipv6": "2001:7fd::1", "asn": 25152}]]);
const sites = [{"root": "K", "town": "Accra", "country": "GH", "ipv4": true, "ipv6": true, "lat": 5.5571096, "lon": -0.2012376, "instances": 1}, {"root": "K", "town": "Accra", "country": "GH", "ipv4": true, "ipv6": false, "lat": 5.5571096, "lon": -0.2012376, "instances": 1}, {"root": "K", "town": "Accra", "country": "GH", "ipv4": true, "ipv6": true, "lat": 5.60, "lon": -0.19, "instances": 2}, {"root": "A", "town": "Ashburn [Data] Center", "country": "US", "ipv4": true, "ipv6": true, "lat": 39.0, "lon": -77.5, "instances": 3}, {"root": "K", "town": "Nowhere", "country": "XX", "ipv4": true, "ipv6": false, "lat": null, "lon": null, "instances": 1}];
function draw() { const x = "[not data]"; }
"#;

    #[test]
    fn the_two_constants_are_found_by_bracket_matching_and_sites_join_their_root() {
        let d = decode(SCRIPT, Utc::now(), &SourceId::new("root-servers")).unwrap();
        assert_eq!(d.roots, 2);
        assert_eq!(d.unplaced, 1);
        assert_eq!(d.observations.len(), 3);
        let accra = &d.observations[0];
        assert_eq!(accra.entity.key, "root:K:GH:accra");
        assert_eq!(accra.entity.kind, EntityKind::Feature);
        assert_eq!(accra.label.as_deref(), Some("K-root, Accra"));
        assert_eq!(accra.attrs["operator"], "RIPE NCC");
        assert_eq!(accra.attrs["asn"], 25152);
        assert_eq!(accra.attrs["address_v4"], "193.0.14.129");
        assert_eq!(accra.attrs["instances"], 2, "two rows at one coordinate are one site with two instances");
        let second = &d.observations[1];
        assert_eq!(second.entity.key, "root:K:GH:accra:2", "a distinct site in the same town is numbered");
        assert_eq!(second.attrs["instances"], 2);
        let ashburn = &d.observations[2];
        assert_eq!(ashburn.entity.key, "root:A:US:ashburn-[data]-center", "brackets inside a string do not end the array");
        assert_eq!(ashburn.attrs["operator"], "Verisign, Inc.");
    }

    #[test]
    fn a_script_without_the_constants_is_refused() {
        assert!(decode("const nothing = 1;", Utc::now(), &SourceId::new("root-servers")).is_err());
        assert!(decode("const roots = new Map([]); const sites = [];", Utc::now(), &SourceId::new("root-servers")).is_err());
    }
}
