//! The electricity grid — transmission lines, substations and power
//! plants — from OpenStreetMap, through Overpass.
//!
//! OSM's power mapping is the best open picture of the grid there is:
//! every 400 kV line in Britain is traced, most substations are drawn as
//! their fence line, and the larger plants carry their fuel and output.
//! Overpass answers a bounding-box query for tagged ways, nodes and
//! relations with their geometry in one JSON response, and the response
//! for three degrees square around London was 3.5 MB in sixty seconds,
//! so an area is asked for in two-degree tiles, aligned to the grid so
//! two areas share them, at the patient client's pace. Weekly: the grid
//! changes over years, Overpass is a shared public instance, and the
//! store writes a feature only when something about it changed.
//!
//! ## What the whole layer showed
//!
//! - Three quarters of `power=substation` objects (14,536 of 20,588 in
//!   one tile) carry neither a `substation=` type nor a voltage. They are
//!   the green boxes at the end of every street — 11 kV distribution
//!   kiosks, each mapped as a tiny square — and drawing them would bury
//!   the grid under its own furniture. A substation is kept when it says
//!   what it is (`substation=transmission`, `distribution`, `transition`,
//!   `converter`, `traction`, `industrial`, `generation`, `compensation`)
//!   or carries a voltage of 33 kV or more; `minor_distribution` is
//!   excluded in the query itself.
//! - `voltage` is a semicolon list on double-circuit lines
//!   (`400000;275000`) and on substations with several levels
//!   (`33000;11000`). The highest is `voltage_kv`; the list is carried.
//! - Plants are ways (the site outline) or relations (the site as a
//!   collection of generators). A relation's outer way members come back
//!   with geometry and are joined into its outline where they close;
//!   otherwise the plant is a point at the relation's bounds centre.
//! - OSM ids are stable for the life of an object, so `way/12345` is the
//!   key; a way split by a mapper becomes two features and the old one is
//!   never retired, which is the honest limit of a map that never forgets.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use argus_core::BoundingBox;
use chrono::{DateTime, Utc};
use geo_types::{Coord, Geometry, LineString, Polygon};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";

/// Weekly.
const CADENCE_SECS: u64 = 7 * 24 * 3600;

/// Two degrees: the British Isles area is 42 tiles of this, each well
/// under a minute and a few megabytes.
const TILE_DEG: f64 = 2.0;

/// Overpass's own timeout for a tile, under the patient client's 120 s.
const QUERY_TIMEOUT_SECS: u32 = 110;

/// A substation with a voltage at or above this is kept even when it
/// does not say what kind it is.
const MIN_UNTYPED_SUBSTATION_KV: f64 = 33.0;

/// Overpass gives an address two slots and holds a slot for a while
/// after each heavy query; a tile sent the second the previous one
/// finished is refused with a 429. On a refusal the tile waits this long
/// (or what `Retry-After` says) and is asked again, up to
/// [`RATE_LIMIT_RETRIES`] times, before it is given up for this cycle.
const RATE_LIMIT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const RATE_LIMIT_RETRIES: usize = 6;

pub struct PowerGrid {
    descriptor: SourceDescriptor,
    http: HttpClient,
    /// Tiles read this cycle, so the home area inside the British Isles
    /// area is not asked of Overpass twice a week.
    done: Mutex<HashMap<String, DateTime<Utc>>>,
}

