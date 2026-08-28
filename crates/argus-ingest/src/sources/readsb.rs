//! Aircraft from readsb-format community aggregators.
//!
//! adsb.lol, adsb.fi and several others all expose the same tar1090/readsb JSON
//! that a dump1090 receiver produces locally, differing only in base URL and in
//! whether the array is called `ac` or `aircraft`. One decoder therefore serves
//! all of them — and the same decoder will serve a local SDR in Phase 10, which
//! is the point: a dongle on the roof is just another provider in the chain.
//!
//! These are the cheap, unmetered members of the flights chain. They are
//! radius-limited (250 nm) rather than global, which suits area-of-interest
//! capture exactly.

use crate::http::HttpClient;
use argus_core::entity::{
    AltitudeDatum, EntityId, EntityKind, Kinematics, Observation, Position, Quality,
};
use argus_core::geo::{BoundingBox, haversine_m};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

const FEET_TO_M: f64 = 0.3048;
const KNOTS_TO_MPS: f64 = 0.514_444;
const FPM_TO_MPS: f64 = 0.00508;

/// These endpoints cap the search radius. Asking for more is rejected outright.
const MAX_RADIUS_NM: f64 = 250.0;

/// Aircraft positions age fast. Anything this stale is a receiver still
/// reporting a contact it has not actually heard from recently.
const MAX_POSITION_AGE_S: f64 = 120.0;

const CADENCE_SECS: u64 = 20;

/// One readsb-format aggregator.
pub struct ReadsbProvider {
    descriptor: SourceDescriptor,
    http: HttpClient,
    base_url: String,
}

impl ReadsbProvider {
    /// adsb.lol — community aggregator, no key, no published cap.
    pub fn adsb_lol(http: HttpClient) -> Self {
        Self::new(
            http,
            "adsb-lol",
            "Aircraft (adsb.lol)",
            "https://api.adsb.lol/v2",
            Attribution {
                provider: "adsb.lol".into(),
                url: "https://adsb.lol/".into(),
                license: "ODbL — community-contributed receiver data".into(),
                notice: Some("Aircraft data from the adsb.lol community network".into()),
            },
        )
    }

    /// adsb.fi — same format, independent receiver network, so a genuinely
    /// separate point of failure rather than a mirror.
    pub fn adsb_fi(http: HttpClient) -> Self {
        Self::new(
            http,
            "adsb-fi",
            "Aircraft (adsb.fi)",
            "https://opendata.adsb.fi/api/v2",
            Attribution {
                provider: "adsb.fi".into(),
                url: "https://adsb.fi/".into(),
                license: "Open data — community-contributed receiver data".into(),
                notice: Some("Aircraft data from the adsb.fi community network".into()),
            },
        )
    }

    fn new(
        http: HttpClient,
        id: &str,
        display_name: &str,
        base_url: &str,
        attribution: Attribution,
    ) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new(id),
                layer_id: LayerId::new("flights"),
                display_name: display_name.into(),
                kind: EntityKind::Aircraft,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Bounded,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution,
                base_quality: Quality::Live,
                // No published numeric cap. Politeness is enforced by cadence
                // rather than by an allowance we would be inventing.
                quota: None,
            },
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

/// Convert a bounding box into the centre-and-radius query these endpoints take.
///
/// The radius covers the box's corners, so the query is a superset of what was
/// asked for; results outside the box are filtered after decoding. Erring
/// outward is deliberate — a radius that inscribes the box would silently miss
/// aircraft in its corners.
pub fn bbox_to_center_radius(bbox: BoundingBox) -> (f64, f64, f64) {
    // Split boxes are handled a level up; a wrapped box here would compute a
    // centre on the wrong side of the planet.
    let parts = bbox.split_at_antimeridian();
    let b = parts[0];
    let lat = (b.south + b.north) / 2.0;
    let lon = (b.west + b.east) / 2.0;
    let corner_m = haversine_m(lat, lon, b.north, b.east);
    let radius_nm = (corner_m / 1852.0).ceil().clamp(1.0, MAX_RADIUS_NM);
    (lat, lon, radius_nm)
}

