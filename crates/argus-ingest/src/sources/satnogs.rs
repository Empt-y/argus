//! SatNOGS ground stations: the amateur satellite-receiving network, with
//! what each station is listening to right now.
//!
//! SatNOGS is 4,470 registered stations — an antenna, a software radio and
//! a Raspberry Pi in someone's garden — scheduled centrally to record passes
//! of amateur and scientific satellites. The station list is one request
//! (3.4 MB, no paging). The schedule is a second endpoint, paged 25 at a
//! time, and a poll asks it for every observation starting or ending within
//! twenty minutes of now — 175 an hour, so a few pages — and attaches to each
//! station the pass it is recording, or the next one it will. The NORAD
//! number in that attachment is the join to the `satellites` layer.
//!
//! ## Most of the network is dark, and that is the fact to carry
//!
//! 4,135 of 4,470 stations are `Offline`; 305 are `Online` and every one of
//! those was seen within the day. 1,379 have never been seen at all. The
//! layer keeps them all, because a registered station is a fact about where
//! the network could listen from, and marks the difference: an online station
//! is [`Quality::Live`], anything else is [`Quality::Stale`] with its
//! `last_seen` carried, so a client draws a station that went quiet in 2022
//! as what it is. Poll time is the observation time, as for the outfalls and
//! the floats — the station has not moved; its status is what is observed.
//!
//! 207 stations are registered at 0°N 0°E. Null Island is not a garden; they
//! are dropped and counted.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::HashMap;

const STATIONS_URL: &str = "https://network.satnogs.org/api/stations/?format=json";
const OBSERVATIONS_URL: &str = "https://network.satnogs.org/api/observations/?format=json";

/// A pass lasts ten minutes and the schedule is set hours ahead; ten
/// minutes keeps "listening to" current without re-reading 3.4 MB of
/// station list more often than it changes.
const CADENCE_SECS: u64 = 600;

/// How far either side of now the schedule is asked about. A pass wholly
/// inside this window is either in progress or about to be.
const SCHEDULE_HALF_WINDOW: Duration = Duration::minutes(20);

/// A safety stop on schedule paging. Seven pages covered an hour when this
/// was written; forty is an outage of the paging, not a busy day.
const MAX_SCHEDULE_PAGES: usize = 40;

pub struct SatnogsStations {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl SatnogsStations {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("satnogs"),
                layer_id: LayerId::new("ground-stations"),
                display_name: "Satellite ground stations (SatNOGS)".into(),
                kind: EntityKind::Station,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "SatNOGS (Libre Space Foundation)".into(),
                    url: "https://network.satnogs.org/".into(),
                    license: "CC BY-SA 4.0".into(),
                    notice: Some(
                        "Ground station and observation data from the SatNOGS network".into(),
                    ),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }

    /// The schedule around `now`, following `Link: rel="next"` until it
    /// runs out. A page that fails ends the walk with what was collected: a
    /// station without its next pass is still a station.
    async fn schedule(&self, now: DateTime<Utc>) -> Vec<Pass> {
        let fmt = |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut url = format!(
            "{OBSERVATIONS_URL}&start={}&end={}",
            fmt(now - SCHEDULE_HALF_WINDOW),
            fmt(now + SCHEDULE_HALF_WINDOW)
        );
        let mut passes = Vec::new();
        for _ in 0..MAX_SCHEDULE_PAGES {
            let page = match self.http.get_page(&url).await {
                Ok(p) => p,
                Err(err) => {
                    tracing::warn!(source = %self.descriptor.id, %err, "schedule page failed; stations keep what was read");
                    break;
                }
            };
            match serde_json::from_slice::<Vec<Pass>>(&page.body) {
                Ok(mut batch) => passes.append(&mut batch),
                Err(err) => {
                    tracing::warn!(source = %self.descriptor.id, %err, "schedule page did not decode");
                    break;
                }
            }
            match page.next_link() {
                Some(next) => url = next,
                None => break,
            }
        }
        passes
    }
}

