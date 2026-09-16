//! HF propagation, as the WSPR network measures it: every path along
//! which a beacon was heard in the last ten minutes, from wspr.live.
//!
//! WSPR is a beacon mode: amateur stations transmit a callsign, a grid
//! locator and a power level at a fraction of a watt, and every receiver
//! that decodes one reports it. The reports are the best open record of
//! where HF radio is propagating right now — which bands are open, and
//! between which parts of the world. wspr.live keeps them all in
//! ClickHouse and answers SQL over HTTP, keyless; the whole database is
//! 4.4 million spots a day, so the question has to be narrow and
//! aggregated, and this one is: the paths with an end inside the area,
//! over the last ten minutes, one row per band and transmitter–receiver
//! pair, with the spot count and the best signal. 6,770 rows and 1.3 MB
//! in a third of a second for the British Isles.
//!
//! A [`EntityKind::Measure`], not an event: a path open ten minutes ago
//! is a reading of the ionosphere now, and a path from last Tuesday says
//! nothing about tonight, which is what the measure horizon says too.
//! Drawn as the great circle between the two locators — the radio went
//! that way, more or less — and keyed by band and the two locators, so a
//! path that stays open is one entity refreshed each poll. Grid locators
//! are four or six characters; the position is the square's centre,
//! which is what the network reports.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::geo::{destination, haversine_m, initial_bearing_deg};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use argus_core::BoundingBox;
use chrono::{DateTime, Utc};
use geo_types::{Coord, Geometry, LineString};
use serde::Deserialize;

const QUERY_URL: &str = "https://db1.wspr.live/";

/// WSPR transmits on two-minute slots; ten minutes is five of them.
const CADENCE_SECS: u64 = 600;
const WINDOW_MINUTES: u32 = 10;

/// A great circle is drawn with a vertex every so often; short paths are
/// a straight line.
const SEGMENT_KM: f64 = 250.0;