#[async_trait::async_trait]
impl Source for ReadsbProvider {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let bbox = ctx.bbox.unwrap_or(BoundingBox::GLOBAL);
        let (lat, lon, radius_nm) = bbox_to_center_radius(bbox);
        let url = format!(
            "{}/lat/{lat:.5}/lon/{lon:.5}/dist/{radius_nm:.0}",
            self.base_url
        );
        let feed: ReadsbFeed = self.http.get_json(&url).await?;
        Ok(decode(feed, &self.descriptor.id, Utc::now()))
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ReadsbFeed {
    /// adsb.lol calls it `ac`; adsb.fi calls it `aircraft`. Same contents.
    #[serde(alias = "aircraft")]
    ac: Vec<Aircraft>,
    /// Server time, seconds since the epoch. Used with `seen_pos` to recover
    /// when each position was actually observed.
    now: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Aircraft {
    hex: Option<String>,
    flight: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    /// Barometric altitude in feet, or the literal string "ground".
    alt_baro: Option<Altitude>,
    /// Geometric (GNSS) altitude in feet.
    alt_geom: Option<f64>,
    /// Ground speed, knots.
    gs: Option<f64>,
    /// True track over ground, degrees.
    track: Option<f64>,
    true_heading: Option<f64>,
    mag_heading: Option<f64>,
    /// Barometric vertical rate, feet per minute.
    baro_rate: Option<f64>,
    geom_rate: Option<f64>,
    squawk: Option<String>,
    category: Option<String>,
    emergency: Option<String>,
    /// Registration.
    r: Option<String>,
    /// ICAO type code.
    t: Option<String>,
    /// Seconds since this position was received.
    seen_pos: Option<f64>,
    /// Present when the position came from multilateration rather than from
    /// the aircraft's own ADS-B broadcast.
    mlat: Option<serde_json::Value>,
}

/// `alt_baro` is a number in flight and the string "ground" on the surface.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Altitude {
    Feet(f64),
    Ground(String),
}

impl Altitude {
    fn feet(&self) -> Option<f64> {
        match self {
            Self::Feet(f) => Some(*f),
            Self::Ground(_) => None,
        }
    }

