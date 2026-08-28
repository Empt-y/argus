//! Aircraft from the OpenSky Network.
//!
//! The first genuinely metered provider: 400 credits a day anonymously, and a
//! bounded query costs between one and four of them depending on how much sky
//! it covers. That makes it the member of the flights chain whose allowance
//! actually has to be rationed — the community aggregators ahead of it are
//! unmetered, so OpenSky is only reached when they are both down, which is
//! exactly when its allowance matters most.
//!
//! It reports in SI already (metres, m/s), which is a pleasant change from the
//! knots and feet everywhere else in aviation.

use crate::http::HttpClient;
use argus_core::entity::{
    AltitudeDatum, EntityId, EntityKind, Kinematics, Observation, Position, Quality,
};
use argus_core::geo::BoundingBox;
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Quota, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::DateTime;
use serde::Deserialize;

const API_URL: &str = "https://opensky-network.org/api/states/all";

/// Anonymous daily allowance. An authenticated account gets 4,000.
const ANON_DAILY_CREDITS: u32 = 400;

/// Contacts older than this are dropped — OpenSky keeps reporting an aircraft
/// for a while after the last position fix.
const MAX_POSITION_AGE_S: i64 = 120;

/// OpenSky's own cadence: state vectors update roughly every 5–10 s
/// anonymously, so polling faster only spends credits for nothing.
const CADENCE_SECS: u64 = 30;

pub struct OpenSky {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl OpenSky {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("opensky"),
                layer_id: LayerId::new("flights"),
                display_name: "Aircraft (OpenSky Network)".into(),
                kind: EntityKind::Aircraft,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Bounded,
                // Anonymous access works fully. An account raises the allowance
                // tenfold but is never required, so this must not gate the
                // source — see AuthRequirement::Optional.
                auth: AuthRequirement::Optional {
                    config_key: "client_id".into(),
                },
                cost: CostClass::Metered,
                attribution: Attribution {
                    provider: "The OpenSky Network".into(),
                    url: "https://opensky-network.org/".into(),
                    license: "CC BY-SA 4.0 — non-commercial use".into(),
                    notice: Some(
                        "Data from The OpenSky Network, https://opensky-network.org".into(),
                    ),
                },
                base_quality: Quality::Live,
                quota: Some(Quota {
                    limit: ANON_DAILY_CREDITS,
                    window: std::time::Duration::from_secs(86_400),
                    // Deliberately the worst-case tariff rather than the one a
                    // small area would actually incur.
                    //
                    // The real cost is 1–4 credits by area, but the chain
                    // charges a fixed figure before the call and cannot know
                    // the box in advance. Over-charging under-uses the
                    // allowance; under-charging overruns it and earns a wall of
                    // 429s. For a last-resort provider, under-using costs
                    // nothing — so the conservative figure is the right error
                    // to make.
                    cost_per_poll: 4,
                }),
            },
            http,
        }
    }
}

/// OpenSky's published credit tariff, by the area of the requested box.
///
/// Not used for accounting — see the note on `cost_per_poll` — but it is what
/// the tariff actually is, and it is what a future dynamic-cost chain would
/// call.
pub fn credit_cost(bbox: BoundingBox) -> u32 {
    let parts = bbox.split_at_antimeridian();
    let area: f64 = parts
        .iter()
        .map(|b| (b.east - b.west).abs() * (b.north - b.south).abs())
        .sum();
    match area {
        a if a <= 25.0 => 1,
        a if a <= 100.0 => 2,
        a if a <= 400.0 => 3,
        _ => 4,
    }
}

