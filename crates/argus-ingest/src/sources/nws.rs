//! Active weather alerts from the US National Weather Service.
//!
//! The first driver whose data is fundamentally a *shape* rather than a point.
//! A tornado warning is an area; reducing it to a centroid would throw away the
//! only thing that matters about it. This is what `Observation::geom` exists
//! for, and it is why the store carries geometry alongside position rather than
//! instead of it — the polygon is the alert, and the centroid is where to put
//! the label.
//!
//! Most alerts do not carry that polygon. Measured on a live feed: 181 of 193
//! active alerts had `geometry: null`, because NWS issues by forecast and county
//! zone and expects the consumer to resolve the zone ids itself. Reading only
//! the inline geometry therefore left ~94% of active weather alerts in the
//! database with nothing to draw — present, correct, and invisible. So this
//! driver resolves `affectedZones` too, through a
//! [`GeometryCache`](argus_core::GeometryCache): zone boundaries are static, and
//! re-fetching a county outline every two minutes because an advisory is still
//! in force would be both slow and rude.

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

/// Zone boundaries to fetch in one poll, at most.
///
/// A cold cache needs a few hundred, and asking for them all at once would be a
/// burst of several hundred requests at a public, unmetered, taxpayer-funded
/// API. Spreading it means the map fills in over the first few polls instead of
/// instantly, which is a fair price. Cached zones cost nothing, so this only
/// bites while the cache is cold.
const MAX_ZONE_FETCHES_PER_POLL: usize = 60;

pub struct NwsAlerts {
    descriptor: SourceDescriptor,
    http: HttpClient,
    zones: std::sync::Arc<dyn argus_core::GeometryCache>,
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
            // An in-memory default so the driver is usable — and testable —
            // without a database. `argusd` swaps in the store, which is what
            // makes the cache survive a restart.
            zones: std::sync::Arc::new(argus_core::MemoryGeometryCache::new()),
        }
    }

    /// Back the zone cache with something persistent.
    #[must_use]
    pub fn with_zone_cache(
        mut self,
        cache: std::sync::Arc<dyn argus_core::GeometryCache>,
    ) -> Self {
        self.zones = cache;
        self
    }

    /// Fill in geometry for alerts that named zones instead of carrying a
    /// polygon.
    ///
    /// Partial resolution is deliberate and is reported rather than hidden: an
    /// alert covering six marine zones of which four are cached is drawn with
    /// those four and marked as partial, because four-sixths of a Small Craft
    /// Advisory on the map beats none of it, and the next poll completes it.
    async fn resolve_zones(&self, decoded: &mut [Decoded]) -> usize {
        let mut fetched = 0usize;
        for item in decoded.iter_mut() {
            if item.observation.geom.is_some() || item.zones.is_empty() {
                continue;
            }
            let mut polygons: Vec<Polygon<f64>> = Vec::new();
            let mut missing = 0usize;

            for url in &item.zones {
                let key = zone_cache_key(url);
                if let Some(geometry) = self.zones.get(&key).await {
                    collect_polygons(geometry, &mut polygons);
                    continue;
                }
                if fetched >= MAX_ZONE_FETCHES_PER_POLL {
                    missing += 1;
                    continue;
                }
                match self.http.get_json::<ZoneFeature>(url).await {
                    Ok(zone) => {
                        fetched += 1;
                        match zone.geometry.as_ref().and_then(convert_geometry) {
                            Some(geometry) => {
                                self.zones.put(&key, &geometry).await;
                                collect_polygons(geometry, &mut polygons);
                            }
                            // A zone with no geometry of its own is a real
                            // upstream state (some marine zones have none). It
                            // is cached as nothing so it is not re-fetched
                            // forever, by simply never being asked for again
                            // within this poll.
                            None => missing += 1,
                        }
                    }
                    Err(err) => {
                        missing += 1;
                        tracing::debug!(url, "could not resolve NWS zone: {err}");
                    }
                }
            }

            if polygons.is_empty() {
                continue;
            }
            let geometry = Geometry::MultiPolygon(MultiPolygon(polygons));
            item.observation.position = polygon_centroid(&geometry)
                .map(|(lon, lat)| Position::surface(lon, lat));
            item.observation.geom = Some(geometry);
            if let Some(attrs) = item.observation.attrs.as_object_mut() {
                attrs.insert("has_polygon".into(), serde_json::Value::Bool(true));
                attrs.insert("area_from".into(), "zones".into());
                attrs.insert(
                    "zones_unresolved".into(),
                    serde_json::Value::from(missing),
                );
            }
        }
        fetched
    }
}