impl PowerGrid {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("osm-power"),
                layer_id: LayerId::new("power-grid"),
                display_name: "Power grid: lines, substations, plants (OpenStreetMap)".into(),
                kind: EntityKind::Feature,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Bounded,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "OpenStreetMap contributors, via Overpass API".into(),
                    url: "https://www.openstreetmap.org/copyright".into(),
                    license: "ODbL 1.0".into(),
                    notice: Some("© OpenStreetMap contributors".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            done: Mutex::new(HashMap::new()),
        }
    }

    /// Whether this tile is due, and mark it read if so.
    fn claim(&self, tile: &BoundingBox, now: DateTime<Utc>) -> bool {
        let mut done = self.done.lock().expect("tile lock poisoned");
        let key = format!("{:.1},{:.1}", tile.west, tile.south);
        let fresh = chrono::Duration::seconds(CADENCE_SECS as i64 - 3600);
        if done.get(&key).is_some_and(|t| now - *t < fresh) {
            return false;
        }
        done.insert(key, now);
        true
    }

    fn release(&self, tile: &BoundingBox) {
        let mut done = self.done.lock().expect("tile lock poisoned");
        done.remove(&format!("{:.1},{:.1}", tile.west, tile.south));
    }
}

/// The Overpass QL for one tile.
pub fn query(tile: &BoundingBox) -> String {
    let b = format!("({:.4},{:.4},{:.4},{:.4})", tile.south, tile.west, tile.north, tile.east);
    format!(
        "[out:json][timeout:{QUERY_TIMEOUT_SECS}];(\
         way[\"power\"=\"line\"]{b};\
         node[\"power\"=\"substation\"][\"substation\"!=\"minor_distribution\"]{b};\
         way[\"power\"=\"substation\"][\"substation\"!=\"minor_distribution\"]{b};\
         way[\"power\"=\"plant\"]{b};\
         relation[\"power\"=\"plant\"]{b};\
         );out geom;"
    )
}

/// Cut an area into tiles aligned to the tile size, as for the crime
/// layer, so the home area's tiles are the British Isles area's tiles.
pub fn tiles(bbox: &BoundingBox, size: f64) -> Vec<BoundingBox> {
    let mut out = Vec::new();
    let mut south = (bbox.south / size).floor() * size;
    while south < bbox.north {
        let mut west = (bbox.west / size).floor() * size;
        while west < bbox.east {
            out.push(BoundingBox::new(west, south, west + size, south + size));
            west += size;
        }
        south += size;
    }
    out
}

#[async_trait::async_trait]
impl Source for PowerGrid {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let Some(bbox) = ctx.bbox else {
            return Err(SourceError::Decode("the power grid is a bounded source and was polled without an area".into()));
        };
        let now = Utc::now();
        let mut out = Vec::new();
        let mut failed = 0;
        let mut kept = Kept::default();
        let tiles = tiles(&bbox, TILE_DEG);
        let mut skipped = 0;
        for tile in &tiles {
            if !self.claim(tile, now) {
                skipped += 1;
                continue;
            }
            let url = reqwest::Url::parse_with_params(OVERPASS_URL, &[("data", query(tile))])
                .map_err(|e| SourceError::Decode(e.to_string()))?;
            let mut result = self.http.get_bytes(url.as_str()).await;
            for _ in 0..RATE_LIMIT_RETRIES {
                let Err(SourceError::RateLimited { retry_after }) = &result else { break };
                let wait = retry_after.unwrap_or(RATE_LIMIT_WAIT).max(std::time::Duration::from_secs(5));
                tracing::debug!(source = %self.descriptor.id, tile = ?tile, wait_s = wait.as_secs(), "overpass slot busy; waiting");
                tokio::time::sleep(wait).await;
                result = self.http.get_bytes(url.as_str()).await;
            }
            match result {
                Ok(bytes) => match decode(&bytes, &self.descriptor.id, now) {
                    Ok((obs, k)) => {
                        kept.add(&k);
                        out.extend(obs);
                    }
                    Err(err) => {
                        failed += 1;
                        self.release(tile);
                        tracing::warn!(source = %self.descriptor.id, %err, tile = ?tile, "tile did not decode");
                    }
                },
                Err(err) => {
                    failed += 1;
                    self.release(tile);
                    tracing::warn!(source = %self.descriptor.id, %err, tile = ?tile, "tile failed");
                }
            }
        }
        tracing::info!(source = %self.descriptor.id, tiles = tiles.len(), skipped, failed, ?kept, "power grid read");
        if out.is_empty() && failed > 0 {
            return Err(SourceError::Transport(format!("{failed} of {} tiles failed and none answered", tiles.len())));
        }
        Ok(out)
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    elements: Vec<Element>,
}

