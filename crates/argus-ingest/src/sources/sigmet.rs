//! International SIGMETs — significant meteorological hazards to aviation.
//!
//! A SIGMET is a warning issued by a Meteorological Watch Office about weather
//! dangerous to aircraft in flight: severe turbulence, icing, thunderstorms,
//! volcanic ash, dust storms. Each names a polygon, a flight-level band, and a
//! validity window.
//!
//! Two things make them worth carrying as their own layer rather than folding
//! into `weather-alerts`. They are three-dimensional — a SIGMET between FL270
//! and FL300 is not a hazard to anything below it, and the base and top are the
//! most operationally useful numbers on the message. And they expire: an alert
//! whose `validTimeTo` has passed is not a current hazard, whatever the feed
//! still lists.
//!
//! The source is the US Aviation Weather Center, which aggregates SIGMETs from
//! watch offices worldwide, so the coverage is global despite the operator.

use crate::http::HttpClient;
use argus_core::entity::{EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, TimeZone, Utc};
use geo_types::{Coord, Geometry, LineString, Polygon};
use serde::Deserialize;

const FEED_URL: &str = "https://aviationweather.gov/api/data/isigmet?format=json";

/// SIGMETs are issued and amended on the hour and half hour, and a new one can
/// appear at any time. Five minutes is well inside that without being wasteful.
const CADENCE_SECS: u64 = 300;