#[async_trait::async_trait]
impl Source for OpenSky {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let url = match ctx.bbox {
            Some(bbox) => {
                let b = bbox.split_at_antimeridian()[0];
                format!(
                    "{API_URL}?lamin={:.4}&lomin={:.4}&lamax={:.4}&lomax={:.4}",
                    b.south, b.west, b.north, b.east
                )
            }
            None => API_URL.to_string(),
        };
        let feed: StatesFeed = self.http.get_json(&url).await?;
        Ok(decode(feed, &self.descriptor.id))
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StatesFeed {
    time: i64,
    /// Positional arrays, not objects. Field order is the API contract.
    states: Option<Vec<Vec<serde_json::Value>>>,
}

/// Indices into a state vector, from the OpenSky API documentation. Named
/// rather than inlined because a bare `v[13]` in the decode body is unreadable
/// and a transposed pair is invisible in review.
mod field {
    pub const ICAO24: usize = 0;
    pub const CALLSIGN: usize = 1;
    pub const ORIGIN_COUNTRY: usize = 2;
    pub const TIME_POSITION: usize = 3;
    pub const LAST_CONTACT: usize = 4;
    pub const LONGITUDE: usize = 5;
    pub const LATITUDE: usize = 6;
    pub const BARO_ALTITUDE: usize = 7;
    pub const ON_GROUND: usize = 8;
    pub const VELOCITY: usize = 9;
    pub const TRUE_TRACK: usize = 10;
    pub const VERTICAL_RATE: usize = 11;
    pub const GEO_ALTITUDE: usize = 13;
    pub const SQUAWK: usize = 14;
    pub const POSITION_SOURCE: usize = 16;
}

/// `position_source` values. 2 is multilateration — computed from receiver
/// timing rather than broadcast by the aircraft.
const SOURCE_MLAT: u64 = 2;

fn decode(feed: StatesFeed, source_id: &SourceId) -> Vec<Observation> {
    let Some(states) = feed.states else {
        // A quiet box legitimately returns null rather than an empty array.
        return Vec::new();
    };
    states
        .into_iter()
        .filter_map(|v| decode_state(&v, source_id, feed.time))
        .collect()
}

fn num(v: &[serde_json::Value], i: usize) -> Option<f64> {
    v.get(i)?.as_f64()
}

fn text(v: &[serde_json::Value], i: usize) -> Option<String> {
    let s = v.get(i)?.as_str()?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn decode_state(
    v: &[serde_json::Value],
    source_id: &SourceId,
    feed_time: i64,
) -> Option<Observation> {
    let icao24 = text(v, field::ICAO24)?;
    let lon = num(v, field::LONGITUDE)?;
    let lat = num(v, field::LATITUDE)?;

    // `time_position` is when the position was actually fixed; `last_contact`
    // is merely when the aircraft was last heard from at all. Using the latter
    // would date a position by a message that carried no position.
    let position_time = num(v, field::TIME_POSITION)
        .or_else(|| num(v, field::LAST_CONTACT))
        .map(|t| t as i64)
        .unwrap_or(feed_time);
    if feed_time - position_time > MAX_POSITION_AGE_S {
        return None;
    }
    let observed_at = DateTime::from_timestamp(position_time, 0)?;

    let on_ground = v
        .get(field::ON_GROUND)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // Same datum rule as every other aircraft source: geometric where offered,
    // barometric only as a fallback and labelled as such.
    let (alt_m, datum) = if on_ground {
        (Some(0.0), AltitudeDatum::AboveGround)
    } else if let Some(geo) = num(v, field::GEO_ALTITUDE) {
        (Some(geo), AltitudeDatum::Wgs84Ellipsoid)
    } else if let Some(baro) = num(v, field::BARO_ALTITUDE) {
        (Some(baro), AltitudeDatum::Barometric)
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

    let is_mlat = v
        .get(field::POSITION_SOURCE)
        .and_then(serde_json::Value::as_u64)
        == Some(SOURCE_MLAT);

    let callsign = text(v, field::CALLSIGN);

    let attrs = serde_json::json!({
        "callsign": callsign,
        "origin_country": text(v, field::ORIGIN_COUNTRY),
        "squawk": text(v, field::SQUAWK),
        "on_ground": on_ground,
        "mlat": is_mlat,
        "position_age_s": feed_time - position_time,
        "baro_altitude_m": num(v, field::BARO_ALTITUDE),
        "geo_altitude_m": num(v, field::GEO_ALTITUDE),
    });

    Some(
        Observation::new(
            source_id.clone(),
            EntityId::aircraft(&icao24),
            observed_at,
            if is_mlat {
                Quality::Estimated
            } else {
                Quality::Live
            },
        )
        .with_position(position)
        .with_kinematics(Kinematics {
            course_deg: num(v, field::TRUE_TRACK),
            // OpenSky reports track over ground only; it never supplies a
            // separate body heading, and inventing one from the track would be
            // wrong under any crosswind.
            heading_deg: None,
            ground_speed_mps: num(v, field::VELOCITY),
            vertical_rate_mps: num(v, field::VERTICAL_RATE),
        })
        .with_label(callsign.unwrap_or_else(|| icao24.to_uppercase()))
        .with_attrs(attrs),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../fixtures/opensky_states.json");

    fn decoded() -> Vec<Observation> {
        let feed: StatesFeed =
            serde_json::from_str(FIXTURE).expect("fixture parses as the live wire format");
        decode(feed, &SourceId::new("opensky"))
    }

    #[test]
    fn the_fixture_decodes_into_aircraft() {
        let obs = decoded();
        assert!(!obs.is_empty(), "fixture produced nothing");
        assert!(obs.iter().all(|o| o.entity.kind == EntityKind::Aircraft));
        assert!(obs.iter().all(|o| o.position.is_some_and(|p| p.is_plausible())));
    }

    #[test]
    fn positional_arrays_are_read_at_the_documented_indices() {
        // The API contract is field order, so a transposed pair would silently
        // put velocity into the vertical rate. Pin a hand-built vector where
        // every value is distinguishable.
        let feed: StatesFeed = serde_json::from_str(
            r#"{"time": 1756000100, "states": [[
                "abc123", "TEST123 ", "United Kingdom", 1756000090, 1756000095,
                -0.45, 51.47, 10000.5, false, 230.5, 271.5, -2.5, null, 10150.75,
                "1234", false, 0
            ]]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("opensky"));
        assert_eq!(obs.len(), 1);
        let o = &obs[0];
        assert_eq!(o.entity.key, "abc123");
        assert_eq!(o.label.as_deref(), Some("TEST123"));
        let p = o.position.unwrap();
        assert!((p.lon - -0.45).abs() < 1e-9);
        assert!((p.lat - 51.47).abs() < 1e-9);
        // Geometric altitude wins over barometric.
        assert!((p.alt_m.unwrap() - 10150.75).abs() < 1e-9);
        assert_eq!(p.datum, AltitudeDatum::Wgs84Ellipsoid);
        let k = o.kinematics.unwrap();
        assert_eq!(k.ground_speed_mps, Some(230.5));
        assert_eq!(k.course_deg, Some(271.5));
        assert_eq!(k.vertical_rate_mps, Some(-2.5));
        assert_eq!(o.attrs["origin_country"], serde_json::json!("United Kingdom"));
        assert_eq!(o.attrs["squawk"], serde_json::json!("1234"));
    }

    #[test]
    fn multilateration_is_marked_as_an_estimate() {
        let feed: StatesFeed = serde_json::from_str(
            r#"{"time": 1756000100, "states": [
                ["aaa111","A ","UK",1756000090,1756000095,-0.4,51.4,1000,false,200,270,0,null,1010,"1200",false,2],
                ["bbb222","B ","UK",1756000090,1756000095,-0.4,51.4,1000,false,200,270,0,null,1010,"1200",false,0]
            ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("opensky"));
        let mlat = obs.iter().find(|o| o.entity.key == "aaa111").unwrap();
        let adsb = obs.iter().find(|o| o.entity.key == "bbb222").unwrap();
        assert_eq!(mlat.quality, Quality::Estimated);
        assert_eq!(adsb.quality, Quality::Live);
    }

    #[test]
    fn a_null_states_array_is_an_empty_sky_not_an_error() {
        // A quiet box legitimately returns null rather than [].
        let feed: StatesFeed = serde_json::from_str(r#"{"time": 1756000100, "states": null}"#).unwrap();
        assert!(decode(feed, &SourceId::new("opensky")).is_empty());
    }

    #[test]
    fn position_time_is_preferred_over_last_contact() {
        // last_contact can be updated by a message carrying no position at all;
        // dating a fix by it would claim the aircraft is somewhere it was not.
        let feed: StatesFeed = serde_json::from_str(
            r#"{"time": 1756000100, "states": [
                ["abc123","T ","UK",1756000050,1756000099,-0.4,51.4,1000,false,200,270,0,null,1010,"1200",false,0]
            ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("opensky"));
        assert_eq!(obs[0].observed_at.timestamp(), 1_756_000_050);
        assert_eq!(obs[0].attrs["position_age_s"], serde_json::json!(50));
    }

    #[test]
    fn stale_contacts_are_dropped() {
        let feed: StatesFeed = serde_json::from_str(
            r#"{"time": 1756000500, "states": [
                ["fresh1","F ","UK",1756000490,1756000495,-0.4,51.4,1000,false,200,270,0,null,1010,"1200",false,0],
                ["stale1","S ","UK",1756000100,1756000105,-0.4,51.4,1000,false,200,270,0,null,1010,"1200",false,0]
            ]}"#,
        )
        .unwrap();
        let obs = decode(feed, &SourceId::new("opensky"));
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].entity.key, "fresh1");
    }

    #[test]
    fn aircraft_without_a_position_are_dropped_not_defaulted() {
        let feed: StatesFeed = serde_json::from_str(
            r#"{"time": 1756000100, "states": [
                ["nopos1","N ","UK",1756000090,1756000095,null,null,1000,false,200,270,0,null,1010,"1200",false,0]
            ]}"#,
        )
        .unwrap();
        assert!(decode(feed, &SourceId::new("opensky")).is_empty());
    }

    #[test]
    fn the_credit_tariff_follows_the_published_bands() {
        // 5x5 degrees = 25 sq deg, the top of the cheapest band.
        assert_eq!(credit_cost(BoundingBox::new(0.0, 0.0, 5.0, 5.0)), 1);
        assert_eq!(credit_cost(BoundingBox::new(0.0, 0.0, 10.0, 5.0)), 2);
        assert_eq!(credit_cost(BoundingBox::new(0.0, 0.0, 20.0, 10.0)), 3);
        assert_eq!(credit_cost(BoundingBox::GLOBAL), 4);
        // A wrapped box is measured across both halves, not by the negative
        // span that a naive subtraction would produce.
        assert_eq!(credit_cost(BoundingBox::new(179.0, 0.0, -179.0, 2.0)), 1);
    }

    #[test]
    fn the_declared_allowance_is_conservative_rather_than_optimistic() {
        // Under-charging overruns the allowance and earns a wall of 429s;
        // over-charging merely under-uses it. For a last-resort provider the
        // conservative error is free.
        let src = OpenSky::new(HttpClient::new(std::time::Duration::from_secs(5)).unwrap());
        let quota = src.descriptor().quota.expect("metered");
        assert_eq!(quota.limit, ANON_DAILY_CREDITS);
        assert!(
            quota.cost_per_poll >= credit_cost(BoundingBox::GLOBAL),
            "declared cost must not be under the worst-case tariff"
        );
        // 400 credits at 4 each: 100 polls a day, which at a 30-second cadence
        // is well under a day's worth — so it is genuinely rationed, which is
        // the point of it being last in the chain.
        assert_eq!(quota.polls_remaining(0), 100);
    }

    #[test]
    fn an_absent_account_never_gates_the_source() {
        // Anonymous access works fully; reporting "key required" would send a
        // user chasing an account they do not need.
        let src = OpenSky::new(HttpClient::new(std::time::Duration::from_secs(5)).unwrap());
        assert!(matches!(
            src.descriptor().auth,
            AuthRequirement::Optional { .. }
        ));
    }
}
