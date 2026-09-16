//! Submarine cables and their landing points, from TeleGeography's
//! Submarine Cable Map.
//!
//! TeleGeography publishes the map's own data as JSON: one file of every
//! cable's route as a MultiLineString (728 when this was written, 740 KB),
//! one of every landing point (1,925), and a record per cable with its
//! owners, supplier, length, ready-for-service year and landing points.
//! The routes are schematic — a cable is drawn as a smooth path between
//! its landings, not the surveyed track along the seabed, which the
//! owners do not publish — and the card says so.
//!
//! Two [`EntityKind::Feature`] layers: the cables as lines and the
//! landings as points. The per-cable record is a request each, paced at
//! one a second, so a poll is twelve minutes; it runs weekly, the store
//! writes a feature only when something about it changed, and a cable
//! whose detail request fails is written from its route alone rather than
//! dropped. Planned cables are carried with `planned: true` and drawn:
//! where a cable will land is a fact about the coast.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use geo_types::{Coord, Geometry, LineString, MultiLineString};
use serde::Deserialize;
use std::collections::HashMap;

const BASE: &str = "https://www.submarinecablemap.com/api/v3";

/// Weekly. Cables are laid over years and the map is updated when they are.
const CADENCE_SECS: u64 = 7 * 24 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asset {
    Cables,
    Landings,
}

pub struct SubmarineCables {
    descriptor: SourceDescriptor,
    http: HttpClient,
    asset: Asset,
}

fn attribution() -> Attribution {
    Attribution {
        provider: "TeleGeography Submarine Cable Map".into(),
        url: "https://www.submarinecablemap.com/".into(),
        license: "CC BY-SA 4.0".into(),
        notice: Some("Submarine cable data © TeleGeography, CC BY-SA 4.0".into()),
    }
}

impl SubmarineCables {
    /// Every cable as a line, with its owners, supplier and service year.
    pub fn cables(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("submarine-cables"),
                layer_id: LayerId::new("submarine-cables"),
                display_name: "Submarine cables (TeleGeography)".into(),
                kind: EntityKind::Feature,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: attribution(),
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            asset: Asset::Cables,
        }
    }

    /// Every landing point, with the cables that come ashore there.
    pub fn landing_points(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("cable-landings"),
                layer_id: LayerId::new("cable-landings"),
                display_name: "Submarine cable landing points (TeleGeography)".into(),
                kind: EntityKind::Feature,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: attribution(),
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            asset: Asset::Landings,
        }
    }

    /// The per-cable records, by cable id. A failed record is a warning
    /// and a cable without owners, not a failed poll.
    async fn details(&self, ids: &[String]) -> HashMap<String, CableDetail> {
        let mut out = HashMap::with_capacity(ids.len());
        let mut failed = 0;
        for id in ids {
            let url = format!("{BASE}/cable/{id}.json");
            match self.http.get_json::<CableDetail>(&url).await {
                Ok(d) => {
                    out.insert(id.clone(), d);
                }
                Err(err) => {
                    failed += 1;
                    if failed <= 5 {
                        tracing::warn!(source = %self.descriptor.id, cable = %id, %err, "cable record failed; written from its route alone");
                    }
                }
            }
        }
        if failed > 0 {
            tracing::warn!(source = %self.descriptor.id, failed, of = ids.len(), "cable records failed");
        }
        out
    }
}