    fn is_ground(&self) -> bool {
        matches!(self, Self::Ground(s) if s.eq_ignore_ascii_case("ground"))
    }
}

/// Turn a decoded feed into observations.
fn decode(feed: ReadsbFeed, source_id: &SourceId, fallback_now: DateTime<Utc>) -> Vec<Observation> {
    let server_now = feed
        .now
        .and_then(|secs| DateTime::from_timestamp(secs as i64, 0))
        .unwrap_or(fallback_now);
    feed.ac
        .into_iter()
        .filter_map(|a| decode_aircraft(a, source_id, server_now))
        .collect()
}

fn decode_aircraft(
    a: Aircraft,
    source_id: &SourceId,
    server_now: DateTime<Utc>,
) -> Option<Observation> {
    let hex = a.hex.as_deref()?.trim();
    if hex.is_empty() {
        return None;
    }
    let (lat, lon) = (a.lat?, a.lon?);

    // Positions age. `seen_pos` is how long ago the receiver actually heard
    // this one, so it is what `observed_at` must be built from — using fetch
    // time instead would claim a two-minute-old contact is current.
    let age_s = a.seen_pos.unwrap_or(0.0).max(0.0);
    if age_s > MAX_POSITION_AGE_S {
        return None;
    }
    let observed_at = server_now - Duration::milliseconds((age_s * 1000.0) as i64);

    let on_ground = a.alt_baro.as_ref().is_some_and(Altitude::is_ground);

    // Prefer the geometric altitude. `alt_baro` is pressure altitude against
    // the 1013.25 hPa standard datum, which on a high-pressure day sits
    // hundreds of feet from where the aircraft actually is — the readings in
    // this very feed differ by ~150 ft. Take the GNSS height when offered and
    // record which one was used.
    let (alt_m, datum) = if on_ground {
        (Some(0.0), AltitudeDatum::AboveGround)
    } else if let Some(geom_ft) = a.alt_geom {
        (Some(geom_ft * FEET_TO_M), AltitudeDatum::Wgs84Ellipsoid)
    } else if let Some(baro_ft) = a.alt_baro.as_ref().and_then(Altitude::feet) {
        (Some(baro_ft * FEET_TO_M), AltitudeDatum::Barometric)
    } else {
        (None, AltitudeDatum::Barometric)
    };

    let position = Position {
        lon,
        lat,
        alt_m,
        datum,
    };
    if !position.is_plausible() {
        return None;
    }

    // A multilaterated position is computed from arrival-time differences at
    // several receivers, not broadcast by the aircraft. Typically a few
    // hundred metres out and prone to jumps, so it is an estimate and is
    // labelled as one.
    let is_mlat = a
        .mlat
        .as_ref()
        .is_some_and(|v| !matches!(v, serde_json::Value::Null) && v.as_array().is_none_or(|arr| !arr.is_empty()));
    let quality = if is_mlat {
        Quality::Estimated
    } else {
        Quality::Live
    };

    let kinematics = Kinematics {
        course_deg: a.track,
        // True heading where the aircraft reports it; magnetic is left in
        // attrs rather than silently treated as true, since the difference
        // reaches 20° at high latitudes.
        heading_deg: a.true_heading,
        ground_speed_mps: a.gs.map(|kt| kt * KNOTS_TO_MPS),
        vertical_rate_mps: a.geom_rate.or(a.baro_rate).map(|fpm| fpm * FPM_TO_MPS),
    };

    let callsign = a
        .flight
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let attrs = serde_json::json!({
        "callsign": callsign,
        "registration": a.r,
        "type_code": a.t,
        "category": a.category,
        "squawk": a.squawk,
        "emergency": a.emergency.as_deref().filter(|e| *e != "none"),
        "on_ground": on_ground,
        "mlat": is_mlat,
        "magnetic_heading_deg": a.mag_heading,
        "baro_altitude_ft": a.alt_baro.as_ref().and_then(Altitude::feet),
        "geom_altitude_ft": a.alt_geom,
        "position_age_s": age_s,
    });

    Some(
        Observation::new(source_id.clone(), EntityId::aircraft(hex), observed_at, quality)
            .with_position(position)
            .with_kinematics(kinematics)
            .with_label(callsign.unwrap_or_else(|| hex.to_uppercase()))
            .with_attrs(attrs),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADSB_LOL: &str = include_str!("../../fixtures/adsb_lol.json");
    const ADSB_FI: &str = include_str!("../../fixtures/adsb_fi.json");

    fn decode_str(raw: &str) -> Vec<Observation> {
        let feed: ReadsbFeed = serde_json::from_str(raw).expect("parses as readsb format");
        decode(feed, &SourceId::new("test"), Utc::now())
    }

    #[test]
    fn one_decoder_handles_both_providers() {
        // adsb.lol calls the array `ac`, adsb.fi calls it `aircraft`. Same
        // contents, so the same decoder must read both — that equivalence is
        // what makes them interchangeable in a chain.
        let lol = decode_str(ADSB_LOL);
        let fi = decode_str(ADSB_FI);
        assert!(!lol.is_empty(), "adsb.lol fixture produced nothing");
        assert!(!fi.is_empty(), "adsb.fi fixture produced nothing");
        assert!(lol.iter().all(|o| o.entity.kind == EntityKind::Aircraft));
        assert!(fi.iter().all(|o| o.entity.kind == EntityKind::Aircraft));
    }

    #[test]
    fn the_two_providers_agree_about_shared_aircraft() {
        // Both networks see many of the same aircraft. Where they overlap the
        // decoded positions must agree closely, or one of them is being read
        // wrong.
        let lol = decode_str(ADSB_LOL);
        let fi = decode_str(ADSB_FI);
        let mut compared = 0;
        for a in &lol {
            let Some(b) = fi.iter().find(|o| o.entity.key == a.entity.key) else {
                continue;
            };
            let (pa, pb) = (a.position.unwrap(), b.position.unwrap());
            let sep_km = haversine_m(pa.lat, pa.lon, pb.lat, pb.lon) / 1000.0;
            assert!(
                sep_km < 10.0,
                "{} differs by {sep_km:.1} km between providers",
                a.entity.key
            );
            compared += 1;
        }
        assert!(compared > 5, "only {compared} shared aircraft to compare");
    }

    #[test]
    fn geometric_altitude_is_preferred_over_barometric() {
        // The two differ by ~150 ft in this very fixture. Barometric is
        // pressure altitude, not where the aircraft is.
        let obs = decode_str(ADSB_LOL);
        let both = obs
            .iter()
            .find(|o| {
                o.attrs["geom_altitude_ft"].is_number() && o.attrs["baro_altitude_ft"].is_number()
            })
            .expect("fixture has an aircraft reporting both altitudes");
        let p = both.position.unwrap();
        assert_eq!(p.datum, AltitudeDatum::Wgs84Ellipsoid);
        let geom_ft = both.attrs["geom_altitude_ft"].as_f64().unwrap();
        assert!((p.alt_m.unwrap() - geom_ft * FEET_TO_M).abs() < 1e-6);
    }

    #[test]
    fn barometric_is_used_and_labelled_when_geometric_is_absent() {
        let feed: ReadsbFeed = serde_json::from_str(
            r#"{"now":1756000000,"ac":[
                {"hex":"abc123","lat":51.0,"lon":-0.4,"alt_baro":35000,"seen_pos":1.0}
            ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"), Utc::now());
        let p = obs[0].position.unwrap();
        assert_eq!(p.datum, AltitudeDatum::Barometric);
        assert!((p.alt_m.unwrap() - 35000.0 * FEET_TO_M).abs() < 1e-6);
    }

    #[test]
    fn grounded_aircraft_are_recognised_from_the_string_altitude() {
        // alt_baro is a number in flight and the literal "ground" on the
        // surface; a naive numeric parse drops every taxiing aircraft.
        let obs = decode_str(ADSB_LOL);
        let grounded: Vec<_> = obs
            .iter()
            .filter(|o| o.attrs["on_ground"] == serde_json::json!(true))
            .collect();
        assert!(!grounded.is_empty(), "no grounded aircraft decoded");
        for g in grounded {
            let p = g.position.unwrap();
            assert_eq!(p.alt_m, Some(0.0));
            assert_eq!(p.datum, AltitudeDatum::AboveGround);
        }
    }

    #[test]
    fn multilaterated_positions_are_marked_as_estimates() {
        // MLAT is computed from receiver timing, not broadcast by the aircraft.
        let feed: ReadsbFeed = serde_json::from_str(
            r#"{"now":1756000000,"ac":[
                {"hex":"aaa111","lat":51.0,"lon":-0.4,"alt_baro":10000,"seen_pos":1.0,"mlat":["lat","lon"]},
                {"hex":"bbb222","lat":51.1,"lon":-0.5,"alt_baro":10000,"seen_pos":1.0,"mlat":[]}
            ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"), Utc::now());
        let mlat = obs.iter().find(|o| o.entity.key == "aaa111").unwrap();
        let adsb = obs.iter().find(|o| o.entity.key == "bbb222").unwrap();
        assert_eq!(mlat.quality, Quality::Estimated);
        assert_eq!(adsb.quality, Quality::Live);
    }

    #[test]
    fn observed_at_is_built_from_position_age_not_fetch_time() {
        // seen_pos says how long ago the receiver actually heard the aircraft.
        // Using fetch time would claim a stale contact is current, and would
        // smear every track in the DVR.
        let feed: ReadsbFeed = serde_json::from_str(
            r#"{"now":1756000000,"ac":[
                {"hex":"abc123","lat":51.0,"lon":-0.4,"alt_baro":35000,"seen_pos":45.0}
            ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"), Utc::now());
        let expected = DateTime::from_timestamp(1_756_000_000 - 45, 0).unwrap();
        assert!((obs[0].observed_at - expected).num_seconds().abs() <= 1);
    }

    #[test]
    fn stale_contacts_are_dropped() {
        let feed: ReadsbFeed = serde_json::from_str(
            r#"{"now":1756000000,"ac":[
                {"hex":"fresh1","lat":51.0,"lon":-0.4,"alt_baro":35000,"seen_pos":5.0},
                {"hex":"stale1","lat":51.0,"lon":-0.4,"alt_baro":35000,"seen_pos":600.0}
            ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"), Utc::now());
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].entity.key, "fresh1");
    }

    #[test]
    fn units_are_converted_out_of_aviation_into_si() {
        let feed: ReadsbFeed = serde_json::from_str(
            r#"{"now":1756000000,"ac":[
                {"hex":"abc123","lat":51.0,"lon":-0.4,"alt_geom":35000,
                 "gs":450.0,"track":270.0,"true_heading":268.0,"baro_rate":-640,"seen_pos":1.0}
            ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"), Utc::now());
        let k = obs[0].kinematics.unwrap();
        assert!((k.ground_speed_mps.unwrap() - 450.0 * KNOTS_TO_MPS).abs() < 1e-6);
        assert!((k.vertical_rate_mps.unwrap() - (-640.0 * FPM_TO_MPS)).abs() < 1e-6);
        assert_eq!(k.course_deg, Some(270.0));
        assert_eq!(k.heading_deg, Some(268.0));
        assert!((obs[0].position.unwrap().alt_m.unwrap() - 35000.0 * FEET_TO_M).abs() < 1e-6);
    }

    #[test]
    fn magnetic_heading_is_not_passed_off_as_true() {
        // The difference reaches 20° at high latitudes; treating one as the
        // other points every icon wrong.
        let feed: ReadsbFeed = serde_json::from_str(
            r#"{"now":1756000000,"ac":[
                {"hex":"abc123","lat":51.0,"lon":-0.4,"alt_geom":10000,"mag_heading":100.0,"seen_pos":1.0}
            ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("t"), Utc::now());
        assert_eq!(obs[0].kinematics.unwrap().heading_deg, None);
        assert_eq!(obs[0].attrs["magnetic_heading_deg"], serde_json::json!(100.0));
    }

    #[test]
    fn a_bbox_becomes_a_radius_that_covers_its_corners() {
        // A radius inscribing the box would silently miss aircraft in the
        // corners, so it must reach them.
        let bbox = BoundingBox::new(-1.0, 51.0, 0.5, 52.0);
        let (lat, lon, radius_nm) = bbox_to_center_radius(bbox);
        assert!((lat - 51.5).abs() < 1e-9);
        assert!((lon - -0.25).abs() < 1e-9);
        let corner_nm = haversine_m(lat, lon, 52.0, 0.5) / 1852.0;
        assert!(
            radius_nm >= corner_nm,
            "radius {radius_nm} nm does not reach the corner at {corner_nm} nm"
        );
    }

    #[test]
    fn the_radius_is_capped_at_what_the_endpoints_accept() {
        // A global box would ask for thousands of nautical miles and be
        // rejected outright.
        let (_, _, radius_nm) = bbox_to_center_radius(BoundingBox::GLOBAL);
        assert!(radius_nm <= MAX_RADIUS_NM, "radius {radius_nm} exceeds the cap");
    }

    #[test]
    fn entity_keys_are_normalised_so_providers_merge_onto_one_track() {
        // The whole point of a chain: adsb.lol and adsb.fi describing the same
        // aircraft must land on the same entity, not two.
        let lol = decode_str(ADSB_LOL);
        let fi = decode_str(ADSB_FI);
        let shared = lol
            .iter()
            .filter(|a| fi.iter().any(|b| b.entity == a.entity))
            .count();
        assert!(shared > 5, "only {shared} entities matched across providers");
        assert!(lol.iter().all(|o| o.entity.key == o.entity.key.to_lowercase()));
    }
}
