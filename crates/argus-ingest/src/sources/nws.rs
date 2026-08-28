//! Active weather alerts from the US National Weather Service.
//!
//! The first driver whose data is fundamentally a *shape* rather than a point.
//! A tornado warning is an area; reducing it to a centroid would throw away the
//! only thing that matters about it. This is what `Observation::geom` exists
//! for, and it is why the store carries geometry alongside position rather than
//! instead of it — the polygon is the alert, and the centroid is where to put
//! the label.

use crate::http::HttpClient;
use argus_core::entity::{EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use geo_types::{Coord, Geometry, LineString, MultiPolygon, Polygon};
use serde::Deserialize;

const API_URL: &str = "https://api.weather.gov/alerts/active";

const CADENCE_SECS: u64 = 120;

pub struct NwsAlerts {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl NwsAlerts {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("nws-alerts"),
                layer_id: LayerId::new("weather-alerts"),
                display_name: "Weather alerts (NWS)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                // National coverage, delivered in one request. Filtering by
                // area would cost more requests than fetching the lot.
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "US National Weather Service".into(),
                    url: "https://www.weather.gov/".into(),
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
impl Source for NwsAlerts {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let feed: FeatureCollection = self.http.get_json(API_URL).await?;
        Ok(decode(feed, &self.descriptor.id))
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FeatureCollection {
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    id: Option<String>,
    geometry: Option<GeoJsonGeometry>,
    properties: Properties,
}

/// Only the shapes NWS actually emits for alerts.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum GeoJsonGeometry {
    Polygon { coordinates: Vec<Vec<[f64; 2]>> },
    MultiPolygon { coordinates: Vec<Vec<Vec<[f64; 2]>>> },
}

#[derive(Debug, Deserialize)]
struct Properties {
    id: Option<String>,
    event: Option<String>,
    headline: Option<String>,
    description: Option<String>,
    instruction: Option<String>,
    severity: Option<String>,
    certainty: Option<String>,
    urgency: Option<String>,
    /// "Actual", "Exercise", "Test", ...
    status: Option<String>,
    /// "Alert", "Update", "Cancel", ...
    #[serde(rename = "messageType")]
    message_type: Option<String>,
    /// When the hazard begins.
    onset: Option<String>,
    /// When the alert stops being in force.
    expires: Option<String>,
    /// When the office issued it.
    sent: Option<String>,
    #[serde(rename = "senderName")]
    sender_name: Option<String>,
    #[serde(rename = "areaDesc")]
    area_desc: Option<String>,
}

fn decode(feed: FeatureCollection, source_id: &SourceId) -> Vec<Observation> {
    feed.features
        .into_iter()
        .filter_map(|f| decode_feature(f, source_id))
        .collect()
}

fn decode_feature(f: Feature, source_id: &SourceId) -> Option<Observation> {
    let key = f.properties.id.clone().or_else(|| f.id.clone())?;

    // Exercises and tests are broadcast on the same feed as real warnings and
    // are indistinguishable by shape. Publishing them would put a fictional
    // tornado warning on the map.
    if f.properties
        .status
        .as_deref()
        .is_some_and(|s| !s.eq_ignore_ascii_case("actual"))
    {
        return None;
    }

    // A cancellation is a statement about an alert, not an alert. Ingesting it
    // as one would leave a phantom warning standing after the real one cleared.
    if f.properties
        .message_type
        .as_deref()
        .is_some_and(|m| m.eq_ignore_ascii_case("cancel"))
    {
        return None;
    }

    // `sent` is when the office issued it. `onset` is when the weather starts,
    // which may be hours ahead — using that would place the observation in the
    // future and break every lag calculation downstream.
    let observed_at = f
        .properties
        .sent
        .as_deref()
        .and_then(parse_time)
        .or_else(|| f.properties.onset.as_deref().and_then(parse_time))?;

    let geometry = f.geometry.as_ref().and_then(convert_geometry);

    // Many alerts are issued by zone or county code with no polygon attached.
    // They are real and worth keeping, but there is nothing to draw, so they
    // are recorded with their area description and no geometry rather than
    // being given an invented shape.
    let centroid = geometry.as_ref().and_then(polygon_centroid);

    let label = f
        .properties
        .event
        .clone()
        .or_else(|| f.properties.headline.clone())
        .unwrap_or_else(|| "Weather alert".into());

    let attrs = serde_json::json!({
        "event": f.properties.event,
        "headline": f.properties.headline,
        "description": f.properties.description,
        "instruction": f.properties.instruction,
        "severity": f.properties.severity,
        "certainty": f.properties.certainty,
        "urgency": f.properties.urgency,
        "area": f.properties.area_desc,
        "office": f.properties.sender_name,
        "onset": f.properties.onset.as_deref().and_then(parse_time),
        "expires": f.properties.expires.as_deref().and_then(parse_time),
        "has_polygon": geometry.is_some(),
    });

    let mut obs = Observation::new(
        source_id.clone(),
        EntityId::new(EntityKind::Event, key),
        observed_at,
        Quality::Live,
    )
    .with_label(label)
    .with_attrs(attrs);

    if let Some(g) = geometry {
        obs = obs.with_geom(g);
    }
    if let Some((lon, lat)) = centroid {
        obs = obs.with_position(Position::surface(lon, lat));
    }
    Some(obs)
}

fn ring(coords: &[[f64; 2]]) -> LineString<f64> {
    LineString(coords.iter().map(|c| Coord { x: c[0], y: c[1] }).collect())
}

fn convert_geometry(g: &GeoJsonGeometry) -> Option<Geometry<f64>> {
    match g {
        GeoJsonGeometry::Polygon { coordinates } => {
            let (outer, holes) = coordinates.split_first()?;
            Some(Geometry::Polygon(Polygon::new(
                ring(outer),
                holes.iter().map(|h| ring(h)).collect(),
            )))
        }
        GeoJsonGeometry::MultiPolygon { coordinates } => {
            let polys: Vec<Polygon<f64>> = coordinates
                .iter()
                .filter_map(|rings| {
                    let (outer, holes) = rings.split_first()?;
                    Some(Polygon::new(
                        ring(outer),
                        holes.iter().map(|h| ring(h)).collect(),
                    ))
                })
                .collect();
            (!polys.is_empty()).then_some(Geometry::MultiPolygon(MultiPolygon(polys)))
        }
    }
}

/// Mean of the outer ring's vertices — a label anchor, not a true centroid.
///
/// Good enough to hang a marker on and far cheaper than an area-weighted
/// centroid. It is deliberately not presented as the alert's location: the
/// polygon is the alert.
fn polygon_centroid(g: &Geometry<f64>) -> Option<(f64, f64)> {
    let exterior = match g {
        Geometry::Polygon(p) => p.exterior(),
        Geometry::MultiPolygon(mp) => mp.0.first()?.exterior(),
        _ => return None,
    };
    let pts: Vec<&Coord<f64>> = exterior.0.iter().collect();
    if pts.is_empty() {
        return None;
    }
    let n = pts.len() as f64;
    let lon = pts.iter().map(|c| c.x).sum::<f64>() / n;
    let lat = pts.iter().map(|c| c.y).sum::<f64>() / n;
    Some((lon, lat))
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../fixtures/nws_alerts.json");

    fn decoded() -> Vec<Observation> {
        let feed: FeatureCollection =
            serde_json::from_str(FIXTURE).expect("fixture parses as the live wire format");
        decode(feed, &SourceId::new("nws-alerts"))
    }

    #[test]
    fn the_fixture_decodes_into_events() {
        let obs = decoded();
        assert!(!obs.is_empty(), "fixture produced nothing");
        assert!(obs.iter().all(|o| o.entity.kind == EntityKind::Event));
        assert!(obs.iter().all(|o| o.is_meaningful()));
    }

    #[test]
    fn polygons_survive_as_geometry_not_as_a_point() {
        // The whole reason this driver exists: an alert IS its area. Reducing
        // it to a centroid would throw away the only thing that matters.
        let obs = decoded();
        let with_shape: Vec<_> = obs.iter().filter(|o| o.geom.is_some()).collect();
        assert!(
            !with_shape.is_empty(),
            "no alert in the fixture carried a polygon"
        );
        for o in &with_shape {
            match o.geom.as_ref().unwrap() {
                Geometry::Polygon(p) => {
                    assert!(p.exterior().0.len() >= 4, "degenerate ring");
                }
                Geometry::MultiPolygon(mp) => assert!(!mp.0.is_empty()),
                other => panic!("unexpected geometry: {other:?}"),
            }
            // And it still has a point to hang a label on.
            assert!(o.position.is_some());
        }
    }

    #[test]
    fn the_label_anchor_falls_inside_the_alert_area() {
        // A centroid outside its own polygon would put the marker somewhere the
        // warning does not apply.
        let obs = decoded();
        let o = obs
            .iter()
            .find(|o| o.geom.is_some())
            .expect("an alert with a polygon");
        let p = o.position.unwrap();
        let Geometry::Polygon(poly) = o.geom.as_ref().unwrap() else {
            return;
        };
        let xs: Vec<f64> = poly.exterior().0.iter().map(|c| c.x).collect();
        let ys: Vec<f64> = poly.exterior().0.iter().map(|c| c.y).collect();
        let (min_x, max_x) = (
            xs.iter().cloned().fold(f64::MAX, f64::min),
            xs.iter().cloned().fold(f64::MIN, f64::max),
        );
        let (min_y, max_y) = (
            ys.iter().cloned().fold(f64::MAX, f64::min),
            ys.iter().cloned().fold(f64::MIN, f64::max),
        );
        assert!(
            p.lon >= min_x && p.lon <= max_x && p.lat >= min_y && p.lat <= max_y,
            "anchor {:?} outside the alert's own bounds",
            (p.lon, p.lat)
        );
    }

    #[test]
    fn zone_based_alerts_without_a_polygon_are_kept_not_invented() {
        // Most alerts are issued by county or zone with no geometry. They are
        // real; giving them a made-up shape would be worse than having none.
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[{"id":"zone1","geometry":null,"properties":{
                "id":"zone1","event":"Flood Watch","status":"Actual","messageType":"Alert",
                "sent":"2026-08-28T12:00:00-05:00","areaDesc":"Travis County"}}]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"));
        assert_eq!(obs.len(), 1);
        assert!(obs[0].geom.is_none());
        assert!(obs[0].position.is_none());
        assert_eq!(obs[0].attrs["has_polygon"], serde_json::json!(false));
        // Still meaningful — it carries attributes worth recording.
        assert!(obs[0].is_meaningful());
    }

    #[test]
    fn exercises_and_tests_are_not_published_as_real_warnings() {
        // These ride the same feed and are shaped identically to live alerts.
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[
              {"id":"a","geometry":null,"properties":{"id":"a","event":"Tornado Warning",
               "status":"Test","messageType":"Alert","sent":"2026-08-28T12:00:00-05:00"}},
              {"id":"b","geometry":null,"properties":{"id":"b","event":"Tornado Warning",
               "status":"Exercise","messageType":"Alert","sent":"2026-08-28T12:00:00-05:00"}},
              {"id":"c","geometry":null,"properties":{"id":"c","event":"Tornado Warning",
               "status":"Actual","messageType":"Alert","sent":"2026-08-28T12:00:00-05:00"}}
            ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"));
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].entity.key, "c");
    }

    #[test]
    fn cancellations_are_not_ingested_as_alerts() {
        // A cancel is a statement about an alert, not an alert. Ingesting it
        // would leave a phantom warning standing after the real one cleared.
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[{"id":"x","geometry":null,"properties":{"id":"x",
               "event":"Severe Thunderstorm Warning","status":"Actual","messageType":"Cancel",
               "sent":"2026-08-28T12:00:00-05:00"}}]}"#,
        )
        .unwrap();
        assert!(decode(feed, &SourceId::new("t")).is_empty());
    }

    #[test]
    fn observed_at_is_when_it_was_issued_not_when_the_weather_starts() {
        // onset can be hours ahead; using it would place the observation in the
        // future and poison every lag calculation downstream.
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[{"id":"x","geometry":null,"properties":{"id":"x",
               "event":"Winter Storm Watch","status":"Actual","messageType":"Alert",
               "sent":"2026-08-28T12:00:00Z","onset":"2026-08-29T06:00:00Z"}}]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"));
        assert_eq!(obs[0].observed_at, parse_time("2026-08-28T12:00:00Z").unwrap());
        assert!(obs[0].observed_at < Utc::now());
    }

    #[test]
    fn polygons_with_holes_keep_their_holes() {
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[{"id":"x","geometry":{"type":"Polygon","coordinates":[
                 [[-97.0,30.0],[-96.0,30.0],[-96.0,31.0],[-97.0,31.0],[-97.0,30.0]],
                 [[-96.8,30.2],[-96.2,30.2],[-96.2,30.8],[-96.8,30.8],[-96.8,30.2]]
               ]},"properties":{"id":"x","event":"Test","status":"Actual",
               "messageType":"Alert","sent":"2026-08-28T12:00:00Z"}}]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"));
        let Geometry::Polygon(p) = obs[0].geom.as_ref().unwrap() else {
            panic!("expected a polygon");
        };
        assert_eq!(p.interiors().len(), 1, "hole was dropped");
    }
}
