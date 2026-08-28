//! Earthquakes from the EMSC seismic portal.
//!
//! Fallback for the earthquakes layer behind USGS. Both are GeoJSON and both
//! are global despite EMSC's regional name, but they are not the same format
//! and one trap is worth naming: USGS puts **positive depth** in the third
//! coordinate, EMSC puts **negative depth** — an altitude — in the same slot.
//! Reusing the USGS decoder would flip the sign on every event and hang the
//! world's earthquakes in the sky.
//!
//! This driver therefore reads `properties.depth`, which both catalogues define
//! the same way (kilometres below the surface, positive down), rather than the
//! coordinate whose meaning differs.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

const API_URL: &str =
    "https://www.seismicportal.eu/fdsnws/event/1/query?format=json&limit=1000&orderby=time";

const CADENCE_SECS: u64 = 300;

pub struct EmscEarthquakes {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl EmscEarthquakes {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("emsc-quakes"),
                layer_id: LayerId::new("earthquakes"),
                display_name: "Earthquakes (EMSC)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "European-Mediterranean Seismological Centre".into(),
                    url: "https://www.seismicportal.eu/".into(),
                    license: "Free for non-commercial use; see EMSC terms".into(),
                    notice: Some("Seismic data from EMSC-CSEM".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for EmscEarthquakes {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let start = (Utc::now() - chrono::Duration::hours(24)).format("%Y-%m-%dT%H:%M:%S");
        let url = format!("{API_URL}&start={start}");
        let feed: FeatureCollection = self.http.get_json(&url).await?;
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
    properties: Properties,
}

#[derive(Debug, Deserialize)]
struct Properties {
    unid: Option<String>,
    /// ISO-8601 origin time.
    time: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    /// Kilometres below the surface, positive down — the same convention USGS
    /// uses, and the reason this is read instead of the third coordinate.
    depth: Option<f64>,
    mag: Option<f64>,
    magtype: Option<String>,
    /// Human-readable region name.
    flynn_region: Option<String>,
    /// Contributing network.
    auth: Option<String>,
    /// Event type: `ke` earthquake, `se` explosion, and so on.
    evtype: Option<String>,
    lastupdate: Option<String>,
}

fn decode(feed: FeatureCollection, source_id: &SourceId) -> Vec<Observation> {
    feed.features
        .into_iter()
        .filter_map(|f| decode_feature(f, source_id))
        .collect()
}

fn decode_feature(f: Feature, source_id: &SourceId) -> Option<Observation> {
    let key = f.properties.unid.clone().or_else(|| f.id.clone())?;
    let (lat, lon) = (f.properties.lat?, f.properties.lon?);
    let observed_at = parse_time(f.properties.time.as_deref()?)?;

    // Positive depth becomes negative altitude, exactly as for USGS. Reading
    // the coordinate array instead would double-negate.
    let alt_m = f.properties.depth.map(|km| -km * 1000.0);

    let position = Position {
        lon,
        lat,
        alt_m,
        datum: AltitudeDatum::Geoid,
    };
    if !position.is_plausible() {
        return None;
    }

    let label = match (f.properties.mag, f.properties.flynn_region.as_deref()) {
        (Some(mag), Some(region)) => format!("M{mag:.1} — {region}"),
        (Some(mag), None) => format!("M{mag:.1}"),
        (None, Some(region)) => region.to_string(),
        (None, None) => "Seismic event".into(),
    };

    let attrs = serde_json::json!({
        "magnitude": f.properties.mag,
        "magnitude_type": f.properties.magtype,
        "place": f.properties.flynn_region,
        "event_type": f.properties.evtype,
        "depth_km": f.properties.depth,
        "network": f.properties.auth,
        "catalog": "EMSC",
        "updated_at": f.properties.lastupdate.as_deref().and_then(parse_time),
    });

    Some(
        Observation::new(
            source_id.clone(),
            EntityId::new(EntityKind::Event, key),
            observed_at,
            Quality::Live,
        )
        .with_position(position)
        .with_label(label)
        .with_attrs(attrs),
    )
}

/// EMSC timestamps are ISO-8601 but inconsistently terminated — some carry a
/// `Z`, some do not, and fractional seconds come and go.
fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    let s = raw.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.fZ",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(t.and_utc());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../fixtures/emsc_events.json");