#[derive(Debug, Deserialize)]
struct Element {
    #[serde(rename = "type")]
    kind: String,
    id: i64,
    #[serde(default)]
    lat: Option<f64>,
    #[serde(default)]
    lon: Option<f64>,
    #[serde(default)]
    bounds: Option<Bounds>,
    #[serde(default)]
    geometry: Option<Vec<LatLon>>,
    #[serde(default)]
    members: Vec<Member>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Bounds {
    minlat: f64,
    minlon: f64,
    maxlat: f64,
    maxlon: f64,
}

#[derive(Debug, Deserialize)]
struct LatLon {
    lat: f64,
    lon: f64,
}

#[derive(Debug, Deserialize)]
struct Member {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    geometry: Option<Vec<LatLon>>,
}

#[derive(Debug, Default)]
pub struct Kept {
    pub lines: usize,
    pub substations: usize,
    pub substations_dropped: usize,
    pub plants: usize,
}

impl Kept {
    fn add(&mut self, other: &Kept) {
        self.lines += other.lines;
        self.substations += other.substations;
        self.substations_dropped += other.substations_dropped;
        self.plants += other.plants;
    }
}

/// `400000;275000` → the highest, in kV, and every level.
fn voltages_kv(s: &str) -> Vec<f64> {
    let mut v: Vec<f64> = s
        .split(';')
        .filter_map(|p| p.trim().parse::<f64>().ok())
        .map(|volts| volts / 1000.0)
        .collect();
    v.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    v
}

fn line_of(points: &[LatLon]) -> LineString<f64> {
    LineString(points.iter().map(|p| Coord { x: p.lon, y: p.lat }).collect())
}

fn is_closed(points: &[LatLon]) -> bool {
    points.len() >= 4 && points.first().is_some_and(|a| points.last().is_some_and(|b| a.lat == b.lat && a.lon == b.lon))
}

fn centroid_of(points: &[LatLon]) -> Option<(f64, f64)> {
    if points.is_empty() {
        return None;
    }
    let n = points.len() as f64;
    Some((points.iter().map(|p| p.lon).sum::<f64>() / n, points.iter().map(|p| p.lat).sum::<f64>() / n))
}

fn keep_substation(tags: &BTreeMap<String, String>) -> bool {
    match tags.get("substation").map(String::as_str) {
        Some("minor_distribution") => false,
        Some(_) => true,
        None => tags.get("voltage").is_some_and(|v| voltages_kv(v).first().is_some_and(|kv| *kv >= MIN_UNTYPED_SUBSTATION_KV)),
    }
}

pub fn decode(bytes: &[u8], source_id: &SourceId, now: DateTime<Utc>) -> Result<(Vec<Observation>, Kept), SourceError> {
    let response: Response = serde_json::from_slice(bytes).map_err(|e| SourceError::Decode(e.to_string()))?;
    let mut out = Vec::with_capacity(response.elements.len());
    let mut kept = Kept::default();
    for e in &response.elements {
        let Some(power) = e.tags.get("power").map(String::as_str) else { continue };
        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        let text = |k: &str| e.tags.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let voltages = e.tags.get("voltage").map(|v| voltages_kv(v)).unwrap_or_default();
        put("osm", serde_json::json!(format!("{}/{}", e.kind, e.id)));
        put("name", serde_json::json!(text("name")));
        put("operator", serde_json::json!(text("operator")));
        put("ref", serde_json::json!(text("ref")));
        if let Some(kv) = voltages.first() {
            put("voltage_kv", serde_json::json!(kv));
        }
        if voltages.len() > 1 {
            put("voltages_kv", serde_json::json!(voltages));
        }

        let (position, geom, label): ((f64, f64), Option<Geometry<f64>>, String) = match power {
            "line" => {
                let Some(points) = e.geometry.as_ref().filter(|g| g.len() >= 2) else { continue };
                put("kind", serde_json::json!("line"));
                put("cables", serde_json::json!(text("cables").and_then(|c| c.parse::<i64>().ok())));
                put("circuits", serde_json::json!(text("circuits").and_then(|c| c.parse::<i64>().ok())));
                put("frequency_hz", serde_json::json!(text("frequency").and_then(|c| c.parse::<f64>().ok())));
                put("line_type", serde_json::json!(text("line")));
                put("location", serde_json::json!(text("location")));
                let mid = &points[points.len() / 2];
                let label = match (voltages.first(), text("name"), text("ref")) {
                    (Some(kv), Some(n), _) => format!("{} kV line: {n}", kv),
                    (Some(kv), None, Some(r)) => format!("{} kV line {r}", kv),
                    (Some(kv), None, None) => format!("{} kV line", kv),
                    (None, Some(n), _) => format!("Power line: {n}"),
                    (None, None, _) => "Power line".to_string(),
                };
                kept.lines += 1;
                ((mid.lon, mid.lat), Some(Geometry::LineString(line_of(points))), label)
            }
            "substation" => {
                if !keep_substation(&e.tags) {
                    kept.substations_dropped += 1;
                    continue;
                }
                put("kind", serde_json::json!("substation"));
                put("substation_type", serde_json::json!(text("substation")));
                put("owner", serde_json::json!(text("owner")));
                let (position, geom) = match (e.lat, e.lon, e.geometry.as_ref()) {
                    (Some(lat), Some(lon), _) => ((lon, lat), None),
                    (_, _, Some(points)) if is_closed(points) => {
                        let Some(c) = centroid_of(points) else { continue };
                        (c, Some(Geometry::Polygon(Polygon::new(line_of(points), vec![]))))
                    }
                    (_, _, Some(points)) => {
                        let Some(c) = centroid_of(points) else { continue };
                        (c, None)
                    }
                    _ => continue,
                };
                let label = match (text("name"), voltages.first()) {
                    (Some(n), Some(kv)) => format!("{n} ({} kV)", kv),
                    (Some(n), None) => n,
                    (None, Some(kv)) => format!("{} kV substation", kv),
                    (None, None) => "Substation".to_string(),
                };
                kept.substations += 1;
                (position, geom, label)
            }
            "plant" => {
                put("kind", serde_json::json!("plant"));
                put("source", serde_json::json!(text("plant:source")));
                put("method", serde_json::json!(text("plant:method")));
                put("output_mw", serde_json::json!(text("plant:output:electricity").and_then(parse_mw)));
                put("repd_id", serde_json::json!(text("repd:id")));
                put("start_date", serde_json::json!(text("start_date")));
                let (position, geom) = match (e.geometry.as_ref(), &e.bounds) {
                    (Some(points), _) if is_closed(points) => {
                        let Some(c) = centroid_of(points) else { continue };
                        (c, Some(Geometry::Polygon(Polygon::new(line_of(points), vec![]))))
                    }
                    (Some(points), _) => {
                        let Some(c) = centroid_of(points) else { continue };
                        (c, None)
                    }
                    (None, Some(b)) => {
                        // A relation: its outline is the outer way that
                        // closes on itself, if one does.
                        let outline = e
                            .members
                            .iter()
                            .filter(|m| m.kind == "way" && (m.role == "outer" || m.role.is_empty()))
                            .filter_map(|m| m.geometry.as_ref())
                            .find(|g| is_closed(g))
                            .map(|g| Geometry::Polygon(Polygon::new(line_of(g), vec![])));
                        (((b.minlon + b.maxlon) / 2.0, (b.minlat + b.maxlat) / 2.0), outline)
                    }
                    _ => continue,
                };
                let source = text("plant:source");
                let label = match (text("name"), source.as_deref(), text("plant:output:electricity").and_then(parse_mw)) {
                    (Some(n), Some(s), Some(mw)) => format!("{n} ({s}, {} MW)", trim_mw(mw)),
                    (Some(n), Some(s), None) => format!("{n} ({s})"),
                    (Some(n), None, _) => n,
                    (None, Some(s), Some(mw)) => format!("{} MW {s} plant", trim_mw(mw)),
                    (None, Some(s), None) => format!("{s} plant"),
                    (None, None, _) => "Power plant".to_string(),
                };
                kept.plants += 1;
                (position, geom, label)
            }
            _ => continue,
        };
        let (lon, lat) = position;
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            continue;
        }
        let mut obs = Observation::new(source_id.clone(), EntityId::new(EntityKind::Feature, format!("osm:{}/{}", e.kind, e.id)), now, Quality::Live)
            .with_position(Position { lon, lat, alt_m: None, datum: AltitudeDatum::Geoid })
            .with_label(label)
            .with_attrs(serde_json::Value::Object(attrs));
        if let Some(g) = geom {
            obs = obs.with_geom(g);
        }
        out.push(obs);
    }
    Ok((out, kept))
}