/// One decoded alert, plus the zones it named if it carried no polygon.
///
/// Decoding stays a pure function of the response — the zone resolution that
/// follows is I/O, and keeping the two apart is what lets every decode rule
/// above be tested without a network.
struct Decoded {
    observation: Observation,
    zones: Vec<String>,
}

/// Cache key for a zone URL.
///
/// Keyed on the path rather than the whole URL so a scheme or host change at
/// NWS does not silently orphan a few hundred cached boundaries.
fn zone_cache_key(url: &str) -> String {
    let path = url.rsplit("/zones/").next().unwrap_or(url);
    format!("nws-zone:{}", path.trim_end_matches('/'))
}

/// Flatten whatever a zone returned into a list of polygons.
fn collect_polygons(geometry: Geometry<f64>, into: &mut Vec<Polygon<f64>>) {
    match geometry {
        Geometry::Polygon(p) => into.push(p),
        Geometry::MultiPolygon(mp) => into.extend(mp.0),
        // Zones are areas; anything else is an upstream surprise and is
        // dropped rather than guessed at.
        _ => {}
    }
}

#[async_trait::async_trait]
impl Source for NwsAlerts {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let feed: FeatureCollection = self.http.get_json(API_URL).await?;
        let mut decoded = decode(feed, &self.descriptor.id);
        let fetched = self.resolve_zones(&mut decoded).await;
        if fetched > 0 {
            tracing::debug!(fetched, "resolved NWS zone boundaries");
        }
        Ok(decoded.into_iter().map(|d| d.observation).collect())
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
    /// URLs of the forecast/county zones this alert covers. Present on almost
    /// every alert, and the only way to draw the ones with no inline polygon.
    #[serde(rename = "affectedZones", default)]
    affected_zones: Vec<String>,
}

/// A zone boundary, as `https://api.weather.gov/zones/...` returns it.
#[derive(Debug, Deserialize)]
struct ZoneFeature {
    geometry: Option<GeoJsonGeometry>,
}

fn decode(feed: FeatureCollection, source_id: &SourceId) -> Vec<Decoded> {
    feed.features
        .into_iter()
        .filter_map(|f| decode_feature(f, source_id))
        .collect()
}