    fn decoded() -> Vec<Observation> {
        let feed: FeatureCollection =
            serde_json::from_str(FIXTURE).expect("fixture parses as the live wire format");
        decode(feed, &SourceId::new("emsc-quakes"))
    }

    #[test]
    fn the_fixture_decodes_into_events() {
        let obs = decoded();
        assert!(!obs.is_empty(), "fixture produced nothing");
        assert!(obs.iter().all(|o| o.entity.kind == EntityKind::Event));
        assert!(obs.iter().all(|o| o.is_meaningful()));
    }

    #[test]
    fn depth_is_read_from_properties_not_the_third_coordinate() {
        // The trap this driver exists to avoid: EMSC's coordinates[2] is
        // ALREADY negative (an altitude), while USGS's is positive (a depth).
        // Reading the coordinate here and negating it, as the USGS decoder
        // does, would hang every earthquake in the sky.
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[{"id":"x","geometry":{"type":"Point","coordinates":[34.8,40.4,-7.0]},
                 "properties":{"unid":"x","time":"2026-08-28T21:12:10.0Z","lat":40.4,"lon":34.8,
                 "depth":7.0,"mag":1.1,"flynn_region":"CENTRAL TURKEY"}}]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"));
        let alt = obs[0].position.unwrap().alt_m.unwrap();
        assert!(
            (alt - -7000.0).abs() < 1e-6,
            "7 km deep should be -7000 m, got {alt}"
        );
        assert!(alt < 0.0, "earthquake ended up above ground");
    }

    #[test]
    fn timestamps_parse_with_and_without_a_trailing_z() {
        // EMSC is inconsistent about this across endpoints.
        assert!(parse_time("2026-08-28T21:12:10.0Z").is_some());
        assert!(parse_time("2026-08-28T21:12:10Z").is_some());
        assert!(parse_time("2026-08-28T21:12:10.582995").is_some());
        assert!(parse_time("2026-08-28T21:12:10").is_some());
        assert_eq!(
            parse_time("2026-08-28T21:12:10Z").unwrap().timestamp(),
            parse_time("2026-08-28T21:12:10").unwrap().timestamp()
        );
        assert!(parse_time("not a time").is_none());
    }

    #[test]
    fn observed_at_is_the_origin_time_not_the_last_update() {
        // lastupdate moves as the catalogue revises; using it would march the
        // event forward through the DVR every time someone refines its
        // magnitude.
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[{"id":"x","properties":{"unid":"x",
                 "time":"2026-08-28T21:12:10.0Z","lastupdate":"2026-08-28T23:59:00.0Z",
                 "lat":40.4,"lon":34.8,"depth":7.0,"mag":1.1}}]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"));
        assert_eq!(
            obs[0].observed_at,
            parse_time("2026-08-28T21:12:10.0Z").unwrap(),
            "observed_at should be the origin time, not lastupdate"
        );
        assert_ne!(obs[0].observed_at, parse_time("2026-08-28T23:59:00.0Z").unwrap());
    }

    #[test]
    fn events_without_a_time_or_position_are_dropped() {
        let feed: FeatureCollection = serde_json::from_str(
            r#"{"features":[
                 {"id":"a","properties":{"unid":"a","lat":40.0,"lon":34.0,"depth":5.0}},
                 {"id":"b","properties":{"unid":"b","time":"2026-08-28T21:12:10Z","depth":5.0}},
                 {"id":"c","properties":{"unid":"c","time":"2026-08-28T21:12:10Z","lat":40.0,"lon":34.0,"depth":5.0}}
               ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"));
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].entity.key, "c");
    }

    #[test]
    fn the_catalogue_is_recorded_so_cross_catalogue_duplicates_are_traceable() {
        // EMSC and USGS assign different ids to the same physical earthquake,
        // so at a failover boundary the layer can briefly hold both. Recording
        // which catalogue an event came from is what makes reconciling them
        // possible later.
        let obs = decoded();
        assert!(obs.iter().all(|o| o.attrs["catalog"] == serde_json::json!("EMSC")));
    }
}