/// `plant:output:electricity`: `49.9 MW`, `1200 MW`, `500 kW`, `2 GW`,
/// or a bare number that OSM's wiki says is megawatts.
fn parse_mw(s: String) -> Option<f64> {
    let s = s.trim().to_lowercase().replace(',', "");
    let (num, unit) = match s.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => (s[..i].trim().to_string(), s[i..].trim().to_string()),
        None => (s.clone(), "mw".to_string()),
    };
    let n: f64 = num.parse().ok()?;
    Some(match unit.as_str() {
        "kw" => n / 1000.0,
        "gw" => n * 1000.0,
        "w" => n / 1_000_000.0,
        _ => n,
    })
}

fn trim_mw(mw: f64) -> String {
    if mw.fract() == 0.0 { format!("{mw:.0}") } else { format!("{mw:.1}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE: &str = r#"{"elements":[
      {"type":"way","id":1,"bounds":{"minlat":51.0,"minlon":-1.0,"maxlat":51.1,"maxlon":-0.9},"geometry":[{"lat":51.0,"lon":-1.0},{"lat":51.05,"lon":-0.95},{"lat":51.1,"lon":-0.9}],"tags":{"power":"line","voltage":"400000;275000","cables":"6","circuits":"2","name":"Bramley - Fleet","operator":"National Grid"}},
      {"type":"node","id":2,"lat":51.2,"lon":-1.2,"tags":{"power":"substation","substation":"transmission","voltage":"400000;132000","name":"Bramley"}},
      {"type":"way","id":3,"bounds":{"minlat":51.3,"minlon":-1.3,"maxlat":51.31,"maxlon":-1.29},"geometry":[{"lat":51.3,"lon":-1.3},{"lat":51.3,"lon":-1.29},{"lat":51.31,"lon":-1.29},{"lat":51.3,"lon":-1.3}],"tags":{"power":"substation","voltage":"33000;11000","operator":"SSEN"}},
      {"type":"way","id":4,"bounds":{"minlat":51.4,"minlon":-1.4,"maxlat":51.41,"maxlon":-1.39},"geometry":[{"lat":51.4,"lon":-1.4},{"lat":51.4,"lon":-1.39},{"lat":51.41,"lon":-1.39},{"lat":51.4,"lon":-1.4}],"tags":{"power":"substation"}},
      {"type":"way","id":5,"bounds":{"minlat":51.5,"minlon":-1.5,"maxlat":51.51,"maxlon":-1.49},"geometry":[{"lat":51.5,"lon":-1.5},{"lat":51.5,"lon":-1.49},{"lat":51.51,"lon":-1.49},{"lat":51.5,"lon":-1.5}],"tags":{"power":"plant","plant:source":"solar","plant:output:electricity":"49.9 MW","name":"Westmill"}},
      {"type":"relation","id":6,"bounds":{"minlat":51.6,"minlon":-1.6,"maxlat":51.62,"maxlon":-1.58},"members":[{"type":"way","ref":60,"role":"outer","geometry":[{"lat":51.6,"lon":-1.6},{"lat":51.6,"lon":-1.58},{"lat":51.62,"lon":-1.58},{"lat":51.6,"lon":-1.6}]},{"type":"node","ref":61,"role":"generator","lat":51.61,"lon":-1.59}],"tags":{"type":"site","power":"plant","plant:source":"gas","plant:output:electricity":"2 GW","name":"Didcot B"}}
    ]}"#;

    #[test]
    fn lines_typed_substations_and_plants_are_kept_and_the_kiosk_is_not() {
        let (obs, kept) = decode(RESPONSE.as_bytes(), &SourceId::new("osm-power"), Utc::now()).unwrap();
        assert_eq!((kept.lines, kept.substations, kept.substations_dropped, kept.plants), (1, 2, 1, 2));
        assert_eq!(obs.len(), 5);
        let by = |k: &str| obs.iter().find(|o| o.entity.key == k).unwrap();
        let line = by("osm:way/1");
        assert_eq!(line.label.as_deref(), Some("400 kV line: Bramley - Fleet"));
        assert_eq!(line.attrs["voltage_kv"], 400.0);
        assert_eq!(line.attrs["voltages_kv"], serde_json::json!([400.0, 275.0]));
        assert!(matches!(line.geom, Some(Geometry::LineString(_))));
        let sub = by("osm:node/2");
        assert_eq!(sub.label.as_deref(), Some("Bramley (400 kV)"));
        assert!(sub.geom.is_none());
        let fenced = by("osm:way/3");
        assert_eq!(fenced.label.as_deref(), Some("33 kV substation"));
        assert!(matches!(fenced.geom, Some(Geometry::Polygon(_))));
        let solar = by("osm:way/5");
        assert_eq!(solar.label.as_deref(), Some("Westmill (solar, 49.9 MW)"));
        assert_eq!(solar.attrs["output_mw"], 49.9);
        let gas = by("osm:relation/6");
        assert_eq!(gas.label.as_deref(), Some("Didcot B (gas, 2000 MW)"));
        assert!(matches!(gas.geom, Some(Geometry::Polygon(_))), "the closed outer member is the outline");
        let p = gas.position.unwrap();
        assert!((p.lon + 1.59).abs() < 1e-9 && (p.lat - 51.61).abs() < 1e-9, "at the bounds centre");
    }

    #[test]
    fn output_is_read_in_whatever_unit_the_mapper_used() {
        assert_eq!(parse_mw("49.9 MW".into()), Some(49.9));
        assert_eq!(parse_mw("500 kW".into()), Some(0.5));
        assert_eq!(parse_mw("2 GW".into()), Some(2000.0));
        assert_eq!(parse_mw("1,200".into()), Some(1200.0));
        assert_eq!(parse_mw("unknown".into()), None);
    }

    #[test]
    fn the_query_names_the_tile_south_west_north_east_and_tiles_align() {
        let q = query(&BoundingBox::new(-2.0, 50.0, 0.0, 52.0));
        assert!(q.contains("(50.0000,-2.0000,52.0000,0.0000)"));
        assert!(q.contains("[\"substation\"!=\"minor_distribution\"]"));
        assert!(q.ends_with("out geom;"));
        let home = tiles(&BoundingBox::new(-2.5, 51.0, 0.5, 52.5), 2.0);
        assert_eq!(home.len(), 3 * 2, "columns at -4, -2, 0 and rows at 50, 52");
        assert_eq!(home[0].west, -4.0);
        assert_eq!(home[0].south, 50.0);
    }
}