#[async_trait::async_trait]
impl Source for SubmarineCables {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let now = Utc::now();
        match self.asset {
            Asset::Cables => {
                let geo: CableGeo = self.http.get_json(&format!("{BASE}/cable/cable-geo.json")).await?;
                if geo.features.is_empty() {
                    return Err(SourceError::Decode("cable-geo.json has no features".into()));
                }
                let mut ids: Vec<String> = geo.features.iter().filter_map(|f| f.properties.id.clone()).collect();
                ids.sort_unstable();
                ids.dedup();
                let details = self.details(&ids).await;
                let obs = decode_cables(&geo, &details, &self.descriptor.id, now);
                tracing::info!(source = %self.descriptor.id, cables = obs.len(), with_record = details.len(), "submarine cables read");
                Ok(obs)
            }
            Asset::Landings => {
                let geo: LandingGeo = self.http.get_json(&format!("{BASE}/landing-point/landing-point-geo.json")).await?;
                // Which cables land where comes from the cable records, so
                // the landings layer reads them too. Same twelve minutes;
                // the two sources are scheduled independently.
                let list: Vec<CableRef> = self.http.get_json(&format!("{BASE}/cable/all.json")).await.unwrap_or_default();
                let ids: Vec<String> = list.into_iter().filter_map(|c| c.id).collect();
                let details = self.details(&ids).await;
                let obs = decode_landings(&geo, &details, &self.descriptor.id, now);
                tracing::info!(source = %self.descriptor.id, landings = obs.len(), "cable landing points read");
                Ok(obs)
            }
        }
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CableGeo {
    #[serde(default)]
    features: Vec<CableFeature>,
}

#[derive(Debug, Deserialize)]
struct CableFeature {
    properties: CableProps,
    geometry: Option<LineGeometry>,
}

#[derive(Debug, Deserialize)]
struct CableProps {
    id: Option<String>,
    name: Option<String>,
    color: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum LineGeometry {
    LineString { coordinates: Vec<[f64; 2]> },
    MultiLineString { coordinates: Vec<Vec<[f64; 2]>> },
}

#[derive(Debug, Deserialize, Default)]
struct CableRef {
    id: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct CableDetail {
    name: Option<String>,
    length: Option<String>,
    #[serde(default)]
    landing_points: Vec<LandingRef>,
    owners: Option<String>,
    suppliers: Option<String>,
    rfs: Option<String>,
    rfs_year: Option<i64>,
    is_planned: Option<bool>,
    url: Option<String>,
    notes: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct LandingRef {
    id: Option<String>,
    name: Option<String>,
    country: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LandingGeo {
    #[serde(default)]
    features: Vec<LandingFeature>,
}

#[derive(Debug, Deserialize)]
struct LandingFeature {
    properties: LandingProps,
    geometry: Option<PointGeometry>,
}

#[derive(Debug, Deserialize)]
struct LandingProps {
    id: Option<String>,
    name: Option<String>,
    is_tbd: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PointGeometry {
    coordinates: [f64; 2],
}

fn lines(g: &LineGeometry) -> Vec<LineString<f64>> {
    let to_line = |pts: &Vec<[f64; 2]>| LineString(pts.iter().map(|[x, y]| Coord { x: *x, y: *y }).collect());
    match g {
        LineGeometry::LineString { coordinates } => vec![to_line(coordinates)],
        LineGeometry::MultiLineString { coordinates } => coordinates.iter().map(to_line).collect(),
    }
    .into_iter()
    .filter(|l| l.0.len() >= 2)
    .collect()
}

/// A point on the cable to anchor its card: the middle vertex of its
/// longest part.
fn anchor(parts: &[LineString<f64>]) -> Option<(f64, f64)> {
    let longest = parts.iter().max_by_key(|l| l.0.len())?;
    let mid = &longest.0[longest.0.len() / 2];
    Some((mid.x, mid.y))
}

/// `"45,000 km"` → 45000.
fn parse_length_km(s: &str) -> Option<f64> {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
    digits.parse().ok()
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

fn decode_cables(geo: &CableGeo, details: &HashMap<String, CableDetail>, source_id: &SourceId, now: DateTime<Utc>) -> Vec<Observation> {
    // The geo file has one feature per cable, but guard against a split
    // cable arriving as several features: merge parts by cable id.
    let mut by_id: HashMap<String, (CableProps, Vec<LineString<f64>>)> = HashMap::new();
    let mut order = Vec::new();
    for f in &geo.features {
        let Some(id) = f.properties.id.clone() else { continue };
        let Some(g) = f.geometry.as_ref() else { continue };
        let parts = lines(g);
        match by_id.get_mut(&id) {
            Some((_, existing)) => existing.extend(parts),
            None => {
                order.push(id.clone());
                by_id.insert(
                    id,
                    (
                        CableProps {
                            id: f.properties.id.clone(),
                            name: f.properties.name.clone(),
                            color: f.properties.color.clone(),
                        },
                        parts,
                    ),
                );
            }
        }
    }

    let mut out = Vec::with_capacity(order.len());
    for id in order {
        let (props, parts) = &by_id[&id];
        let Some((lon, lat)) = anchor(parts) else { continue };
        let detail = details.get(&id);
        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        let name = detail.and_then(|d| d.name.clone()).or_else(|| props.name.clone()).unwrap_or_else(|| id.clone());
        put("cable_id", serde_json::json!(id));
        put("name", serde_json::json!(name));
        put("map_color", serde_json::json!(props.color));
        put("route", serde_json::json!("schematic"));
        let mut planned = false;
        if let Some(d) = detail {
            planned = d.is_planned.unwrap_or(false);
            put("length_km", serde_json::json!(d.length.as_deref().and_then(parse_length_km)));
            put("length_text", serde_json::json!(d.length));
            put("owners", serde_json::json!(d.owners.as_deref().map(split_list).filter(|v| !v.is_empty())));
            put("suppliers", serde_json::json!(d.suppliers.as_deref().map(split_list).filter(|v| !v.is_empty())));
            put("ready_for_service", serde_json::json!(d.rfs));
            put("ready_for_service_year", serde_json::json!(d.rfs_year));
            put("planned", serde_json::json!(planned));
            put("url", serde_json::json!(d.url.as_deref().filter(|u| u.starts_with("http"))));
            put("notes", serde_json::json!(d.notes.as_deref().map(str::trim).filter(|s| !s.is_empty())));
            let landings: Vec<serde_json::Value> = d
                .landing_points
                .iter()
                .filter_map(|l| {
                    let name = l.name.as_deref()?;
                    Some(serde_json::json!({"id": l.id, "name": name, "country": l.country}))
                })
                .collect();
            put("landing_count", serde_json::json!(landings.len()));
            put("landings", serde_json::json!(landings));
        }
        let label = if planned { format!("{name} (planned)") } else { name.clone() };
        let geom = match parts.len() {
            0 => None,
            1 => Some(Geometry::LineString(parts[0].clone())),
            _ => Some(Geometry::MultiLineString(MultiLineString(parts.clone()))),
        };
        let mut obs = Observation::new(source_id.clone(), EntityId::new(EntityKind::Feature, format!("cable:{id}")), now, Quality::Live)
            .with_position(Position { lon, lat, alt_m: None, datum: AltitudeDatum::Geoid })
            .with_label(label)
            .with_attrs(serde_json::Value::Object(attrs));
        if let Some(g) = geom {
            obs = obs.with_geom(g);
        }
        out.push(obs);
    }
    out
}

fn decode_landings(geo: &LandingGeo, details: &HashMap<String, CableDetail>, source_id: &SourceId, now: DateTime<Utc>) -> Vec<Observation> {
    // Landing id → the cables that land there.
    let mut cables_at: HashMap<&str, Vec<(String, bool)>> = HashMap::new();
    for (id, d) in details {
        let name = d.name.clone().unwrap_or_else(|| id.clone());
        for l in &d.landing_points {
            if let Some(lid) = l.id.as_deref() {
                cables_at.entry(lid).or_default().push((name.clone(), d.is_planned.unwrap_or(false)));
            }
        }
    }
    let mut out = Vec::with_capacity(geo.features.len());
    for f in &geo.features {
        let (Some(id), Some(g)) = (f.properties.id.as_deref(), f.geometry.as_ref()) else { continue };
        let [lon, lat] = g.coordinates;
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            continue;
        }
        let name = f.properties.name.clone().unwrap_or_else(|| id.to_string());
        let mut cables = cables_at.get(id).cloned().unwrap_or_default();
        cables.sort();
        cables.dedup();
        let mut attrs = serde_json::Map::new();
        attrs.insert("landing_id".into(), serde_json::json!(id));
        attrs.insert("name".into(), serde_json::json!(name));
        if f.properties.is_tbd == Some(true) {
            attrs.insert("location_tbd".into(), serde_json::json!(true));
        }
        if !cables.is_empty() {
            let in_service: Vec<&str> = cables.iter().filter(|(_, p)| !p).map(|(n, _)| n.as_str()).collect();
            let planned: Vec<&str> = cables.iter().filter(|(_, p)| *p).map(|(n, _)| n.as_str()).collect();
            attrs.insert("cables".into(), serde_json::json!(in_service));
            if !planned.is_empty() {
                attrs.insert("planned_cables".into(), serde_json::json!(planned));
            }
            attrs.insert("cable_count".into(), serde_json::json!(cables.len()));
        }
        out.push(
            Observation::new(source_id.clone(), EntityId::new(EntityKind::Feature, format!("landing:{id}")), now, Quality::Live)
                .with_position(Position { lon, lat, alt_m: None, datum: AltitudeDatum::Geoid })
                .with_label(name)
                .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEO: &str = r##"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"id":"2africa","name":"2Africa","color":"#939597","feature_id":"2africa-0"},"geometry":{"type":"MultiLineString","coordinates":[[[13.2,-8.8],[10.0,-10.0],[5.0,-5.0]],[[-4.0,50.3],[-6.0,48.0]]]}},
      {"type":"Feature","properties":{"id":"tiny","name":"Tiny","color":"#000000"},"geometry":{"type":"MultiLineString","coordinates":[[[0.0,0.0]]]}}
    ]}"##;

    fn detail() -> HashMap<String, CableDetail> {
        let d: CableDetail = serde_json::from_str(r#"{"id":"2africa","name":"2Africa","length":"45,000 km","landing_points":[{"id":"luanda-angola","name":"Luanda, Angola","country":"Angola","is_tbd":null},{"id":"bude-united-kingdom","name":"Bude, United Kingdom","country":"United Kingdom","is_tbd":null}],"owners":"Bayobab, China Mobile, Meta","suppliers":"ASN","rfs":"2024","rfs_year":2024,"is_planned":false,"url":"https://www.2africacable.net/","notes":null}"#).unwrap();
        HashMap::from([("2africa".to_string(), d)])
    }

    #[test]
    fn a_cable_is_a_line_feature_with_its_owners_and_landings() {
        let geo: CableGeo = serde_json::from_str(GEO).unwrap();
        let obs = decode_cables(&geo, &detail(), &SourceId::new("submarine-cables"), Utc::now());
        assert_eq!(obs.len(), 1, "a one-vertex part is not a line, and a cable with none is dropped");
        let c = &obs[0];
        assert_eq!(c.entity.key, "cable:2africa");
        assert_eq!(c.entity.kind, EntityKind::Feature);
        assert!(matches!(c.geom, Some(Geometry::MultiLineString(_))));
        let p = c.position.unwrap();
        assert!((p.lon - 10.0).abs() < 1e-9 && (p.lat + 10.0).abs() < 1e-9, "anchored at the middle vertex of the longest part");
        assert_eq!(c.attrs["length_km"], 45000.0);
        assert_eq!(c.attrs["owners"], serde_json::json!(["Bayobab", "China Mobile", "Meta"]));
        assert_eq!(c.attrs["landing_count"], 2);
        assert_eq!(c.attrs["route"], "schematic");
        assert_eq!(c.label.as_deref(), Some("2Africa"));
    }

    #[test]
    fn a_landing_lists_the_cables_that_come_ashore() {
        let geo: LandingGeo = serde_json::from_str(r#"{"type":"FeatureCollection","features":[
          {"type":"Feature","properties":{"id":"bude-united-kingdom","name":"Bude, United Kingdom","is_tbd":false},"geometry":{"type":"Point","coordinates":[-4.55,50.83]}},
          {"type":"Feature","properties":{"id":"nowhere","name":"Nowhere","is_tbd":true},"geometry":{"type":"Point","coordinates":[1.0,1.0]}}
        ]}"#).unwrap();
        let obs = decode_landings(&geo, &detail(), &SourceId::new("cable-landings"), Utc::now());
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0].entity.key, "landing:bude-united-kingdom");
        assert_eq!(obs[0].attrs["cables"], serde_json::json!(["2Africa"]));
        assert_eq!(obs[0].attrs["cable_count"], 1);
        assert!(obs[1].attrs.get("cables").is_none());
        assert_eq!(obs[1].attrs["location_tbd"], true);
    }
}
