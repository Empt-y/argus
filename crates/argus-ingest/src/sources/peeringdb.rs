//! The buildings the internet is wired together in, from PeeringDB.
//!
//! PeeringDB is the industry's own register of where networks meet: the
//! internet exchanges where they peer and the colocation facilities that
//! house the exchanges and the routers. Networks maintain their own
//! entries, and the data is CC0. Two layers from it, weekly, as features:
//! `data-centres` from `/api/fac` (5,874 facilities, 5,263 with
//! coordinates) and `internet-exchanges` from `/api/ix` — which carries
//! no coordinates at all; an exchange is placed through `/api/ixfac`,
//! the list of which facilities each exchange has a presence in. 915 of
//! 1,324 exchanges have at least one located facility; an exchange in
//! several buildings across a city is a MultiPoint, one in one building
//! a point, and the 409 with none are counted and left unplaced rather
//! than put at their city's centre. Anonymous access is rate-limited —
//! hard: a burst of research pulls and a test run earned a 429 with a
//! fifty-two minute Retry-After — so the two sources share one fetch of
//! the facility list, the three pulls a week are spaced out, and nothing
//! else is asked. The scheduler honours the Retry-After when it comes.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use geo_types::{Geometry, MultiPoint, Point};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

const API: &str = "https://www.peeringdb.com/api";
const CADENCE_SECS: u64 = 7 * 24 * 3600;
/// Between the pulls one poll makes; anonymous access is throttled.
const BETWEEN_PULLS: Duration = Duration::from_secs(10);

/// The facility list, fetched once and shared by both sources: the
/// exchanges need it for coordinates and the facilities layer is it.
pub struct Shared {
    http: HttpClient,
    facilities: tokio::sync::Mutex<Option<(std::time::Instant, Vec<Facility>)>>,
}

/// A fetched facility list is good for this long before either source
/// asks again; a week's cadence means it is fetched once a week anyway.
const FACILITIES_FRESH_FOR: Duration = Duration::from_secs(24 * 3600);

impl Shared {
    pub fn new(http: HttpClient) -> Self {
        Self { http, facilities: tokio::sync::Mutex::new(None) }
    }

    async fn facilities(&self) -> Result<Vec<Facility>, SourceError> {
        let mut cached = self.facilities.lock().await;
        if let Some((at, list)) = cached.as_ref()
            && at.elapsed() < FACILITIES_FRESH_FOR
        {
            return Ok(list.clone());
        }
        let bytes = self.http.get_bytes(&format!("{API}/fac")).await?;
        let list = decode_facilities(&bytes)?;
        *cached = Some((std::time::Instant::now(), list.clone()));
        Ok(list)
    }
}

fn attribution() -> Attribution {
    Attribution {
        provider: "PeeringDB".into(),
        url: "https://www.peeringdb.com/".into(),
        license: "CC0 1.0".into(),
        notice: Some("Facility and exchange data from PeeringDB (CC0)".into()),
    }
}

// --- facilities -------------------------------------------------------------------

pub struct Facilities {
    descriptor: SourceDescriptor,
    shared: std::sync::Arc<Shared>,
}

impl Facilities {
    pub fn new(shared: std::sync::Arc<Shared>) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("peeringdb-fac"),
                layer_id: LayerId::new("data-centres"),
                display_name: "Data centres and colocation (PeeringDB)".into(),
                kind: EntityKind::Feature,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: attribution(),
                base_quality: Quality::Live,
                quota: None,
            },
            shared,
        }
    }
}

#[async_trait::async_trait]
impl Source for Facilities {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let facilities = self.shared.facilities().await?;
        let now = Utc::now();
        let mut unplaced = 0;
        let observations: Vec<Observation> = facilities
            .iter()
            .filter_map(|f| {
                let o = facility_observation(f, now, &self.descriptor.id);
                if o.is_none() {
                    unplaced += 1;
                }
                o
            })
            .collect();
        if observations.is_empty() {
            return Err(SourceError::Decode(format!("{} facilities and none with coordinates", facilities.len())));
        }
        tracing::info!(source = %self.descriptor.id, facilities = facilities.len(), placed = observations.len(), unplaced, "facilities read");
        Ok(observations)
    }
}

// --- exchanges --------------------------------------------------------------------

pub struct Exchanges {
    descriptor: SourceDescriptor,
    shared: std::sync::Arc<Shared>,
}

