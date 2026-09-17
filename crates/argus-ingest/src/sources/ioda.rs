//! Internet outages, from IODA.
//!
//! IODA (Georgia Tech's Internet Outage Detection and Analysis) watches
//! three signals for every country, region and network on the internet:
//! how many of a network's prefixes the world's BGP collectors can still
//! see, whether its /24s still answer a ping, and whether the unsolicited
//! background traffic that every network leaks into a darknet has stopped.
//! When one of them drops below its recent history, that is an outage
//! event, scored by how far and for how long. This is BGP made spatial by
//! people who do it professionally; Argus reads their events rather than
//! re-deriving a poorer version from the raw route stream.
//!
//! Every event is placed. A region or a geo-ASN event is drawn on the
//! region's outline and a country event on the country's, both from the
//! Natural Earth polygons IODA itself publishes as TopoJSON (37 MB for
//! 4,581 regions, fetched once and kept in the geometry cache). An AS-wide
//! event has no place of its own — RIPEstat will not geolocate an AS — so
//! it is drawn on the country the AS is registered in, and says so.
//!
//! Counted over a day of events before this was designed: 2,000 (the page
//! limit) in 24 hours across `geoasn` 868 (a quarter of them a network in
//! a country rather than a region), `asn` 867, `region` 241 and
//! `country` 24; `status` is always 0 and `fraction` and `uncertainty`
//! always null, so none of the three is stored; three (location, start)
//! pairs repeat with a different `datasource`, so the key carries it. An
//! event still in progress is listed with its duration so far, capped at
//! IODA's fourteen-day window; the row is dated by its start and is not
//! rewritten as it lengthens, so the card says "for at least".

use crate::http::HttpClient;
use crate::ripestat::RipeStat;
use crate::topojson;
use argus_core::entity::{EntityId, EntityKind, Observation, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use argus_core::GeometryCache;
use chrono::{DateTime, Duration, Utc};
use geo_types::Geometry;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

const API: &str = "https://api.ioda.inetintel.cc.gatech.edu/v2";
const CADENCE_SECS: u64 = 10 * 60;
/// How far back each poll asks. Events in progress are returned whatever
/// their start, so this only bounds the finished ones.
const WINDOW_HOURS: i64 = 24;
const PAGE: usize = 2000;
const MAX_PAGES: usize = 5;
/// IODA reports a still-running event with its duration so far, and
/// stops extending it at two weeks.
const ONGOING_CAP_SECS: i64 = 14 * 24 * 3600;
/// The region topology is 37 MB decoded.
const MAX_TOPOLOGY_BYTES: usize = 96 * 1024 * 1024;
/// Cache key suffix marking a topology as wholly cached.
const ALL_CACHED: &str = "__all__";

pub struct Ioda {
    descriptor: SourceDescriptor,
    http: HttpClient,
    ripestat: Arc<RipeStat>,
    cache: Arc<dyn GeometryCache>,
    /// Polygons decoded this process, by cache key, so one poll's thousand
    /// lookups do not each go to the database.
    shapes: tokio::sync::Mutex<HashMap<String, Geometry<f64>>>,
    /// Which topologies have been downloaded this process, so a key that
    /// is genuinely absent is not a download per event.
    loaded: tokio::sync::Mutex<[bool; 2]>,
}

impl Ioda {
    pub fn new(http: HttpClient, ripestat: Arc<RipeStat>) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("ioda"),
                layer_id: LayerId::new("internet-outages"),
                display_name: "Internet outages (IODA)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "IODA, Georgia Tech Internet Intelligence Lab".into(),
                    url: "https://ioda.inetintel.cc.gatech.edu/".into(),
                    license: "IODA data, © Georgia Tech Research Corporation; free for non-commercial use with attribution".into(),
                    notice: Some("Outage data from IODA (Georgia Tech). Region and country outlines © Natural Earth.".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http: http.with_max_bytes(MAX_TOPOLOGY_BYTES),
            ripestat,
            cache: Arc::new(argus_core::MemoryGeometryCache::new()),
            shapes: tokio::sync::Mutex::new(HashMap::new()),
            loaded: tokio::sync::Mutex::new([false, false]),
        }
    }

    /// Back the outlines with something persistent, so a restart does not
    /// download 57 MB of topology again.
    #[must_use]
    pub fn with_shape_cache(mut self, cache: Arc<dyn GeometryCache>) -> Self {
        self.cache = cache;
        self
    }

    /// The outline for a cache key, from memory, the persistent cache, or
    /// the topology it lives in — downloaded once per process.
    async fn outline(&self, key: &str) -> Result<Option<Geometry<f64>>, SourceError> {
        if let Some(g) = self.shapes.lock().await.get(key) {
            return Ok(Some(g.clone()));
        }
        if let Some(g) = self.cache.get(key).await {
            self.shapes.lock().await.insert(key.to_string(), g.clone());
            return Ok(Some(g));
        }
        let (which, path, object, id_property) = if key.starts_with("ioda:region:") {
            (0, "region", "ne_10m_admin_1.regions", "id")
        } else {
            (1, "country", "ne_10m_admin_0.countries", "usercode")
        };
        {
            let mut loaded = self.loaded.lock().await;
            if loaded[which] {
                return Ok(None);
            }
            // A sentinel in the persistent cache says the whole topology is
            // there, so a key it lacks — an AS registered to "EU" — is a
            // real absence and not a reason to download 37 MB.
            if self.cache.get(&format!("ioda:{path}:{ALL_CACHED}")).await.is_some() {
                loaded[which] = true;
                return Ok(None);
            }
        }
        let bytes = self.http.get_bytes(&format!("{API}/topo/{path}")).await?;
        let decoded = decode_topology(&bytes, object, id_property)?;
        tracing::info!(source = %self.descriptor.id, topology = path, outlines = decoded.len(), "topology read");
        let mut shapes = self.shapes.lock().await;
        for (id, geometry) in decoded {
            let k = format!("ioda:{path}:{id}");
            self.cache.put(&k, &geometry).await;
            shapes.insert(k, geometry);
        }
        self.cache.put(&format!("ioda:{path}:{ALL_CACHED}"), &Geometry::Point(geo_types::Point::new(0.0, 0.0))).await;
        self.loaded.lock().await[which] = true;
        Ok(shapes.get(key).cloned())
    }

    /// Where an event is drawn, and how the placement was decided.
    async fn place(&self, location: &Location) -> Result<Option<(Geometry<f64>, &'static str)>, SourceError> {
        match location {
            Location::Region(id) | Location::AsnInRegion { region: id, .. } => Ok(self.outline(&format!("ioda:region:{id}")).await?.map(|g| (g, "region"))),
            Location::Country(cc) | Location::AsnInCountry { country: cc, .. } => Ok(self.outline(&format!("ioda:country:{cc}")).await?.map(|g| (g, "country"))),
            Location::Asn(asn) => {
                let Some(cc) = self.ripestat.asn_country(*asn).await? else { return Ok(None) };
                Ok(self.outline(&format!("ioda:country:{cc}")).await?.map(|g| (g, "registered country")))
            }
        }
    }
}