pub struct Sigmets {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl Sigmets {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("awc-sigmet"),
                layer_id: LayerId::new("sigmets"),
                display_name: "Aviation hazards (SIGMET)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "NOAA Aviation Weather Center".into(),
                    url: "https://aviationweather.gov/".into(),
                    license: "Public domain (US Government)".into(),
                    notice: None,
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for Sigmets {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let feed: Vec<Advisory> = self.http.get_json(FEED_URL).await?;
        Ok(decode(feed, &self.descriptor.id, Utc::now()))
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Advisory {
    #[serde(rename = "icaoId")]
    icao_id: Option<String>,
    #[serde(rename = "firId")]
    fir_id: Option<String>,
    #[serde(rename = "firName")]
    fir_name: Option<String>,
    #[serde(rename = "seriesId")]
    series_id: Option<String>,
    /// TURB, ICE, TS, MTW, VA, DS, TC, …
    hazard: Option<String>,
    /// SEV, MOD, EMBD, OBSC, …
    qualifier: Option<String>,
    /// Flight levels in feet.
    base: Option<i64>,
    top: Option<i64>,
    /// Unix seconds.
    #[serde(rename = "validTimeFrom")]
    valid_from: Option<i64>,
    #[serde(rename = "validTimeTo")]
    valid_to: Option<i64>,
    #[serde(rename = "receiptTime")]
    receipt_time: Option<String>,
    dir: Option<String>,
    /// A string, always — the feed sends `"05"`, zero-padded, never a number.
    /// Measured across a live pull: 124 strings, 19 nulls, no integers. The
    /// first record in the feed has it null, so a fixture written from that
    /// record alone will not catch this.
    spd: Option<String>,
    /// One ring, or several.
    ///
    /// The feed switches shape on the `geom` field: `"AREA"` gives a flat list
    /// of points, `"AREAS"` gives a list of rings — one advisory covering
    /// several separate regions, which is a real thing a watch office issues.
    /// 141 of 143 advisories in a live pull were the flat form, so a fixture
    /// built from the head of the feed will not show this.
    coords: Option<Coords>,
    #[serde(rename = "rawSigmet")]
    raw: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Coords {
    One(Vec<LonLat>),
    Many(Vec<Vec<LonLat>>),
}

impl Coords {
    /// Every ring, however the feed spelled it.
    fn rings(&self) -> Vec<&Vec<LonLat>> {
        match self {
            Self::One(ring) => vec![ring],
            Self::Many(rings) => rings.iter().collect(),
        }
    }
}

/// A vertex.
///
/// Both optional, because they are not always present: one advisory in a live
/// pull of 143 carried `{"lon": null, "lat": 6.633}`. Declared as `f64` that
/// single vertex failed the whole untagged enum, which failed the whole record,
/// which failed the entire poll — 142 perfectly good hazard areas lost to one
/// null. Tolerance belongs at the smallest element, not the largest.
#[derive(Debug, Deserialize)]
struct LonLat {
    lon: Option<f64>,
    lat: Option<f64>,
}

fn decode(feed: Vec<Advisory>, source_id: &SourceId, now: DateTime<Utc>) -> Vec<Observation> {
    feed.into_iter()
        .filter_map(|a| decode_advisory(a, source_id, now))
        .collect()
}

fn decode_advisory(
    a: Advisory,
    source_id: &SourceId,
    now: DateTime<Utc>,
) -> Option<Observation> {
    // Three points is the minimum that bounds an area; anything less is not a
    // polygon and PostGIS would refuse it.
    let rings: Vec<&Vec<LonLat>> = a
        .coords
        .as_ref()
        .map(Coords::rings)
        .unwrap_or_default()
        .into_iter()
        // A ring with a null vertex is dropped whole rather than closed over
        // the gap. A hazard polygon missing a corner is a *different* polygon,
        // and quietly reshaping the area a SIGMET warns about would be worse
        // than not drawing it.
        .filter(|r| r.iter().all(|c| c.lon.is_some() && c.lat.is_some()))
        .filter(|r| r.len() >= 3)
        .collect();
    if rings.is_empty() {
        return None;
    }

    // Expired advisories are dropped here rather than left to the freshness
    // horizon. The horizon asks "how long ago was this observed"; a SIGMET
    // carries its own answer to "is this still in force", and that is the one
    // that matters — a superseded turbulence warning is not a hazard, however
    // recently the feed republished it.
    let valid_to = a.valid_to.and_then(|t| Utc.timestamp_opt(t, 0).single());
    if valid_to.is_some_and(|t| t < now) {
        return None;
    }

    // Observed when it was issued, not when we fetched it: `validTimeFrom` is
    // the moment the hazard is declared to begin.
    let observed_at = a
        .valid_from
        .and_then(|t| Utc.timestamp_opt(t, 0).single())
        .or_else(|| a.receipt_time.as_deref().and_then(|s| s.parse().ok()))
        .unwrap_or(now);

    let polygons: Vec<Polygon<f64>> = rings.iter().map(|r| ring_to_polygon(r)).collect();
    let geometry = if polygons.len() == 1 {
        Geometry::Polygon(polygons.into_iter().next().expect("checked non-empty"))
    } else {
        Geometry::MultiPolygon(geo_types::MultiPolygon(polygons))
    };
    // The label goes on the first area; a multi-area advisory has no single
    // sensible point and inventing one between two regions would put it in the
    // sea between them.
    let (lon, lat) = centroid(rings[0]);

    // FIR + series identifies an advisory; a re-issue of the same series
    // supersedes the previous one, which is exactly the upsert we want.
    let key = format!(
        "{}:{}",
        a.fir_id.as_deref().or(a.icao_id.as_deref()).unwrap_or("unknown"),
        a.series_id.as_deref().unwrap_or("?")
    );

    let hazard = a.hazard.as_deref().unwrap_or("hazard");
    let label = match a.qualifier.as_deref() {
        Some(q) if !q.is_empty() => format!("{q} {hazard}"),
        _ => hazard.to_string(),
    };

    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("hazard", serde_json::json!(a.hazard));
    put("qualifier", serde_json::json!(a.qualifier));
    put("fir", serde_json::json!(a.fir_name.or(a.fir_id)));
    put("issuing_office", serde_json::json!(a.icao_id));
    // Flight levels, kept as given. A SIGMET's vertical extent is the
    // difference between a warning that concerns an airliner and one that does
    // not, and flattening it to a 2-D polygon throws that away.
    put("base_ft", serde_json::json!(a.base));
    put("top_ft", serde_json::json!(a.top));
    put("movement_dir", serde_json::json!(a.dir));
    // Parsed to a number where it is one, so a client can compare it, but
    // kept out of the attrs entirely rather than stored as "05".
    put(
        "movement_kt",
        serde_json::json!(a.spd.as_deref().and_then(|s| s.trim().parse::<i64>().ok())),
    );
    put("valid_from", serde_json::json!(a.valid_from.map(|t| t.to_string())));
    put("valid_to", serde_json::json!(a.valid_to.map(|t| t.to_string())));
    put("raw", serde_json::json!(a.raw));

    Some(
        Observation::new(
            source_id.clone(),
            EntityId::new(EntityKind::Event, key),
            observed_at,
            Quality::Live,
        )
        .with_position(Position {
            lon,
            lat,
            alt_m: None,
            datum: argus_core::entity::AltitudeDatum::Barometric,
        })
        .with_geom(geometry)
        .with_label(label)
        .with_attrs(serde_json::Value::Object(attrs)),
    )
}

fn ring_to_polygon(ring: &[LonLat]) -> Polygon<f64> {
    let mut coords: Vec<Coord<f64>> = ring
        .iter()
        .filter_map(|c| Some(Coord { x: c.lon?, y: c.lat? }))
        .collect();
    // A polygon ring must close. The feed usually repeats the first point, but
    // not always, and an unclosed ring is rejected by PostGIS rather than
    // quietly fixed.
    if coords.first() != coords.last()
        && let Some(first) = coords.first().copied()
    {
        coords.push(first);
    }
    Polygon::new(LineString(coords), vec![])
}

/// A representative point for the polygon, so a client that draws only points
/// still has somewhere to put the label.
fn centroid(ring: &[LonLat]) -> (f64, f64) {
    let n = ring.len() as f64;
    (
        ring.iter().filter_map(|c| c.lon).sum::<f64>() / n,
        ring.iter().filter_map(|c| c.lat).sum::<f64>() / n,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advisory(valid_to: i64, coords: usize) -> Advisory {
        Advisory {
            icao_id: Some("FAOR".into()),
            fir_id: Some("FAJO".into()),
            fir_name: Some("FAJO JOHANNESBURG OCEANIC".into()),
            series_id: Some("N01".into()),
            hazard: Some("TURB".into()),
            qualifier: Some("SEV".into()),
            base: Some(27_000),
            top: Some(30_000),
            valid_from: Some(1_788_357_600),
            valid_to: Some(valid_to),
            receipt_time: None,
            dir: Some("NW".into()),
            spd: Some("05".into()),
            coords: Some(Coords::One(
                (0..coords)
                    .map(|i| LonLat { lon: Some(i as f64), lat: Some(-50.0 - i as f64) })
                    .collect(),
            )),
            raw: Some("FAJO SIGMET N01 VALID 021400/021800".into()),
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid instant")
    }

    #[test]
    fn an_expired_advisory_is_dropped_rather_than_left_to_the_freshness_horizon() {
        // The horizon answers "how long ago was this seen"; a SIGMET answers
        // "is this still in force", and only the second is the real question.
        let feed = vec![advisory(1_788_372_000, 4)];
        assert!(decode(feed, &SourceId::new("awc-sigmet"), at(1_788_400_000)).is_empty());

        let feed = vec![advisory(1_788_372_000, 4)];
        assert_eq!(decode(feed, &SourceId::new("awc-sigmet"), at(1_788_360_000)).len(), 1);
    }

    #[test]
    fn a_ring_the_feed_left_open_is_closed_rather_than_rejected_by_postgis() {
        let obs = decode(vec![advisory(i64::MAX, 4)], &SourceId::new("awc-sigmet"), at(0));
        let Some(Geometry::Polygon(p)) = obs[0].geom.as_ref() else {
            panic!("expected a polygon");
        };
        let ring = &p.exterior().0;
        assert_eq!(ring.first(), ring.last(), "an unclosed ring is not a polygon");
        assert_eq!(ring.len(), 5, "four distinct points plus the closing repeat");
    }

    #[test]
    fn a_degenerate_ring_is_not_a_polygon() {
        // Two points cannot bound an area; PostGIS would refuse it and the
        // advisory would be lost noisily rather than skipped quietly.
        let obs = decode(vec![advisory(i64::MAX, 2)], &SourceId::new("awc-sigmet"), at(0));
        assert!(obs.is_empty());
    }

    #[test]
    fn the_vertical_band_survives_because_it_is_what_makes_a_sigmet_actionable() {
        let obs = decode(vec![advisory(i64::MAX, 4)], &SourceId::new("awc-sigmet"), at(0));
        assert_eq!(obs[0].attrs["base_ft"], serde_json::json!(27_000));
        assert_eq!(obs[0].attrs["top_ft"], serde_json::json!(30_000));
        assert_eq!(obs[0].label.as_deref(), Some("SEV TURB"));
    }

    #[test]
    fn an_advisory_covering_several_areas_becomes_a_multipolygon() {
        // `geom: "AREAS"` nests the rings one level deeper. Two of 143
        // advisories in a live pull were this shape, so decoding the flat form
        // alone fails the whole poll rather than one record.
        let mut a = advisory(i64::MAX, 4);
        a.coords = Some(Coords::Many(vec![
            (0..4).map(|i| LonLat { lon: Some(i as f64), lat: Some(10.0 + i as f64) }).collect(),
            (0..4)
                .map(|i| LonLat { lon: Some(50.0 + i as f64), lat: Some(-10.0 - i as f64) })
                .collect(),
        ]));
        let obs = decode(vec![a], &SourceId::new("awc-sigmet"), at(0));
        let Some(Geometry::MultiPolygon(mp)) = obs[0].geom.as_ref() else {
            panic!("expected a multipolygon");
        };
        assert_eq!(mp.0.len(), 2);
        // Labelled on the first area rather than between the two, which would
        // be somewhere neither hazard is.
        let position = obs[0].position.as_ref().expect("a position");
        assert!(position.lat > 0.0, "the point belongs to the first area");
    }

    #[test]
    fn one_null_vertex_costs_its_own_area_and_not_the_whole_feed() {
        // Exactly the live case: `{"lon": null, "lat": 6.633}` in one ring of a
        // multi-area advisory. The other area must still be drawn, and the
        // damaged one must be dropped rather than closed over the gap — a
        // hazard polygon missing a corner is a different polygon.
        let mut a = advisory(i64::MAX, 4);
        a.coords = Some(Coords::Many(vec![
            vec![
                LonLat { lon: None, lat: Some(6.633) },
                LonLat { lon: Some(1.0), lat: Some(2.0) },
                LonLat { lon: Some(3.0), lat: Some(4.0) },
            ],
            (0..4)
                .map(|i| LonLat { lon: Some(50.0 + i as f64), lat: Some(-10.0 - i as f64) })
                .collect(),
        ]));
        let obs = decode(vec![a], &SourceId::new("awc-sigmet"), at(0));
        assert_eq!(obs.len(), 1, "the advisory survives");
        let Some(Geometry::Polygon(_)) = obs[0].geom.as_ref() else {
            panic!("one good ring should be a plain polygon, not a multipolygon of one");
        };
    }

    #[test]
    fn a_zero_padded_string_speed_becomes_a_number() {
        // The feed sends movement speed as `"05"`. Declaring it as an integer
        // made every poll fail to decode — loudly, which is how it was caught,
        // but the attr must still end up comparable rather than a string.
        let obs = decode(vec![advisory(i64::MAX, 4)], &SourceId::new("awc-sigmet"), at(0));
        assert_eq!(obs[0].attrs["movement_kt"], serde_json::json!(5));
        assert_eq!(obs[0].attrs["movement_dir"], serde_json::json!("NW"));
    }

    #[test]
    fn the_key_is_the_fir_and_series_so_a_reissue_supersedes_rather_than_duplicates() {
        let obs = decode(vec![advisory(i64::MAX, 4)], &SourceId::new("awc-sigmet"), at(0));
        assert_eq!(obs[0].entity.key, "FAJO:N01");
    }
}