pub struct WsprPaths {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl WsprPaths {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("wspr"),
                layer_id: LayerId::new("hf-propagation"),
                display_name: "HF propagation paths (WSPR, wspr.live)".into(),
                kind: EntityKind::Measure,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Bounded,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "wspr.live (WSPRnet spot archive)".into(),
                    url: "https://wspr.live/".into(),
                    license: "CC BY-NC 4.0".into(),
                    notice: Some("WSPR spot data from wspr.live; WSPRnet and the amateur operators who report".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

/// The SQL for an area: paths with a transmitter or a receiver inside it,
/// aggregated per band and pair.
pub fn query(bbox: &BoundingBox) -> String {
    let inside = |lat: &str, lon: &str| {
        format!(
            "({lat} BETWEEN {:.3} AND {:.3} AND {lon} BETWEEN {:.3} AND {:.3})",
            bbox.south, bbox.north, bbox.west, bbox.east
        )
    };
    format!(
        "SELECT band, tx_loc, rx_loc, any(tx_sign) AS tx, any(rx_sign) AS rx, \
         any(tx_lat) AS tlat, any(tx_lon) AS tlon, any(rx_lat) AS rlat, any(rx_lon) AS rlon, \
         count() AS spots, max(snr) AS best_snr, any(distance) AS km, any(azimuth) AS azimuth, \
         max(power) AS power_dbm, max(time) AS last \
         FROM wspr.rx WHERE time > now() - INTERVAL {WINDOW_MINUTES} MINUTE AND ({} OR {}) \
         GROUP BY band, tx_loc, rx_loc FORMAT JSONEachRow",
        inside("tx_lat", "tx_lon"),
        inside("rx_lat", "rx_lon")
    )
}

#[async_trait::async_trait]
impl Source for WsprPaths {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let bbox = ctx.bbox.unwrap_or(BoundingBox::GLOBAL);
        let url = reqwest::Url::parse_with_params(QUERY_URL, &[("query", query(&bbox))]).map_err(|e| SourceError::Decode(e.to_string()))?;
        let bytes = self.http.get_bytes(url.as_str()).await?;
        let now = Utc::now();
        let decoded = decode(&bytes, &self.descriptor.id, now)?;
        tracing::info!(source = %self.descriptor.id, paths = decoded.observations.len(), degenerate = decoded.degenerate, "wspr paths read");
        Ok(decoded.observations)
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Row {
    band: i64,
    tx_loc: String,
    rx_loc: String,
    tx: Option<String>,
    rx: Option<String>,
    tlat: f64,
    tlon: f64,
    rlat: f64,
    rlon: f64,
    spots: u64,
    best_snr: Option<f64>,
    km: Option<f64>,
    azimuth: Option<f64>,
    power_dbm: Option<f64>,
    last: Option<String>,
}

#[derive(Debug)]
pub struct Decoded {
    pub observations: Vec<Observation>,
    /// Paths with both ends in the same square, or an end nowhere.
    pub degenerate: usize,
}

/// The band column is the frequency in MHz, truncated; the wavelength is
/// how operators name it.
pub fn band_name(band: i64) -> String {
    match band {
        -1 => "2200 m (LF)".into(),
        0 => "630 m (MF)".into(),
        1 => "160 m".into(),
        3 => "80 m".into(),
        5 => "60 m".into(),
        7 => "40 m".into(),
        10 => "30 m".into(),
        14 => "20 m".into(),
        18 => "17 m".into(),
        21 => "15 m".into(),
        24 => "12 m".into(),
        28 => "10 m".into(),
        50 => "6 m".into(),
        70 => "4 m".into(),
        144 => "2 m".into(),
        432 => "70 cm".into(),
        1296 => "23 cm".into(),
        other => format!("{other} MHz"),
    }
}

/// The great circle from one end to the other, a vertex every
/// [`SEGMENT_KM`], so a path to Australia is not drawn through the crust.
fn great_circle(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> LineString<f64> {
    let total_m = haversine_m(lat1, lon1, lat2, lon2);
    let steps = ((total_m / 1000.0) / SEGMENT_KM).ceil().max(1.0) as usize;
    let mut pts = Vec::with_capacity(steps + 1);
    pts.push(Coord { x: lon1, y: lat1 });
    if steps > 1 {
        // Walk the initial bearing from the start; re-derive the bearing
        // at each step so the walk follows the circle rather than a rhumb.
        let (mut lat, mut lon) = (lat1, lon1);
        let step_m = total_m / steps as f64;
        for _ in 1..steps {
            let bearing = initial_bearing_deg(lat, lon, lat2, lon2);
            let (nlat, nlon) = destination(lat, lon, bearing, step_m);
            lat = nlat;
            lon = nlon;
            pts.push(Coord { x: lon, y: lat });
        }
    }
    pts.push(Coord { x: lon2, y: lat2 });
    LineString(pts)
}

pub fn decode(bytes: &[u8], source_id: &SourceId, now: DateTime<Utc>) -> Result<Decoded, SourceError> {
    let text = std::str::from_utf8(bytes).map_err(|e| SourceError::Decode(e.to_string()))?;
    if text.starts_with("Code:") {
        return Err(SourceError::Decode(format!("wspr.live refused the query: {}", text.lines().next().unwrap_or("").chars().take(160).collect::<String>())));
    }
    let mut observations = Vec::new();
    let mut degenerate = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let r: Row = serde_json::from_str(line).map_err(|e| SourceError::Decode(e.to_string()))?;
        let ends_ok = (-90.0..=90.0).contains(&r.tlat) && (-180.0..=180.0).contains(&r.tlon) && (-90.0..=90.0).contains(&r.rlat) && (-180.0..=180.0).contains(&r.rlon);
        if !ends_ok || (r.tlat == 0.0 && r.tlon == 0.0) || (r.rlat == 0.0 && r.rlon == 0.0) || r.tx_loc == r.rx_loc {
            degenerate += 1;
            continue;
        }
        let at = r
            .last
            .as_deref()
            .and_then(|t| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S").ok())
            .map(|t| t.and_utc())
            .unwrap_or(now)
            .min(now);
        let km = r.km.unwrap_or_else(|| haversine_m(r.tlat, r.tlon, r.rlat, r.rlon) / 1000.0);
        let band = band_name(r.band);

        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("band", serde_json::json!(band));
        put("band_mhz", serde_json::json!(r.band));
        put("tx_callsign", serde_json::json!(r.tx));
        put("rx_callsign", serde_json::json!(r.rx));
        put("tx_locator", serde_json::json!(r.tx_loc));
        put("rx_locator", serde_json::json!(r.rx_loc));
        put("distance_km", serde_json::json!(km));
        put("bearing_deg", serde_json::json!(r.azimuth));
        put("spots", serde_json::json!(r.spots));
        put("best_snr_db", serde_json::json!(r.best_snr));
        put("tx_power_dbm", serde_json::json!(r.power_dbm));
        put("last_spot", serde_json::json!(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
        put("window_minutes", serde_json::json!(WINDOW_MINUTES));

        let label = match (&r.tx, &r.rx) {
            (Some(t), Some(x)) => format!("{band}: {t} → {x}, {} km", km.round()),
            _ => format!("{band}: {} → {}, {} km", r.tx_loc, r.rx_loc, km.round()),
        };
        // Anchored at the midpoint, so a card opens on the path rather than
        // on one operator's garden.
        let mid_bearing = initial_bearing_deg(r.tlat, r.tlon, r.rlat, r.rlon);
        let (mlat, mlon) = destination(r.tlat, r.tlon, mid_bearing, haversine_m(r.tlat, r.tlon, r.rlat, r.rlon) / 2.0);
        observations.push(
            Observation::new(source_id.clone(), EntityId::new(EntityKind::Measure, format!("wspr:{}:{}:{}", r.band, r.tx_loc, r.rx_loc)), at, Quality::Live)
                .with_position(Position { lon: mlon, lat: mlat, alt_m: None, datum: AltitudeDatum::AboveGround })
                .with_geom(Geometry::LineString(great_circle(r.tlat, r.tlon, r.rlat, r.rlon)))
                .with_label(label)
                .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    Ok(Decoded { observations, degenerate })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: &str = r#"{"band":7,"tx_loc":"IO91ta","rx_loc":"JN87aq","tx":"G4LRP","rx":"OE3GBB","tlat":51.021,"tlon":-0.375,"rlat":47.688,"rlon":16.042,"spots":3,"best_snr":-18,"km":1242,"azimuth":104,"power_dbm":23,"last":"2026-09-16 18:28:00"}
{"band":14,"tx_loc":"IO91","rx_loc":"QF56","tx":"G0ABC","rx":"VK2XYZ","tlat":51.5,"tlon":-1.0,"rlat":-33.5,"rlon":151.0,"spots":1,"best_snr":-27,"km":17000,"azimuth":60,"power_dbm":37,"last":"2026-09-16 18:26:00"}
{"band":7,"tx_loc":"IO91ta","rx_loc":"IO91ta","tx":"G4LRP","rx":"G4LRP/SDR","tlat":51.021,"tlon":-0.375,"rlat":51.021,"rlon":-0.375,"spots":5,"best_snr":10,"km":0,"azimuth":0,"power_dbm":23,"last":"2026-09-16 18:28:00"}
"#;

    #[test]
    fn a_path_is_a_measure_along_the_great_circle_keyed_by_band_and_squares() {
        let now: DateTime<Utc> = "2026-09-16T18:30:00Z".parse().unwrap();
        let d = decode(ROWS.as_bytes(), &SourceId::new("wspr"), now).unwrap();
        assert_eq!(d.degenerate, 1, "a station hearing itself is not a path");
        assert_eq!(d.observations.len(), 2);
        let short = &d.observations[0];
        assert_eq!(short.entity.key, "wspr:7:IO91ta:JN87aq");
        assert_eq!(short.entity.kind, EntityKind::Measure);
        assert_eq!(short.observed_at, "2026-09-16T18:28:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(short.attrs["band"], "40 m");
        assert_eq!(short.label.as_deref(), Some("40 m: G4LRP → OE3GBB, 1242 km"));
        let Some(Geometry::LineString(l)) = &short.geom else { panic!("a line") };
        assert_eq!(l.0.len(), 6, "1,242 km at a vertex every 250 km");
        let long = &d.observations[1];
        let Some(Geometry::LineString(l)) = &long.geom else { panic!("a line") };
        assert!(l.0.len() > 60, "{} vertices for 17,000 km", l.0.len());
        // The great circle to Sydney goes over the Middle East, not the Atlantic: a
        // vertex a third of the way should be well east of the start.
        let third = &l.0[l.0.len() / 3];
        assert!(third.x > 30.0 && third.y > 20.0, "third-way vertex at {:.1},{:.1}", third.x, third.y);
        let p = long.position.unwrap();
        assert!(p.lon > 50.0, "anchored midway, at {:.1},{:.1}", p.lon, p.lat);
    }

    #[test]
    fn the_query_asks_for_paths_with_an_end_in_the_area() {
        let q = query(&BoundingBox::new(-11.0, 49.5, 2.0, 61.0));
        assert!(q.contains("tx_lat BETWEEN 49.500 AND 61.000 AND tx_lon BETWEEN -11.000 AND 2.000"));
        assert!(q.contains("rx_lat BETWEEN 49.500"));
        assert!(q.ends_with("GROUP BY band, tx_loc, rx_loc FORMAT JSONEachRow"));
        let err = decode(b"Code: 184. DB::Exception: nope", &SourceId::new("wspr"), Utc::now()).unwrap_err();
        assert!(matches!(err, SourceError::Decode(_)));
    }
}