#[async_trait::async_trait]
impl Source for Ioda {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let until = Utc::now();
        let from = until - Duration::hours(WINDOW_HOURS);
        let mut events = Vec::new();
        for page in 0..MAX_PAGES {
            let url = format!("{API}/outages/events?from={}&until={}&limit={PAGE}&page={page}", from.timestamp(), until.timestamp());
            let bytes = self.http.get_bytes(&url).await?;
            let batch = decode_events(&bytes)?;
            let full = batch.len() >= PAGE;
            events.extend(batch);
            if !full {
                break;
            }
        }
        let mut observations = Vec::with_capacity(events.len());
        let (mut unplaced, mut unknown_location) = (0, 0);
        let mut by_placement: HashMap<&'static str, usize> = HashMap::new();
        for event in &events {
            let Some(location) = Location::parse(&event.location) else {
                unknown_location += 1;
                continue;
            };
            match self.place(&location).await? {
                Some((geometry, placed_by)) => {
                    *by_placement.entry(placed_by).or_default() += 1;
                    observations.push(observation(event, &location, geometry, placed_by, &self.descriptor.id));
                }
                None => unplaced += 1,
            }
        }
        if observations.is_empty() && !events.is_empty() {
            return Err(SourceError::Decode(format!("{} events and none could be placed", events.len())));
        }
        tracing::info!(source = %self.descriptor.id, events = events.len(), placed = observations.len(), unplaced, unknown_location, ?by_placement, "outage events read");
        Ok(observations)
    }
}

// --- wire format --------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    error: Option<String>,
    data: Option<T>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub location: String,
    pub start: i64,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub datasource: String,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub location_name: String,
    #[serde(default)]
    pub overlaps_window: bool,
}

pub fn decode_events(bytes: &[u8]) -> Result<Vec<Event>, SourceError> {
    let env: Envelope<Vec<Event>> = serde_json::from_slice(bytes).map_err(|e| SourceError::Decode(format!("IODA events: {e}")))?;
    if let Some(err) = env.error {
        return Err(SourceError::Decode(format!("IODA events: {err}")));
    }
    Ok(env.data.unwrap_or_default())
}

