//! RIPE Atlas probes: the internet measured from seventeen thousand homes,
//! offices and data centres.
//!
//! Atlas is RIPE NCC's measurement network — small probes volunteers plug
//! in, and larger anchors in data centres — and its API lists every one
//! with a position, the network it sits in, its country, and whether it
//! is connected right now. A probe that drops off is one address on the
//! internet that stopped answering; a hundred dropping off in one city is
//! something else. Counted before building: 17,238 connected or
//! disconnected (15,059 and 2,179), 1,067 of them anchors, 14 with no
//! position, 287 with no IPv4 AS, 6,589 with no description; the other
//! 43,000 probes the API knows are abandoned or never connected and are
//! not asked for. Each carries a dozen tags (home, NAT, native IPv6,
//! datacentre, the firmware line); the slugs are kept.
//!
//! An [`EntityKind::Station`] on the fixture pattern: dated by the poll,
//! the upstream's own stamps as attributes, and a disconnected probe is
//! [`Quality::Stale`]. The filter is `status__in=1,2` — `status=1,2` is a
//! 400 — and the page cursor is the `next` URL in the body, not a `Link`
//! header. Thirty-five pages at the client's one a second; every half
//! hour, because a probe's connection state changes rarely and seventeen
//! thousand rows a poll add up. Addresses are not stored: the network and
//! prefix say where a probe is on the internet without naming the host.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

const FIRST_PAGE: &str = "https://atlas.ripe.net/api/v2/probes/?page_size=500&status__in=1,2&format=json";
const CADENCE_SECS: u64 = 30 * 60;
/// 17,238 probes at 500 a page is 35; a network that doubled would still
/// fit.
const MAX_PAGES: usize = 100;

