//! USGS earthquakes.
//!
//! The simplest possible real driver, and the reference for every one that
//! follows: fetch, decode, normalise, return. No caching, no retries, no
//! timers — the scheduler owns all of that.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;

/// Trailing 24 hours, all magnitudes.
const FEED_URL: &str = "https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/all_day.geojson";

/// USGS revises magnitudes and locations for minutes to hours after an event,
/// so re-polling genuinely produces new information rather than the same rows.
const CADENCE_SECS: u64 = 300;

pub struct UsgsEarthquakes {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl UsgsEarthquakes {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("usgs-quakes"),
                layer_id: LayerId::new("earthquakes"),
                display_name: "Earthquakes (USGS, 24h)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "U.S. Geological Survey".into(),
                    url: "https://earthquake.usgs.gov/".into(),
                    license: "Public domain (USGS)".into(),
                    notice: None,
                },
                base_quality: Quality::Live,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for UsgsEarthquakes {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let feed: FeatureCollection = self.http.get_json(FEED_URL).await?;
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
    id: String,
    properties: Properties,
    geometry: Option<Geometry>,
}

#[derive(Debug, Deserialize)]
struct Properties {
    mag: Option<f64>,
    place: Option<String>,
    /// Milliseconds since the epoch.
    time: Option<i64>,
    updated: Option<i64>,
    url: Option<String>,
    /// "earthquake", "quarry blast", "explosion", ...
    #[serde(rename = "type")]
    event_type: Option<String>,
    tsunami: Option<i64>,
    /// Number of seismic stations that reported it — a rough confidence proxy.
    nst: Option<i64>,
    /// Perceived-shaking reports from the public.
    felt: Option<i64>,
    alert: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Geometry {
    /// `[lon, lat, depth_km]`.
    coordinates: Vec<f64>,
}

/// Turn a decoded feed into observations.
///
/// Split out from `poll` so it can be tested against a captured fixture without
/// a network round trip — every driver in this crate follows the same shape.
fn decode(feed: FeatureCollection, source_id: &SourceId) -> Vec<Observation> {
    feed.features
        .into_iter()
        .filter_map(|f| decode_feature(f, source_id))
        .collect()
}

fn decode_feature(f: Feature, source_id: &SourceId) -> Option<Observation> {
    let coords = f.geometry.as_ref().map(|g| g.coordinates.as_slice())?;
    let (&lon, &lat) = (coords.first()?, coords.get(1)?);

    // USGS reports depth in kilometres below the surface. Negative altitude in
    // metres is the honest representation; a few events genuinely have negative
    // depth (above the datum) and that must survive the sign flip intact.
    let alt_m = coords.get(2).map(|depth_km| -depth_km * 1000.0);

    let observed_at = f.properties.time.and_then(millis_to_utc)?;

    let position = Position {
        lon,
        lat,
        alt_m,
        datum: AltitudeDatum::Geoid,
    };
    if !position.is_plausible() {
        return None;
    }

    let label = f
        .properties
        .mag
        .zip(f.properties.place.as_deref())
        .map(|(mag, place)| format!("M{mag:.1} — {place}"))
        .or_else(|| f.properties.place.clone());

    let attrs = serde_json::json!({
        "magnitude": f.properties.mag,
        "place": f.properties.place,
        "event_type": f.properties.event_type,
        "url": f.properties.url,
        "depth_km": coords.get(2),
        "tsunami": f.properties.tsunami.unwrap_or(0) != 0,
        "stations": f.properties.nst,
        "felt_reports": f.properties.felt,
        "alert_level": f.properties.alert,
        "updated_at": f.properties.updated.and_then(millis_to_utc),
    });

    Some(
        Observation::new(
            source_id.clone(),
            EntityId::new(EntityKind::Event, f.id),
            observed_at,
            Quality::Live,
        )
        .with_position(position)
        .with_attrs(attrs)
        .with_label(label.unwrap_or_else(|| "Seismic event".into())),
    )
}

fn millis_to_utc(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../fixtures/usgs_all_day.json");

    fn decoded() -> Vec<Observation> {
        let feed: FeatureCollection =
            serde_json::from_str(FIXTURE).expect("fixture parses as the live wire format");
        decode(feed, &SourceId::new("usgs-quakes"))
    }

    #[test]
    fn the_fixture_decodes_into_observations() {
        let obs = decoded();
        assert!(!obs.is_empty(), "fixture produced nothing");
        assert!(obs.iter().all(|o| o.entity.kind == EntityKind::Event));
        assert!(obs.iter().all(|o| o.is_meaningful()));
    }

    #[test]
    fn depth_becomes_negative_altitude_in_metres() {
        // A 10 km deep quake is 10,000 m below the datum, not 10 above it.
        let obs = decoded();
        let deep = obs
            .iter()
            .find(|o| o.attrs["depth_km"].as_f64().is_some_and(|d| d > 1.0))
            .expect("fixture has at least one quake deeper than 1 km");
        let alt = deep.position.unwrap().alt_m.expect("altitude present");
        let depth_km = deep.attrs["depth_km"].as_f64().unwrap();
        assert!(alt < 0.0, "depth should be below the datum, got {alt}");
        assert!((alt + depth_km * 1000.0).abs() < 1e-6);
        assert_eq!(deep.position.unwrap().datum, AltitudeDatum::Geoid);
    }

    #[test]
    fn a_negative_depth_becomes_a_positive_altitude() {
        // USGS reports negative depths for events located above the datum
        // (common in volcanic and mining regions). The sign flip must carry
        // them through as genuinely above ground rather than clamping to zero
        // or dropping the row.
        let obs = decoded();
        let shallow = obs
            .iter()
            .find(|o| o.attrs["depth_km"].as_f64().is_some_and(|d| d < 0.0))
            .expect("fixture has an event above the datum");
        let alt = shallow.position.unwrap().alt_m.expect("altitude present");
        assert!(alt > 0.0, "above-datum event should be positive, got {alt}");
    }

    #[test]
    fn observed_at_is_the_event_time_not_the_fetch_time() {
        // Storing ingest time here would smear every quake's position in the
        // DVR by however long the feed took to reach us.
        let obs = decoded();
        let now = Utc::now();
        assert!(
            obs.iter().all(|o| o.observed_at < now),
            "an event claims to be from the future"
        );
        assert!(
            obs.iter().any(|o| (now - o.observed_at).num_seconds() > 60),
            "every event is suspiciously recent; observed_at may be fetch time"
        );
    }

    #[test]
    fn entity_keys_are_the_usgs_ids_so_revisions_update_in_place() {
        // USGS revises magnitude and location for hours after an event. Keying
        // on its id means a revision updates the same entity instead of
        // creating a duplicate quake a few hundred metres away.
        let obs = decoded();
        let keys: std::collections::HashSet<_> =
            obs.iter().map(|o| o.entity.key.as_str()).collect();
        assert_eq!(keys.len(), obs.len(), "duplicate entity keys in one poll");
        assert!(obs.iter().all(|o| !o.entity.key.is_empty()));
    }

    #[test]
    fn features_without_geometry_or_time_are_dropped_not_defaulted() {
        // Defaulting a missing position to 0,0 would put phantom quakes in the
        // Gulf of Guinea; defaulting a missing time to now would corrupt the DVR.
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[
                 {"id":"nogeom","properties":{"time":1756000000000},"geometry":null},
                 {"id":"notime","properties":{},"geometry":{"coordinates":[-97.0,30.0,5.0]}},
                 {"id":"good","properties":{"time":1756000000000,"mag":4.2,"place":"Test"},
                  "geometry":{"coordinates":[-97.0,30.0,5.0]}}
               ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("usgs-quakes"));
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].entity.key, "good");
    }

    #[test]
    fn the_descriptor_declares_the_source_as_keyless_and_free() {
        let src = UsgsEarthquakes::new(HttpClient::new(std::time::Duration::from_secs(5)).unwrap());
        let d = src.descriptor();
        assert!(matches!(d.auth, AuthRequirement::None));
        assert!(matches!(d.cost, CostClass::Free));
        assert!(matches!(d.coverage, Coverage::Global));
        // Attribution is required by the registry, not optional.
        assert!(!d.attribution.provider.is_empty());
        assert!(!d.attribution.license.is_empty());
    }
}