/// A topology as IODA wraps it, decoded to `(id, outline)` pairs.
pub fn decode_topology(bytes: &[u8], object: &str, id_property: &str) -> Result<Vec<(String, Geometry<f64>)>, SourceError> {
    #[derive(Deserialize)]
    struct TopoEnvelope {
        data: Option<TopoData>,
        #[serde(default)]
        error: Option<String>,
    }
    #[derive(Deserialize)]
    struct TopoData {
        topology: topojson::Topology,
    }
    let env: TopoEnvelope = serde_json::from_slice(bytes).map_err(|e| SourceError::Decode(format!("IODA topology: {e}")))?;
    if let Some(err) = env.error {
        return Err(SourceError::Decode(format!("IODA topology: {err}")));
    }
    let data = env.data.ok_or_else(|| SourceError::Decode("IODA topology without data".into()))?;
    let features = topojson::features(&data.topology, Some(object)).map_err(SourceError::Decode)?;
    let out: Vec<(String, Geometry<f64>)> = features
        .into_iter()
        .filter_map(|f| {
            let id = match f.properties.get(id_property)? {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => return None,
            };
            Some((id, f.geometry))
        })
        .collect();
    if out.is_empty() {
        return Err(SourceError::Decode(format!("IODA topology {object} decoded to no outlines")));
    }
    Ok(out)
}

/// The ways IODA names a place. A `geoasn` is a network seen from one
/// place, and that place is a region when the code is a number
/// (`geoasn/3269-1906`, a network in Cagliari) and a country when it is
/// two letters (`geoasn/23456-US`) — a quarter of them, which the first
/// cut treated as regions nobody had an outline for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Country(String),
    Region(String),
    Asn(u32),
    AsnInRegion { asn: u32, region: String },
    AsnInCountry { asn: u32, country: String },
}

impl Location {
    pub fn parse(s: &str) -> Option<Self> {
        let (kind, rest) = s.split_once('/')?;
        match kind {
            "country" => Some(Self::Country(rest.to_uppercase())),
            "region" => Some(Self::Region(rest.to_string())),
            "asn" => rest.parse().ok().map(Self::Asn),
            "geoasn" => {
                let (asn, place) = rest.split_once('-')?;
                let asn = asn.parse().ok()?;
                if place.chars().all(|c| c.is_ascii_digit()) {
                    Some(Self::AsnInRegion { asn, region: place.to_string() })
                } else {
                    Some(Self::AsnInCountry { asn, country: place.to_uppercase() })
                }
            }
            _ => None,
        }
    }

    fn scope(&self) -> &'static str {
        match self {
            Self::Country(_) => "country",
            Self::Region(_) => "region",
            Self::Asn(_) => "network",
            Self::AsnInRegion { .. } => "network in region",
            Self::AsnInCountry { .. } => "network in country",
        }
    }
}