impl Exchanges {
    pub fn new(shared: std::sync::Arc<Shared>) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("peeringdb-ix"),
                layer_id: LayerId::new("internet-exchanges"),
                display_name: "Internet exchange points (PeeringDB)".into(),
                kind: EntityKind::Feature,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: attribution(),
                base_quality: Quality::Live,
                quota: None,
            },
            shared,
        }
    }
}

#[async_trait::async_trait]
impl Source for Exchanges {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let exchanges = decode_exchanges(&self.shared.http.get_bytes(&format!("{API}/ix")).await?)?;
        tokio::time::sleep(BETWEEN_PULLS).await;
        let presences = decode_presences(&self.shared.http.get_bytes(&format!("{API}/ixfac")).await?)?;
        tokio::time::sleep(BETWEEN_PULLS).await;
        let facilities = self.shared.facilities().await?;
        let now = Utc::now();
        let decoded = exchange_observations(&exchanges, &presences, &facilities, now, &self.descriptor.id);
        if decoded.observations.is_empty() {
            return Err(SourceError::Decode(format!("{} exchanges and none could be placed through {} facilities", exchanges.len(), facilities.len())));
        }
        tracing::info!(source = %self.descriptor.id, exchanges = exchanges.len(), placed = decoded.observations.len(), unplaced = decoded.unplaced, "exchanges read");
        Ok(decoded.observations)
    }
}