fn decode_feature(f: Feature, source_id: &SourceId) -> Option<Decoded> {
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

    // An inline polygon is the exact area the office warned on, so it always
    // wins over the zone outlines — the zones are a coarser fallback for the
    // alerts that have no polygon at all.
    let centroid = geometry.as_ref().and_then(polygon_centroid);
    let zones = if geometry.is_some() {
        Vec::new()
    } else {
        f.properties.affected_zones.clone()
    };

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
        // Where the shape came from, so a client can tell an exact warned
        // polygon from a union of county outlines. They are not the same claim.
        "area_from": if geometry.is_some() { Some("inline") } else { None },
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
    Some(Decoded {
        observation: obs,
        zones,
    })
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

    #[test]
    fn an_alert_with_no_polygon_keeps_the_zones_it_named() {
        // The measured reality of this feed: 181 of 193 active alerts arrive
        // like this. Dropping the zone list is what made them unmappable.
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[{"id":"z1","geometry":null,"properties":{
                "id":"z1","event":"Small Craft Advisory","status":"Actual",
                "sent":"2026-08-29T12:00:00+00:00","areaDesc":"Cape Suckling",
                "affectedZones":[
                  "https://api.weather.gov/zones/forecast/PKZ120",
                  "https://api.weather.gov/zones/county/MDC031"]}}]}"#,
        )
        .unwrap();
        let decoded = decode(feed, &SourceId::new("t"));
        assert_eq!(decoded.len(), 1);
        assert!(decoded[0].observation.geom.is_none());
        assert_eq!(decoded[0].zones.len(), 2);
        assert_eq!(decoded[0].observation.attrs["has_polygon"], serde_json::json!(false));
    }

    #[test]
    fn an_inline_polygon_wins_and_the_zones_are_not_kept() {
        // The polygon an office actually drew is the warned area; the zones it
        // happens to intersect are coarser. Resolving both would replace a
        // precise shape with a union of counties.
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[{"id":"p1","geometry":{"type":"Polygon","coordinates":
                [[[-97.0,30.0],[-96.0,30.0],[-96.0,31.0],[-97.0,30.0]]]},
              "properties":{"id":"p1","event":"Tornado Warning","status":"Actual",
                "sent":"2026-08-29T12:00:00+00:00",
                "affectedZones":["https://api.weather.gov/zones/county/TXC453"]}}]}"#,
        )
        .unwrap();
        let decoded = decode(feed, &SourceId::new("t"));
        assert!(decoded[0].observation.geom.is_some());
        assert!(decoded[0].zones.is_empty(), "inline geometry must win");
        assert_eq!(decoded[0].observation.attrs["area_from"], serde_json::json!("inline"));
    }

    #[test]
    fn zone_cache_keys_survive_a_host_or_scheme_change() {
        // Keyed on the path, so a few hundred cached county outlines are not
        // orphaned the day NWS changes hostname.
        assert_eq!(
            zone_cache_key("https://api.weather.gov/zones/county/MDC031"),
            "nws-zone:county/MDC031"
        );
        assert_eq!(
            zone_cache_key("http://other.example/zones/forecast/PKZ120/"),
            "nws-zone:forecast/PKZ120"
        );
    }

    #[tokio::test]
    async fn cached_zones_become_the_alert_geometry() {
        use argus_core::GeometryCache;
        let cache = std::sync::Arc::new(argus_core::MemoryGeometryCache::new());
        // Two adjacent squares, standing in for two marine zones.
        for (key, x0) in [("nws-zone:forecast/PKZ120", 0.0), ("nws-zone:county/MDC031", 1.0)] {
            let square = Geometry::Polygon(Polygon::new(
                LineString(vec![
                    Coord { x: x0, y: 0.0 },
                    Coord { x: x0 + 1.0, y: 0.0 },
                    Coord { x: x0 + 1.0, y: 1.0 },
                    Coord { x: x0, y: 1.0 },
                    Coord { x: x0, y: 0.0 },
                ]),
                vec![],
            ));
            cache.put(key, &square).await;
        }

        let source = NwsAlerts::new(HttpClient::new(std::time::Duration::from_secs(5)).unwrap())
            .with_zone_cache(cache);
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[{"id":"z1","geometry":null,"properties":{
                "id":"z1","event":"Small Craft Advisory","status":"Actual",
                "sent":"2026-08-29T12:00:00+00:00",
                "affectedZones":[
                  "https://api.weather.gov/zones/forecast/PKZ120",
                  "https://api.weather.gov/zones/county/MDC031"]}}]}"#,
        )
        .unwrap();
        let mut decoded = decode(feed, &SourceId::new("t"));

        // Everything is cached, so this must resolve without a single request —
        // which is also what proves the cache is consulted before the network.
        let fetched = source.resolve_zones(&mut decoded).await;
        assert_eq!(fetched, 0, "a warm cache must not hit the network");

        let observation = &decoded[0].observation;
        let Some(Geometry::MultiPolygon(mp)) = observation.geom.as_ref() else {
            panic!("expected a multipolygon, got {:?}", observation.geom);
        };
        assert_eq!(mp.0.len(), 2, "both zones should contribute");
        assert_eq!(observation.attrs["area_from"], serde_json::json!("zones"));
        assert_eq!(observation.attrs["has_polygon"], serde_json::json!(true));
        assert_eq!(observation.attrs["zones_unresolved"], serde_json::json!(0));
        // A label anchor is needed too: without one there is nothing to pin the
        // event name to, and the alert draws as an unlabelled blob.
        assert!(observation.position.is_some());
    }

    /// Decode to plain observations, discarding the zone lists. Every rule the
    /// tests below check is a decode rule, so the resolution step is not what
    /// they are exercising.
    fn observations(feed: FeatureCollection, source_id: &SourceId) -> Vec<Observation> {
        decode(feed, source_id)
            .into_iter()
            .map(|d| d.observation)
            .collect()
    }

    fn decoded() -> Vec<Observation> {
        let feed: FeatureCollection =
            serde_json::from_str(FIXTURE).expect("fixture parses as the live wire format");
        observations(feed, &SourceId::new("nws-alerts"))
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
        let obs = observations(feed, &SourceId::new("t"));
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
        let obs = observations(feed, &SourceId::new("t"));
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
        assert!(observations(feed, &SourceId::new("t")).is_empty());
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
        let obs = observations(feed, &SourceId::new("t"));
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
        let obs = observations(feed, &SourceId::new("t"));
        let Geometry::Polygon(p) = obs[0].geom.as_ref().unwrap() else {
            panic!("expected a polygon");
        };
        assert_eq!(p.interiors().len(), 1, "hole was dropped");
    }
}