pub fn observation(event: &Event, location: &Location, geometry: Geometry<f64>, placed_by: &str, source_id: &SourceId) -> Observation {
    let start = DateTime::from_timestamp(event.start, 0).unwrap_or_else(Utc::now);
    let ongoing = event.overlaps_window || event.duration >= ONGOING_CAP_SECS;
    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("name", serde_json::json!(event.location_name));
    put("scope", serde_json::json!(location.scope()));
    put("location", serde_json::json!(event.location));
    match location {
        Location::Asn(asn) | Location::AsnInRegion { asn, .. } | Location::AsnInCountry { asn, .. } => put("asn", serde_json::json!(asn)),
        _ => {}
    }
    put("datasource", serde_json::json!(event.datasource));
    put("score", serde_json::json!(event.score));
    put("duration_s", serde_json::json!(event.duration));
    put("ongoing", serde_json::json!(ongoing.then_some(true)));
    put("method", serde_json::json!(event.method));
    put("placed_by", serde_json::json!(placed_by));
    put("url", serde_json::json!(format!("https://ioda.inetintel.cc.gatech.edu/{}?from={}&until={}", event.location, event.start.saturating_sub(3600 * 6), event.start + event.duration + 3600 * 6)));
    let key = format!("ioda:{}:{}:{}", event.location, event.datasource, event.start);
    Observation::new(source_id.clone(), EntityId::new(EntityKind::Event, key), start, Quality::Live)
        .with_geom(geometry)
        .with_label(event.location_name.clone())
        .with_attrs(serde_json::Value::Object(attrs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENTS: &str = r#"{"type":"outages.events","metadata":{},"requestParameters":{},"error":null,"perf":null,"data":[{"location":"region/1906","start":1788354900,"duration":1209900,"uncertainty":null,"method":"median","datasource":"bgp","status":0,"fraction":null,"score":59189.58584640128,"location_name":"Cagliari","overlaps_window":true},{"location":"country/TO","start":1788422700,"duration":3600,"uncertainty":null,"method":"median","datasource":"ping-slash24","status":0,"fraction":null,"score":24585.36,"location_name":"Tonga","overlaps_window":false},{"location":"geoasn/3269-1906","start":1788354900,"duration":1209600,"method":"median","datasource":"bgp","status":0,"score":157736.33,"location_name":"ASN-IBSNAZ -- Cagliari","overlaps_window":true},{"location":"asn/41678","start":1788355200,"duration":1209600,"method":"median","datasource":"bgp","status":0,"score":60179.10,"location_name":"AS41678 (TIBUS)","overlaps_window":true},{"location":"planet/earth","start":1,"duration":1,"datasource":"bgp","score":1,"location_name":"?","overlaps_window":false}],"copyright":"..."}"#;

    const TOPO: &str = r#"{"type":"topo","error":null,"data":{"entityType":"country","idField":"usercode","topology":{"type":"Topology","objects":{"ne_10m_admin_0.countries":{"type":"GeometryCollection","geometries":[{"type":"Polygon","arcs":[[0]],"properties":{"id":1,"iso2cc":"TO","usercode":"TO","name":"Tonga"}},{"type":null,"properties":{"id":0,"usercode":"??","name":"?"}}]}},"arcs":[[[-175.3,-21.2],[-175.1,-21.2],[-175.1,-21.0],[-175.3,-21.0],[-175.3,-21.2]]]}}}"#;

    #[test]
    fn events_decode_and_every_location_form_is_understood_but_the_unknown_one() {
        let e = decode_events(EVENTS.as_bytes()).unwrap();
        assert_eq!(e.len(), 5);
        assert_eq!(Location::parse(&e[0].location), Some(Location::Region("1906".into())));
        assert_eq!(Location::parse(&e[1].location), Some(Location::Country("TO".into())));
        assert_eq!(Location::parse(&e[2].location), Some(Location::AsnInRegion { asn: 3269, region: "1906".into() }));
        assert_eq!(Location::parse("geoasn/23456-us"), Some(Location::AsnInCountry { asn: 23456, country: "US".into() }));
        assert_eq!(Location::parse(&e[3].location), Some(Location::Asn(41678)));
        assert_eq!(Location::parse(&e[4].location), None);
        let err = decode_events(br#"{"error":"'from' timestamp must be set","data":null}"#).unwrap_err();
        assert!(err.to_string().contains("from"));
    }

    #[test]
    fn a_topology_yields_outlines_by_the_id_property_skipping_the_null_one() {
        let t = decode_topology(TOPO.as_bytes(), "ne_10m_admin_0.countries", "usercode").unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, "TO");
        assert!(matches!(t[0].1, Geometry::Polygon(_)));
        assert!(decode_topology(TOPO.as_bytes(), "nope", "usercode").is_err());
    }

    #[test]
    fn an_event_is_dated_by_its_start_keyed_by_source_and_says_how_it_was_placed() {
        let e = decode_events(EVENTS.as_bytes()).unwrap();
        let outline = decode_topology(TOPO.as_bytes(), "ne_10m_admin_0.countries", "usercode").unwrap().remove(0).1;
        let asn = observation(&e[3], &Location::parse(&e[3].location).unwrap(), outline.clone(), "registered country", &SourceId::new("ioda"));
        assert_eq!(asn.entity.key, "ioda:asn/41678:bgp:1788355200");
        assert_eq!(asn.entity.kind, EntityKind::Event);
        assert_eq!(asn.observed_at.timestamp(), 1788355200);
        assert_eq!(asn.attrs["asn"], 41678);
        assert_eq!(asn.attrs["scope"], "network");
        assert_eq!(asn.attrs["placed_by"], "registered country");
        assert_eq!(asn.attrs["ongoing"], true);
        assert!(asn.geom.is_some());
        let tonga = observation(&e[1], &Location::parse(&e[1].location).unwrap(), outline, "country", &SourceId::new("ioda"));
        assert_eq!(tonga.attrs["duration_s"], 3600);
        assert!(tonga.attrs.get("ongoing").is_none(), "a finished hour is not ongoing");
        assert!(tonga.attrs.get("asn").is_none());
        assert_eq!(tonga.label.as_deref(), Some("Tonga"));
    }
}