#[async_trait::async_trait]
impl Source for SatnogsStations {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let now = Utc::now();
        let stations: Vec<StationRecord> = self.http.get_json(STATIONS_URL).await?;
        let passes = self.schedule(now).await;
        let decoded = decode(stations, &passes, &self.descriptor.id, now);
        tracing::debug!(
            source = %self.descriptor.id,
            stations = decoded.observations.len(),
            unplaced = decoded.unplaced,
            passes = passes.len(),
            "satnogs decoded"
        );
        Ok(decoded.observations)
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StationRecord {
    id: i64,
    name: Option<String>,
    altitude: Option<f64>,
    min_horizon: Option<f64>,
    lat: Option<f64>,
    lng: Option<f64>,
    qthlocator: Option<String>,
    #[serde(default)]
    antenna: Vec<Antenna>,
    created: Option<String>,
    last_seen: Option<String>,
    observations: Option<i64>,
    future_observations: Option<i64>,
    description: Option<String>,
    client_version: Option<String>,
    /// A percentage on 2,137 stations and the boolean `false` on 2,333 —
    /// the ones with no observations to have a rate of. Declared as a
    /// number, one `false` failed the whole 4,470-station list.
    success_rate: Option<serde_json::Value>,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Antenna {
    frequency: Option<i64>,
    frequency_max: Option<i64>,
    band: Option<String>,
    antenna_type_name: Option<String>,
}

/// One scheduled observation. Only what a station card needs: the pass,
/// the satellite, and what is being listened for.
#[derive(Debug, Deserialize)]
pub struct Pass {
    id: i64,
    start: Option<String>,
    end: Option<String>,
    ground_station: Option<i64>,
    norad_cat_id: Option<i64>,
    /// The TLE name line, `0 CROCUBE`.
    tle0: Option<String>,
    status: Option<String>,
    transmitter_mode: Option<String>,
    transmitter_description: Option<String>,
    observation_frequency: Option<i64>,
    max_altitude: Option<f64>,
}

pub struct Decoded {
    pub observations: Vec<Observation>,
    /// Stations at Null Island or with no coordinates.
    pub unplaced: usize,
}

fn stamp(s: &Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().and_then(|s| s.parse().ok())
}

/// The satellite's name from a TLE name line: `0 CROCUBE` is `CROCUBE`.
fn tle_name(tle0: &str) -> &str {
    tle0.trim().strip_prefix("0 ").unwrap_or(tle0.trim()).trim()
}

pub fn decode(
    stations: Vec<StationRecord>,
    passes: &[Pass],
    source_id: &SourceId,
    now: DateTime<Utc>,
) -> Decoded {
    // Per station: the pass in progress, else the soonest one ahead.
    let mut current: HashMap<i64, &Pass> = HashMap::new();
    for pass in passes {
        let (Some(station), Some(start), Some(end)) =
            (pass.ground_station, stamp(&pass.start), stamp(&pass.end))
        else {
            continue;
        };
        if end < now {
            continue;
        }
        let in_progress = start <= now;
        match current.get(&station) {
            Some(existing) => {
                let existing_start = stamp(&existing.start).unwrap_or(now);
                let existing_in_progress = existing_start <= now;
                // A pass in progress beats one ahead; among passes ahead,
                // the sooner.
                if !existing_in_progress && (in_progress || start < existing_start) {
                    current.insert(station, pass);
                }
            }
            None => {
                current.insert(station, pass);
            }
        }
    }

    let horizon = EntityKind::Station
        .live_horizon()
        .expect("stations have a live horizon");
    let mut decoded = Decoded {
        observations: Vec::new(),
        unplaced: 0,
    };
    for s in stations {
        let (Some(lat), Some(lng)) = (s.lat, s.lng) else {
            decoded.unplaced += 1;
            continue;
        };
        if (lat == 0.0 && lng == 0.0)
            || !(-90.0..=90.0).contains(&lat)
            || !(-180.0..=180.0).contains(&lng)
        {
            decoded.unplaced += 1;
            continue;
        }
        let last_seen = stamp(&s.last_seen);
        let status = s.status.as_deref().unwrap_or("Unknown");
        // Online means the client checked in within minutes; every online
        // station in the live list had been seen within the day. Anything
        // else is a registration, not a receiver, until it comes back.
        let quality = if status == "Online"
            || status == "Testing" && last_seen.is_some_and(|t| now - t <= horizon)
        {
            Quality::Live
        } else {
            Quality::Stale
        };

        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("station_id", serde_json::json!(s.id));
        put("status", serde_json::json!(status));
        put("stale", serde_json::json!(quality == Quality::Stale));
        put(
            "last_seen",
            serde_json::json!(
                last_seen.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            ),
        );
        put(
            "created",
            serde_json::json!(
                stamp(&s.created).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            ),
        );
        put("altitude_m", serde_json::json!(s.altitude));
        put("min_horizon_deg", serde_json::json!(s.min_horizon));
        put(
            "qth_locator",
            serde_json::json!(s.qthlocator.as_deref().filter(|q| !q.is_empty())),
        );
        put("observations", serde_json::json!(s.observations));
        put(
            "future_observations",
            serde_json::json!(s.future_observations),
        );
        put(
            "success_rate_pct",
            serde_json::json!(s.success_rate.as_ref().and_then(serde_json::Value::as_f64)),
        );
        put(
            "client_version",
            serde_json::json!(s.client_version.as_deref().filter(|v| !v.is_empty())),
        );
        put(
            "description",
            serde_json::json!(
                s.description
                    .as_deref()
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
            ),
        );
        let antennas: Vec<serde_json::Value> = s
            .antenna
            .iter()
            .map(|a| {
                let mut m = serde_json::Map::new();
                if let Some(b) = &a.band {
                    m.insert("band".into(), serde_json::json!(b));
                }
                if let Some(t) = &a.antenna_type_name {
                    m.insert("type".into(), serde_json::json!(t));
                }
                if let Some(f) = a.frequency {
                    m.insert("freq_min_hz".into(), serde_json::json!(f));
                }
                if let Some(f) = a.frequency_max {
                    m.insert("freq_max_hz".into(), serde_json::json!(f));
                }
                serde_json::Value::Object(m)
            })
            .collect();
        if !antennas.is_empty() {
            put("antennas", serde_json::Value::Array(antennas));
        }
        let mut bands: Vec<&str> = s
            .antenna
            .iter()
            .filter_map(|a| a.band.as_deref())
            .flat_map(|b| b.split(',').map(str::trim))
            .filter(|b| !b.is_empty())
            .collect();
        bands.sort_unstable();
        bands.dedup();
        if !bands.is_empty() {
            put("bands", serde_json::json!(bands));
        }
        put(
            "url",
            serde_json::json!(format!("https://network.satnogs.org/stations/{}/", s.id)),
        );

        // The pass: in progress under `listening_to`, ahead under `next`.
        // Both name the satellite and its NORAD number, which is the key of
        // the satellites layer.
        if let Some(pass) = current.get(&s.id) {
            let mut p = serde_json::Map::new();
            let mut set = |k: &str, v: serde_json::Value| {
                if !v.is_null() {
                    p.insert(k.to_string(), v);
                }
            };
            set("observation_id", serde_json::json!(pass.id));
            set("norad_id", serde_json::json!(pass.norad_cat_id));
            set(
                "satellite",
                serde_json::json!(pass.tle0.as_deref().map(tle_name).filter(|n| !n.is_empty())),
            );
            set("start", serde_json::json!(pass.start));
            set("end", serde_json::json!(pass.end));
            set("status", serde_json::json!(pass.status));
            set(
                "mode",
                serde_json::json!(pass.transmitter_mode.as_deref().filter(|m| !m.is_empty())),
            );
            set(
                "transmitter",
                serde_json::json!(
                    pass.transmitter_description
                        .as_deref()
                        .filter(|m| !m.is_empty())
                ),
            );
            set(
                "frequency_hz",
                serde_json::json!(pass.observation_frequency),
            );
            set("max_elevation_deg", serde_json::json!(pass.max_altitude));
            let in_progress = stamp(&pass.start).is_some_and(|t| t <= now);
            put(
                if in_progress { "listening_to" } else { "next" },
                serde_json::Value::Object(p),
            );
        }

        let label = s
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Station {}", s.id));

        decoded.observations.push(
            Observation::new(
                source_id.clone(),
                EntityId::new(EntityKind::Station, s.id.to_string()),
                now,
                quality,
            )
            .with_position(Position {
                lon: lng,
                lat,
                alt_m: None,
                datum: AltitudeDatum::Geoid,
            })
            .with_label(label)
            .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> SourceId {
        SourceId::new("satnogs")
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    const NOW: &str = "2026-09-16T12:00:00Z";

    /// Three live records: an online station, one offline since 2022, and
    /// one registered at Null Island.
    const STATIONS: &str = r#"[
      {"id": 2380, "name": "Piszkesteto UHF", "altitude": 954, "min_horizon": 10, "horizon_hard_limit": false, "min_culmination": 10, "min_culmination_hard_limit": false, "lat": 47.917, "lng": 19.895, "qthlocator": "JN97wv", "antenna": [{"frequency": 430000000, "frequency_max": 440000000, "band": "UHF", "antenna_type": "yagi", "antenna_type_name": "Yagi"}, {"frequency": 144000000, "frequency_max": 146000000, "band": "VHF", "antenna_type": "turnstile", "antenna_type_name": "Turnstile"}], "created": "2020-01-01T00:00:00Z", "last_seen": "2026-09-16T11:58:00Z", "observations": 5000, "future_observations": 3, "description": "", "client_version": "1.9.1", "target_utilization": 50, "image": "", "success_rate": 88.5, "owner": "someone", "is_connected": true, "is_available": true, "testing": false, "status": "Online"},
      {"id": 1, "name": "Hackerspace.gr 1", "altitude": 104, "min_horizon": 40, "horizon_hard_limit": false, "min_culmination": 10, "min_culmination_hard_limit": false, "lat": 38.01697, "lng": 23.7314, "qthlocator": "KM18ua", "antenna": [{"frequency": 400000000, "frequency_max": 460000000, "band": "UHF", "antenna_type": "cross-yagi", "antenna_type_name": "Cross Yagi"}], "created": "2015-07-22T13:26:49Z", "last_seen": "2022-10-05T12:49:26Z", "observations": 10624, "future_observations": 0, "description": "Yaesu 5500", "client_version": "1.6", "target_utilization": 100, "image": "", "success_rate": false, "owner": "x", "is_connected": false, "is_available": false, "testing": false, "status": "Offline"},
      {"id": 999, "name": "Nowhere", "altitude": 0, "min_horizon": 0, "horizon_hard_limit": false, "min_culmination": 0, "min_culmination_hard_limit": false, "lat": 0.0, "lng": 0.0, "qthlocator": "JJ00aa", "antenna": [], "created": "2024-01-01T00:00:00Z", "last_seen": null, "observations": 0, "future_observations": 0, "description": null, "client_version": "", "target_utilization": 0, "image": "", "success_rate": 0, "owner": "y", "is_connected": false, "is_available": false, "testing": false, "status": "Offline"}
    ]"#;

    const PASSES: &str = r#"[
      {"id": 15008368, "start": "2026-09-16T11:55:00Z", "end": "2026-09-16T12:05:00Z", "ground_station": 2380, "norad_cat_id": 62394, "station_name": "Piszkesteto UHF", "status": "unknown", "tle0": "0 CROCUBE", "transmitter_mode": "GFSK", "transmitter_description": "Mode U/U - GFSK9k6", "observation_frequency": 436775000, "max_altitude": 38.0},
      {"id": 15008369, "start": "2026-09-16T12:10:00Z", "end": "2026-09-16T12:19:00Z", "ground_station": 2380, "norad_cat_id": 25544, "status": "future", "tle0": "0 ISS (ZARYA)", "transmitter_mode": "FM", "transmitter_description": "Mode V/U FM", "observation_frequency": 437800000, "max_altitude": 60.0},
      {"id": 15008370, "start": "2026-09-16T11:40:00Z", "end": "2026-09-16T11:50:00Z", "ground_station": 1, "norad_cat_id": 40967, "status": "good", "tle0": "0 FOX-1A", "transmitter_mode": "FM", "transmitter_description": "", "observation_frequency": 145980000, "max_altitude": 20.0}
    ]"#;

    fn stations() -> Vec<StationRecord> {
        serde_json::from_str(STATIONS).unwrap()
    }
    fn passes() -> Vec<Pass> {
        serde_json::from_str(PASSES).unwrap()
    }

    #[test]
    fn null_island_is_not_a_garden() {
        let d = decode(stations(), &passes(), &source(), at(NOW));
        assert_eq!(d.observations.len(), 2);
        assert_eq!(d.unplaced, 1);
        assert!(d.observations.iter().all(|o| o.entity.key != "999"));
    }

    #[test]
    fn an_online_station_is_live_and_a_dark_one_is_stale_with_its_last_seen() {
        let d = decode(stations(), &passes(), &source(), at(NOW));
        let online = &d.observations[0];
        assert_eq!(online.entity.key, "2380");
        assert_eq!(online.quality, Quality::Live);
        assert_eq!(online.observed_at, at(NOW), "dated by the poll");
        let dark = &d.observations[1];
        assert_eq!(dark.quality, Quality::Stale);
        assert_eq!(dark.attrs["stale"], serde_json::json!(true));
        assert_eq!(
            dark.attrs["last_seen"],
            serde_json::json!("2022-10-05T12:49:26Z")
        );
        assert_eq!(dark.attrs["status"], serde_json::json!("Offline"));
    }

    #[test]
    fn the_pass_in_progress_wins_over_the_next_one_and_names_the_satellite() {
        let d = decode(stations(), &passes(), &source(), at(NOW));
        let s = &d.observations[0];
        let listening = &s.attrs["listening_to"];
        assert_eq!(listening["norad_id"], serde_json::json!(62394));
        assert_eq!(listening["satellite"], serde_json::json!("CROCUBE"));
        assert_eq!(listening["mode"], serde_json::json!("GFSK"));
        assert_eq!(listening["frequency_hz"], serde_json::json!(436775000));
        assert!(
            s.attrs.get("next").is_none(),
            "one pass per station, the current one"
        );
    }

    #[test]
    fn with_nothing_in_progress_the_soonest_future_pass_is_next() {
        let d = decode(stations(), &passes(), &source(), at("2026-09-16T12:06:00Z"));
        let s = &d.observations[0];
        assert!(s.attrs.get("listening_to").is_none());
        assert_eq!(s.attrs["next"]["norad_id"], serde_json::json!(25544));
        assert_eq!(
            s.attrs["next"]["satellite"],
            serde_json::json!("ISS (ZARYA)")
        );
    }

    #[test]
    fn a_pass_that_has_ended_is_not_attached() {
        // Station 1's only pass ended at 11:50.
        let d = decode(stations(), &passes(), &source(), at(NOW));
        let s = &d.observations[1];
        assert!(s.attrs.get("listening_to").is_none());
        assert!(s.attrs.get("next").is_none());
    }

    #[test]
    fn antennas_and_bands_are_carried_and_the_label_is_the_name() {
        let d = decode(stations(), &passes(), &source(), at(NOW));
        let s = &d.observations[0];
        assert_eq!(s.label.as_deref(), Some("Piszkesteto UHF"));
        assert_eq!(s.attrs["bands"], serde_json::json!(["UHF", "VHF"]));
        assert_eq!(s.attrs["antennas"][0]["type"], serde_json::json!("Yagi"));
        assert_eq!(
            s.attrs["antennas"][0]["freq_min_hz"],
            serde_json::json!(430000000)
        );
        assert_eq!(
            s.attrs["url"],
            serde_json::json!("https://network.satnogs.org/stations/2380/")
        );
        assert!(
            s.attrs.get("description").is_none(),
            "an empty description is no description"
        );
    }

    #[test]
    fn a_boolean_success_rate_is_no_rate_and_not_a_decode_failure() {
        // 2,333 of 4,470 stations send `false` where the rest send a number.
        let d = decode(stations(), &passes(), &source(), at(NOW));
        assert_eq!(
            d.observations[0].attrs["success_rate_pct"],
            serde_json::json!(88.5)
        );
        assert!(d.observations[1].attrs.get("success_rate_pct").is_none());
    }

    #[test]
    fn a_tle_name_line_loses_its_leading_zero() {
        assert_eq!(tle_name("0 CROCUBE"), "CROCUBE");
        assert_eq!(tle_name("ISS (ZARYA)"), "ISS (ZARYA)");
        assert_eq!(tle_name(" 0 X "), "X");
    }
}