pub struct AtlasProbes {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl AtlasProbes {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("ripe-atlas"),
                layer_id: LayerId::new("internet-probes"),
                display_name: "Internet measurement probes (RIPE Atlas)".into(),
                kind: EntityKind::Station,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "RIPE Atlas, RIPE NCC".into(),
                    url: "https://atlas.ripe.net/".into(),
                    license: "RIPE Atlas data, free to use with attribution".into(),
                    notice: Some("Probe data from RIPE Atlas".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for AtlasProbes {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let now = Utc::now();
        let mut url = Some(FIRST_PAGE.to_string());
        let mut observations = Vec::with_capacity(18_000);
        let (mut pages, mut unplaced, mut expected) = (0, 0, None);
        while let Some(next) = url.take() {
            if pages >= MAX_PAGES {
                return Err(SourceError::Decode(format!("more than {MAX_PAGES} pages of probes; the cursor may be looping")));
            }
            let bytes = self.http.get_bytes(&next).await?;
            let page = decode_page(&bytes)?;
            pages += 1;
            expected.get_or_insert(page.count);
            for probe in &page.results {
                match observation(probe, now, &self.descriptor.id) {
                    Some(o) => observations.push(o),
                    None => unplaced += 1,
                }
            }
            url = page.next;
        }
        if observations.is_empty() {
            return Err(SourceError::Decode("the probe list was empty".into()));
        }
        tracing::info!(source = %self.descriptor.id, pages, probes = observations.len(), unplaced, expected = expected.unwrap_or(0), "probes read");
        Ok(observations)
    }
}

// --- wire format --------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Page {
    #[serde(default)]
    pub count: u64,
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub results: Vec<Probe>,
}

#[derive(Debug, Deserialize)]
pub struct Probe {
    pub id: u64,
    #[serde(default)]
    pub asn_v4: Option<u64>,
    #[serde(default)]
    pub asn_v6: Option<u64>,
    #[serde(default)]
    pub prefix_v4: Option<String>,
    #[serde(default)]
    pub prefix_v6: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub firmware_version: Option<u64>,
    #[serde(default)]
    pub first_connected: Option<i64>,
    #[serde(default)]
    pub last_connected: Option<i64>,
    #[serde(default)]
    pub geometry: Option<Geometry>,
    #[serde(default)]
    pub is_anchor: bool,
    #[serde(default)]
    pub is_public: bool,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub status_since: Option<i64>,
    #[serde(default)]
    pub tags: Vec<Tag>,
    #[serde(default)]
    pub total_uptime: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct Geometry {
    #[serde(default)]
    pub coordinates: Vec<f64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct Tag {
    #[serde(default)]
    pub slug: String,
}

pub fn decode_page(bytes: &[u8]) -> Result<Page, SourceError> {
    serde_json::from_slice(bytes).map_err(|e| SourceError::Decode(format!("Atlas probes: {e}")))
}

fn stamp(t: Option<i64>) -> Option<String> {
    t.and_then(|t| DateTime::from_timestamp(t, 0)).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

pub fn observation(p: &Probe, now: DateTime<Utc>, source_id: &SourceId) -> Option<Observation> {
    let g = p.geometry.as_ref()?;
    let (lon, lat) = (*g.coordinates.first()?, *g.coordinates.get(1)?);
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) || (lon == 0.0 && lat == 0.0) {
        return None;
    }
    let connected = p.status.id == 1;
    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("probe_id", serde_json::json!(p.id));
    put("status", serde_json::json!(p.status.name.to_lowercase()));
    put("status_since", serde_json::json!(stamp(p.status_since)));
    put("last_connected", serde_json::json!(stamp(p.last_connected)));
    put("first_connected", serde_json::json!(stamp(p.first_connected)));
    put("anchor", serde_json::json!(p.is_anchor.then_some(true)));
    put("public", serde_json::json!(p.is_public));
    put("asn_v4", serde_json::json!(p.asn_v4));
    put("asn_v6", serde_json::json!(p.asn_v6));
    put("prefix_v4", serde_json::json!(p.prefix_v4));
    put("prefix_v6", serde_json::json!(p.prefix_v6));
    put("country", serde_json::json!(p.country_code.as_deref().filter(|c| !c.is_empty())));
    put("description", serde_json::json!(p.description.as_deref().map(str::trim).filter(|d| !d.is_empty())));
    put("firmware", serde_json::json!(p.firmware_version));
    put("uptime_s", serde_json::json!(p.total_uptime));
    let tags: Vec<&str> = p.tags.iter().map(|t| t.slug.as_str()).filter(|s| !s.is_empty()).collect();
    put("tags", serde_json::json!(if tags.is_empty() { None } else { Some(tags) }));
    put("url", serde_json::json!(format!("https://atlas.ripe.net/probes/{}/", p.id)));
    let label = match (p.is_anchor, p.description.as_deref().map(str::trim).filter(|d| !d.is_empty())) {
        (true, Some(d)) => format!("Anchor {d}"),
        (true, None) => format!("Anchor #{}", p.id),
        (false, Some(d)) => d.to_string(),
        (false, None) => format!("Probe #{}", p.id),
    };
    Some(
        Observation::new(source_id.clone(), EntityId::new(EntityKind::Station, format!("atlas:{}", p.id)), now, if connected { Quality::Live } else { Quality::Stale })
            .with_position(Position { lon, lat, alt_m: None, datum: AltitudeDatum::Geoid })
            .with_label(label)
            .with_attrs(serde_json::Value::Object(attrs)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"{"count":17238,"next":"https://atlas.ripe.net/api/v2/probes/?page=2&page_size=500&status__in=1%2C2","previous":null,"results":[
      {"address_v4":"45.138.229.91","address_v6":"2a10:3781:e22:1:220:4aff:fec8:23d7","asn_v4":206238,"asn_v6":206238,"country_code":"NL","description":"Robert #1 100/10 Freedom.nl","firmware_version":4790,"first_connected":1288367583,"geometry":{"type":"Point","coordinates":[4.9275,52.3475]},"id":1,"is_anchor":false,"is_public":true,"last_connected":1789647529,"prefix_v4":"45.138.228.0/22","prefix_v6":"2a10:3780::/29","status":{"id":1,"name":"Connected","since":"2026-09-15T01:02:12Z"},"status_since":1789434132,"tags":[{"name":"Home","slug":"home"},{"name":"NAT","slug":"nat"}],"total_uptime":400000000,"type":"Probe"},
      {"address_v4":null,"address_v6":null,"asn_v4":null,"asn_v6":null,"country_code":"DE","description":"","firmware_version":5080,"first_connected":1500000000,"geometry":{"type":"Point","coordinates":[8.6825,50.1105]},"id":6001,"is_anchor":true,"is_public":true,"last_connected":1789000000,"prefix_v4":null,"prefix_v6":null,"status":{"id":2,"name":"Disconnected","since":"2026-09-10T00:00:00Z"},"status_since":1789000000,"tags":[],"total_uptime":1,"type":"Probe"},
      {"asn_v4":1,"country_code":"XX","geometry":null,"id":7,"is_anchor":false,"is_public":false,"status":{"id":1,"name":"Connected"},"status_since":1,"tags":[]}
    ]}"#;

    #[test]
    fn a_page_yields_placed_probes_with_the_next_cursor_and_a_disconnected_one_is_stale() {
        let page = decode_page(PAGE.as_bytes()).unwrap();
        assert_eq!(page.count, 17238);
        assert!(page.next.as_deref().unwrap().contains("page=2"));
        let now = Utc::now();
        let obs: Vec<_> = page.results.iter().filter_map(|p| observation(p, now, &SourceId::new("ripe-atlas"))).collect();
        assert_eq!(obs.len(), 2, "the probe with no geometry is not placed");
        let home = &obs[0];
        assert_eq!(home.entity.key, "atlas:1");
        assert_eq!(home.entity.kind, EntityKind::Station);
        assert_eq!(home.quality, Quality::Live);
        assert_eq!(home.observed_at, now);
        assert_eq!(home.label.as_deref(), Some("Robert #1 100/10 Freedom.nl"));
        assert_eq!(home.attrs["status"], "connected");
        assert_eq!(home.attrs["status_since"], "2026-09-15T01:02:12Z");
        assert_eq!(home.attrs["asn_v4"], 206238);
        assert_eq!(home.attrs["tags"][1], "nat");
        assert!(home.attrs.get("anchor").is_none());
        assert!(home.attrs.to_string().contains("45.138.228.0/22") && !home.attrs.to_string().contains("45.138.229.91"), "the prefix is kept, the address is not");
        let anchor = &obs[1];
        assert_eq!(anchor.quality, Quality::Stale);
        assert_eq!(anchor.label.as_deref(), Some("Anchor #6001"));
        assert_eq!(anchor.attrs["anchor"], true);
        assert!(anchor.attrs.get("asn_v4").is_none() && anchor.attrs.get("description").is_none() && anchor.attrs.get("tags").is_none());
    }
}
