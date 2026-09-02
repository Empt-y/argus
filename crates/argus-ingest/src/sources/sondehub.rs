//! Radiosondes, from SondeHub.
//!
//! Weather balloons: launched twice daily from hundreds of sites worldwide,
//! climbing to ~30 km before the envelope bursts. SondeHub is the amateur
//! network that tracks their telemetry — the sondes transmit in the clear on
//! 400 MHz, and volunteers with SDRs decode and pool what they hear.
//!
//! These arrive as `EntityKind::Aircraft` rather than a kind of their own,
//! which is a deliberate reuse rather than a shortcut: a radiosonde is a
//! free-flying object with a position, an altitude, a heading and a rate of
//! climb, and every piece of machinery already built for aircraft — the track
//! query, the freshness horizon, the rotated glyph, dead reckoning — is exactly
//! right for it. Inventing a kind would mean re-deriving all of that to draw
//! the same thing.
//!
//! What makes them worth having is the payload: each fix carries the
//! atmosphere it was measured in. Temperature and humidity at 20 km, from an
//! instrument that is physically there, is not something any other layer here
//! can offer.

use crate::http::HttpClient;
use argus_core::entity::{
    AltitudeDatum, EntityId, EntityKind, Kinematics, Observation, Position, Quality,
};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;

/// The last hour of telemetry, one record per sonde: the newest fix each has
/// sent. Asking for less risks missing a sonde whose receiver is intermittent;
/// asking for more returns the same rows with an older timestamp.
const FEED_URL: &str = "https://api.v2.sondehub.org/sondes?last=3600";

/// A sonde reports every second or so, but only a handful are ever airborne at
/// once and each is climbing at ~5 m/s — a minute of movement is ~300 m, which
/// is a meaningful step on the map without being wasteful.
const CADENCE_SECS: u64 = 60;