// --- wire format --------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(bound = "T: serde::de::DeserializeOwned")]
struct Envelope<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Facility {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub org_name: String,
    #[serde(default)]
    pub address1: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub zipcode: String,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
    #[serde(default)]
    pub net_count: u64,
    #[serde(default)]
    pub ix_count: u64,
    #[serde(default)]
    pub carrier_count: u64,
    #[serde(default)]
    pub website: String,
    #[serde(default)]
    pub clli: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Exchange {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub name_long: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub region_continent: String,
    #[serde(default)]
    pub media: String,
    #[serde(default)]
    pub proto_ipv6: bool,
    #[serde(default)]
    pub website: String,
    #[serde(default)]
    pub url_stats: String,
    #[serde(default)]
    pub net_count: u64,
    #[serde(default)]
    pub fac_count: u64,
    #[serde(default)]
    pub service_level: String,
    #[serde(default)]
    pub terms: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Presence {
    pub ix_id: u64,
    pub fac_id: u64,
    #[serde(default)]
    pub status: String,
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<Vec<T>, SourceError> {
    let env: Envelope<T> = serde_json::from_slice(bytes).map_err(|e| SourceError::Decode(format!("PeeringDB {what}: {e}")))?;
    if env.data.is_empty() {
        return Err(SourceError::Decode(format!("PeeringDB {what}: empty")));
    }
    Ok(env.data)
}

pub fn decode_facilities(bytes: &[u8]) -> Result<Vec<Facility>, SourceError> {
    decode(bytes, "fac")
}
pub fn decode_exchanges(bytes: &[u8]) -> Result<Vec<Exchange>, SourceError> {
    decode(bytes, "ix")
}
pub fn decode_presences(bytes: &[u8]) -> Result<Vec<Presence>, SourceError> {
    decode(bytes, "ixfac")
}

fn nonempty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn placed(f: &Facility) -> Option<(f64, f64)> {
    let (lon, lat) = (f.longitude?, f.latitude?);
    ((-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat) && !(lon == 0.0 && lat == 0.0)).then_some((lon, lat))
}

pub fn facility_observation(f: &Facility, now: DateTime<Utc>, source_id: &SourceId) -> Option<Observation> {
    let (lon, lat) = placed(f)?;
    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    let name = nonempty(&f.name).unwrap_or_else(|| format!("Facility {}", f.id));
    put("name", serde_json::json!(name));
    put("operator", serde_json::json!(nonempty(&f.org_name)));
    let address: Vec<String> = [&f.address1, &f.city, &f.state, &f.zipcode].iter().filter_map(|s| nonempty(s)).collect();
    put("address", serde_json::json!(if address.is_empty() { None } else { Some(address.join(", ")) }));
    put("city", serde_json::json!(nonempty(&f.city)));
    put("country", serde_json::json!(nonempty(&f.country)));
    put("networks", serde_json::json!(f.net_count));
    put("exchanges", serde_json::json!(f.ix_count));
    put("carriers", serde_json::json!(f.carrier_count));
    put("clli", serde_json::json!(nonempty(&f.clli)));
    put("website", serde_json::json!(nonempty(&f.website).filter(|u| u.starts_with("http"))));
    put("peeringdb_id", serde_json::json!(f.id));
    put("url", serde_json::json!(format!("https://www.peeringdb.com/fac/{}", f.id)));
    Some(
        Observation::new(source_id.clone(), EntityId::new(EntityKind::Feature, format!("pdb:fac:{}", f.id)), now, Quality::Live)
            .with_position(Position { lon, lat, alt_m: None, datum: AltitudeDatum::Geoid })
            .with_label(name)
            .with_attrs(serde_json::Value::Object(attrs)),
    )
}

#[derive(Debug)]
pub struct DecodedExchanges {
    pub observations: Vec<Observation>,
    pub unplaced: usize,
}

pub fn exchange_observations(exchanges: &[Exchange], presences: &[Presence], facilities: &[Facility], now: DateTime<Utc>, source_id: &SourceId) -> DecodedExchanges {
    let located: HashMap<u64, (&Facility, (f64, f64))> = facilities.iter().filter_map(|f| placed(f).map(|p| (f.id, (f, p)))).collect();
    let mut sites: HashMap<u64, Vec<&Facility>> = HashMap::new();
    for p in presences {
        if let Some((f, _)) = located.get(&p.fac_id) {
            sites.entry(p.ix_id).or_default().push(f);
        }
    }
    let mut observations = Vec::with_capacity(exchanges.len());
    let mut unplaced = 0;
    for ix in exchanges {
        let Some(in_facilities) = sites.get(&ix.id).filter(|v| !v.is_empty()) else {
            unplaced += 1;
            continue;
        };
        let mut in_facilities: Vec<&Facility> = in_facilities.clone();
        in_facilities.sort_by_key(|f| f.id);
        in_facilities.dedup_by_key(|f| f.id);
        let points: Vec<Point<f64>> = in_facilities.iter().filter_map(|f| placed(f)).map(|(lon, lat)| Point::new(lon, lat)).collect();
        let geometry = if points.len() == 1 { Geometry::Point(points[0]) } else { Geometry::MultiPoint(MultiPoint(points)) };
        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        let name = nonempty(&ix.name).unwrap_or_else(|| format!("IX {}", ix.id));
        put("name", serde_json::json!(name));
        put("long_name", serde_json::json!(nonempty(&ix.name_long).filter(|l| l != &name)));
        put("city", serde_json::json!(nonempty(&ix.city)));
        put("country", serde_json::json!(nonempty(&ix.country)));
        put("continent", serde_json::json!(nonempty(&ix.region_continent)));
        put("networks", serde_json::json!(ix.net_count));
        put("facilities_listed", serde_json::json!(ix.fac_count));
        put("facilities", serde_json::json!(in_facilities.iter().map(|f| serde_json::json!({"name": f.name, "city": f.city, "operator": f.org_name})).collect::<Vec<_>>()));
        put("ipv6", serde_json::json!(ix.proto_ipv6));
        put("media", serde_json::json!(nonempty(&ix.media)));
        put("service_level", serde_json::json!(nonempty(&ix.service_level)));
        put("terms", serde_json::json!(nonempty(&ix.terms)));
        put("website", serde_json::json!(nonempty(&ix.website).filter(|u| u.starts_with("http"))));
        put("traffic_stats", serde_json::json!(nonempty(&ix.url_stats).filter(|u| u.starts_with("http"))));
        put("peeringdb_id", serde_json::json!(ix.id));
        put("url", serde_json::json!(format!("https://www.peeringdb.com/ix/{}", ix.id)));
        observations.push(
            Observation::new(source_id.clone(), EntityId::new(EntityKind::Feature, format!("pdb:ix:{}", ix.id)), now, Quality::Live)
                .with_geom(geometry)
                .with_label(name)
                .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    DecodedExchanges { observations, unplaced }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAC: &str = r#"{"data":[{"id":1,"org_id":2,"org_name":"Equinix, Inc.","name":"Equinix DC1-DC15,DC21-DC22 - Ashburn","address1":"21715 Filigree Ct","city":"Ashburn","state":"VA","zipcode":"20147-6205","country":"US","clli":"ASBNVA","latitude":39.016363,"longitude":-77.459023,"net_count":516,"ix_count":9,"carrier_count":30,"website":"http://www.equinix.com/","status":"ok"},{"id":7,"org_name":"Equinix, Inc.","name":"Equinix CH1/CH2/CH4 - Chicago","city":"Chicago","country":"US","latitude":41.85,"longitude":-87.62,"net_count":300,"ix_count":5,"status":"ok"},{"id":99,"org_name":"Nowhere Ltd","name":"Unplaced","city":"Nowhere","country":"XX","latitude":null,"longitude":null,"net_count":0,"ix_count":0,"status":"ok"}],"meta":{}}"#;
    const IX: &str = r#"{"data":[{"id":1,"org_id":2,"name":"Equinix Ashburn","aka":"","name_long":"Equinix Internet Exchange Ashburn","city":"Ashburn","country":"US","region_continent":"North America","media":"Ethernet","proto_unicast":true,"proto_multicast":false,"proto_ipv6":true,"website":"https://ix.equinix.com","url_stats":"https://ix.equinix.com/home/locations-and-traffic/#traffic","net_count":347,"fac_count":3,"service_level":"24/7 Support","terms":"Recurring Fees","status":"ok"},{"id":2,"name":"Equinix Chicago","name_long":"Equinix Chicago","city":"Chicago","country":"US","proto_ipv6":true,"net_count":200,"fac_count":1,"status":"ok"},{"id":3,"name":"Lonely-IX","city":"Nowhere","country":"XX","net_count":1,"fac_count":1,"status":"ok"}],"meta":{}}"#;
    const IXFAC: &str = r#"{"data":[{"id":1,"name":"Equinix DC1-DC15,DC21-DC22 - Ashburn","city":"Ashburn","country":"US","ix_id":1,"fac_id":1,"status":"ok"},{"id":2,"name":"x","city":"Chicago","country":"US","ix_id":1,"fac_id":7,"status":"ok"},{"id":3,"name":"y","ix_id":2,"fac_id":7,"status":"ok"},{"id":4,"name":"z","ix_id":3,"fac_id":99,"status":"ok"}],"meta":{}}"#;

    #[test]
    fn a_facility_with_coordinates_is_a_feature_and_one_without_is_not() {
        let f = decode_facilities(FAC.as_bytes()).unwrap();
        let now = Utc::now();
        let obs: Vec<_> = f.iter().filter_map(|x| facility_observation(x, now, &SourceId::new("peeringdb-fac"))).collect();
        assert_eq!(obs.len(), 2);
        let ashburn = &obs[0];
        assert_eq!(ashburn.entity.key, "pdb:fac:1");
        assert_eq!(ashburn.entity.kind, EntityKind::Feature);
        assert_eq!(ashburn.attrs["operator"], "Equinix, Inc.");
        assert_eq!(ashburn.attrs["address"], "21715 Filigree Ct, Ashburn, VA, 20147-6205");
        assert_eq!(ashburn.attrs["networks"], 516);
        assert_eq!(ashburn.attrs["clli"], "ASBNVA");
        assert!((ashburn.position.unwrap().lat - 39.016363).abs() < 1e-9);
        assert!(decode_facilities(br#"{"data":[]}"#).is_err());
    }

    #[test]
    fn an_exchange_is_placed_through_its_facilities_as_one_point_or_several() {
        let ix = decode_exchanges(IX.as_bytes()).unwrap();
        let p = decode_presences(IXFAC.as_bytes()).unwrap();
        let f = decode_facilities(FAC.as_bytes()).unwrap();
        let d = exchange_observations(&ix, &p, &f, Utc::now(), &SourceId::new("peeringdb-ix"));
        assert_eq!(d.unplaced, 1, "an exchange whose only facility has no coordinates is not placed");
        assert_eq!(d.observations.len(), 2);
        let ashburn = &d.observations[0];
        assert_eq!(ashburn.entity.key, "pdb:ix:1");
        assert!(matches!(ashburn.geom, Some(Geometry::MultiPoint(ref m)) if m.0.len() == 2), "two buildings: {:?}", ashburn.geom);
        assert_eq!(ashburn.attrs["long_name"], "Equinix Internet Exchange Ashburn");
        assert_eq!(ashburn.attrs["facilities"].as_array().unwrap().len(), 2);
        assert_eq!(ashburn.attrs["networks"], 347);
        assert_eq!(ashburn.attrs["ipv6"], true);
        let chicago = &d.observations[1];
        assert!(matches!(chicago.geom, Some(Geometry::Point(_))), "one building: {:?}", chicago.geom);
        assert!(chicago.attrs.get("long_name").is_none(), "a long name equal to the name is noise");
    }
}