pub struct SondeHub {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl SondeHub {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("sondehub"),
                layer_id: LayerId::new("radiosondes"),
                display_name: "Radiosondes (SondeHub)".into(),
                kind: EntityKind::Aircraft,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "SondeHub".into(),
                    url: "https://sondehub.org/".into(),
                    license: "Open data, contributed by amateur receivers".into(),
                    notice: Some(
                        "Radiosonde telemetry from the SondeHub network and the amateurs \
                         who operate its receivers"
                            .into(),
                    ),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for SondeHub {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let feed: Feed = self.http.get_json(FEED_URL).await?;
        Ok(decode(feed, &self.descriptor.id))
    }
}

// --- wire format -----------------------------------------------------------

/// Serial number → that sonde's newest frame.
///
/// The API nests one level deeper when asked for a history; this endpoint
/// flattens to the latest frame per sonde, which is what a live map wants.
type Feed = HashMap<String, Frame>;

#[derive(Debug, Deserialize)]
struct Frame {
    serial: Option<String>,
    /// When the sonde says the fix was taken, not when a receiver heard it.
    datetime: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    /// Metres above the ellipsoid — these are GPS positions.
    alt: Option<f64>,
    #[serde(rename = "type")]
    sonde_type: Option<String>,
    manufacturer: Option<String>,
    subtype: Option<String>,
    /// Metres per second, positive upward.
    vel_v: Option<f64>,
    vel_h: Option<f64>,
    heading: Option<f64>,
    /// The atmosphere the sonde is flying through — the reason to carry these
    /// at all rather than treating a sonde as a moving dot.
    temp: Option<f64>,
    humidity: Option<f64>,
    pressure: Option<f64>,
    frequency: Option<f64>,
    /// Who heard it. A sonde is only on the map because somebody's receiver is
    /// running, and saying whose is both courtesy and diagnosis.
    uploader_callsign: Option<String>,
    sats: Option<i64>,
    burst_timer: Option<i64>,
}

fn decode(feed: Feed, source_id: &SourceId) -> Vec<Observation> {
    feed.into_iter()
        .filter_map(|(serial, frame)| decode_frame(&serial, frame, source_id))
        .collect()
}

fn decode_frame(serial: &str, f: Frame, source_id: &SourceId) -> Option<Observation> {
    let (lat, lon) = (f.lat?, f.lon?);
    let observed_at: DateTime<Utc> = f.datetime.as_deref()?.parse().ok()?;

    // The serial is the manufacturer's own, printed on the instrument, and is
    // what every other tool in this hobby keys on. Prefixed so it cannot
    // collide with an ICAO hex in the same `aircraft` kind.
    let key = format!("sonde:{}", f.serial.as_deref().unwrap_or(serial));

    let label = match (&f.manufacturer, &f.sonde_type) {
        (Some(m), Some(t)) => format!("{m} {t}"),
        (_, Some(t)) => t.clone(),
        _ => serial.to_string(),
    };

    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("serial", serde_json::json!(f.serial.as_deref().unwrap_or(serial)));
    put("sonde_type", serde_json::json!(f.sonde_type));
    put("subtype", serde_json::json!(f.subtype));
    put("manufacturer", serde_json::json!(f.manufacturer));
    put("temp_c", serde_json::json!(f.temp));
    put("humidity_pct", serde_json::json!(f.humidity));
    put("pressure_hpa", serde_json::json!(f.pressure));
    put("frequency_mhz", serde_json::json!(f.frequency));
    put("heard_by", serde_json::json!(f.uploader_callsign));
    put("gps_satellites", serde_json::json!(f.sats));
    // 65535 is the sentinel for "no burst timer", not a real countdown.
    put(
        "burst_timer_s",
        serde_json::json!(f.burst_timer.filter(|v| *v != 65_535)),
    );
    // Ascending or descending is the single most useful thing about a sonde:
    // one is a flight in progress, the other is a parachute and a recovery.
    put(
        "phase",
        serde_json::json!(f.vel_v.map(|v| if v >= 0.0 { "ascent" } else { "descent" })),
    );

    Some(
        Observation::new(
            source_id.clone(),
            EntityId::new(EntityKind::Aircraft, key),
            observed_at,
            Quality::Live,
        )
        .with_position(Position {
            lon,
            lat,
            alt_m: f.alt,
            // GPS-derived, so ellipsoidal — the same datum the satellites use
            // and emphatically not a barometric altitude, despite this sonde
            // being the thing that measures pressure.
            datum: AltitudeDatum::Wgs84Ellipsoid,
        })
        .with_kinematics(Kinematics {
            course_deg: f.heading,
            heading_deg: None,
            ground_speed_mps: f.vel_h,
            vertical_rate_mps: f.vel_v,
        })
        .with_label(label)
        .with_attrs(serde_json::Value::Object(attrs)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "Y2412421": {
        "serial": "Y2412421", "datetime": "2026-09-02T17:36:51.998000Z",
        "manufacturer": "Vaisala", "type": "RS41", "subtype": "RS41-SG",
        "lat": 30.33756, "lon": -86.81254, "alt": 20769.451,
        "temp": -62.2, "humidity": 1.0, "vel_v": 4.4794, "vel_h": 6.87068,
        "heading": 225.00707, "sats": 10, "frequency": 403.002,
        "burst_timer": 65535, "uploader_callsign": "WA4JOP"
      },
      "descending": {
        "serial": "D1", "datetime": "2026-09-02T17:00:00.000000Z",
        "lat": 51.5, "lon": -0.1, "alt": 9000.0, "vel_v": -12.5,
        "burst_timer": 900
      },
      "no_position": { "serial": "X1", "datetime": "2026-09-02T17:00:00.000000Z" }
    }"#;

    fn decoded() -> Vec<Observation> {
        let feed: Feed = serde_json::from_str(FIXTURE).expect("fixture parses as the wire format");
        let mut obs = decode(feed, &SourceId::new("sondehub"));
        obs.sort_by(|a, b| a.entity.key.cmp(&b.entity.key));
        obs
    }

    #[test]
    fn a_frame_without_a_position_is_dropped_rather_than_placed_at_null_island() {
        let obs = decoded();
        assert_eq!(obs.len(), 2, "the frame with no lat/lon must not become a contact");
        assert!(obs.iter().all(|o| !o.entity.key.contains("X1")));
    }

    #[test]
    fn the_serial_is_namespaced_so_it_cannot_collide_with_an_icao_hex() {
        // Both land in the `aircraft` kind, and a six-character serial could
        // otherwise look exactly like a transponder address.
        let obs = decoded();
        assert!(obs.iter().all(|o| o.entity.key.starts_with("sonde:")));
        assert_eq!(obs[1].entity.key, "sonde:Y2412421");
    }

    #[test]
    fn altitude_is_ellipsoidal_because_the_fix_is_gps() {
        let obs = decoded();
        let position = obs[1].position.as_ref().expect("a position");
        assert_eq!(position.alt_m, Some(20769.451));
        assert_eq!(position.datum, AltitudeDatum::Wgs84Ellipsoid);
    }

    #[test]
    fn the_atmosphere_it_flew_through_is_carried_with_the_fix() {
        let attrs = &decoded()[1].attrs;
        assert_eq!(attrs["temp_c"], serde_json::json!(-62.2));
        assert_eq!(attrs["humidity_pct"], serde_json::json!(1.0));
        assert_eq!(attrs["heard_by"], serde_json::json!("WA4JOP"));
    }

    #[test]
    fn ascent_and_descent_are_distinguished_and_the_burst_sentinel_is_not_a_countdown() {
        let obs = decoded();
        let descending = &obs[0];
        assert_eq!(descending.attrs["phase"], serde_json::json!("descent"));
        assert_eq!(descending.attrs["burst_timer_s"], serde_json::json!(900));

        let ascending = &obs[1];
        assert_eq!(ascending.attrs["phase"], serde_json::json!("ascent"));
        assert!(
            ascending.attrs.get("burst_timer_s").is_none(),
            "65535 means 'no timer', not 18 hours"
        );
    }
}
