//! Raspberry Shake seismographs: the citizen seismic network, from its
//! FDSN station service.
//!
//! A Raspberry Shake is a geophone, or an accelerometer, or an infrasound
//! boom, on a Raspberry Pi under someone's stairs, streaming to a central
//! archive that speaks the FDSN web-service standard. The station service
//! answers a channel-level query for the whole `AM` network in one 2 MB
//! text response: 28,000 station epochs since 2016, of which 6,186 have no
//! end date and are the network as it stands. `endafter=now` asks for only
//! those, and the channel level is what tells one kind of Shake from
//! another — the metadata names every station "Raspberry Shake Citizen
//! Science Station", and the only identity a station has beyond its code is
//! the set of channels it records.
//!
//! There is no availability service on this host (`/fdsnws/availability/1`
//! is a 404), so whether a station is streaming right now cannot be
//! known from here; an open epoch is the network's own statement that it
//! is active. Poll time is the observation time, as for the other fixture
//! layers, and the epoch's start is carried as `installed`. The service
//! redirects `service.iris.edu` style requests; the client follows.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

const STATION_URL: &str =
    "https://data.raspberryshake.org/fdsnws/station/1/query?network=AM&level=channel&format=text";

/// Stations come and go by the day, not the minute. Six hours keeps the
/// layer within its 24 h station horizon with room for a failed poll.
const CADENCE_SECS: u64 = 6 * 3600;

pub struct RaspberryShake {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl RaspberryShake {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("raspberry-shake"),
                layer_id: LayerId::new("seismographs"),
                display_name: "Raspberry Shake seismographs".into(),
                kind: EntityKind::Station,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "Raspberry Shake / OSOP".into(),
                    url: "https://raspberryshake.org/".into(),
                    license: "CC BY 4.0".into(),
                    notice: Some(
                        "Station metadata from the Raspberry Shake FDSN web service".into(),
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
impl Source for RaspberryShake {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let now = Utc::now();
        let url = format!("{STATION_URL}&endafter={}", now.format("%Y-%m-%dT%H:%M:%S"));
        let bytes = self.http.get_bytes(&url).await?;
        let text = String::from_utf8_lossy(&bytes);
        let decoded = decode(&text, &self.descriptor.id, now)?;
        tracing::info!(
            source = %self.descriptor.id,
            stations = decoded.observations.len(),
            channels = decoded.channels,
            unplaced = decoded.unplaced,
            "raspberry shake network read"
        );
        Ok(decoded.observations)
    }
}

// --- wire format -----------------------------------------------------------

/// One row of the channel-level text response.
///
/// `#Network|Station|Location|Channel|Latitude|Longitude|Elevation|Depth|
/// Azimuth|Dip|SensorDescription|Scale|ScaleFreq|ScaleUnits|SampleRate|
/// StartTime|EndTime`
#[derive(Debug, Clone)]
struct ChannelRow<'a> {
    station: &'a str,
    channel: &'a str,
    lat: f64,
    lon: f64,
    elevation_m: Option<f64>,
    sample_rate: Option<f64>,
    start: Option<DateTime<Utc>>,
}

fn parse_row(line: &str) -> Option<ChannelRow<'_>> {
    let f: Vec<&str> = line.split('|').collect();
    if f.len() < 16 || f[0] != "AM" {
        return None;
    }
    Some(ChannelRow {
        station: f[1].trim(),
        channel: f[3].trim(),
        lat: f[4].trim().parse().ok()?,
        lon: f[5].trim().parse().ok()?,
        elevation_m: f[6].trim().parse().ok(),
        sample_rate: f[14].trim().parse().ok(),
        start: parse_stamp(f[15]),
    })
}

/// `2023-01-01T19:53:05.863`: no zone, and the FDSN standard says UTC.
fn parse_stamp(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .ok()
        .map(|t| t.and_utc())
}

/// Which Shake this is, from the channels it records.
///
/// The product line is the channel set: a 1D has one vertical geophone
/// (`EHZ`), a 3D three (`EHZ`, `EHN`, `EHE`), a 4D the vertical geophone
/// plus a three-axis accelerometer (`EN?`), a Boom one infrasound channel
/// (`HDF`), and a Shake & Boom the geophone and the boom. `SHZ` is the
/// vertical geophone on the original 50 Hz units.
fn model(channels: &[&str]) -> &'static str {
    let has = |c: &str| channels.contains(&c);
    let geophone_z = has("EHZ") || has("SHZ");
    let geophone_h = has("EHN") || has("EHE") || has("SHN") || has("SHE");
    let accel = has("ENZ") || has("ENN") || has("ENE");
    let boom = has("HDF");
    match (geophone_z, geophone_h, accel, boom) {
        (true, false, false, true) => "Raspberry Shake & Boom",
        (false, false, false, true) => "Raspberry Boom",
        (true, _, true, _) => "Raspberry Shake 4D",
        (true, true, false, _) => "Raspberry Shake 3D",
        (true, false, false, false) => "Raspberry Shake 1D",
        _ => "Raspberry Shake",
    }
}

fn senses(channels: &[&str]) -> Vec<&'static str> {
    let mut out = Vec::new();
    if channels.iter().any(|c| c.starts_with("EH") || c.starts_with("SH")) {
        out.push("ground velocity");
    }
    if channels.iter().any(|c| c.starts_with("EN")) {
        out.push("ground acceleration");
    }
    if channels.contains(&"HDF") {
        out.push("infrasound");
    }
    out
}

#[derive(Debug)]
pub struct Decoded {
    pub observations: Vec<Observation>,
    pub channels: usize,
    pub unplaced: usize,
}

/// Group the channel rows by station and make one observation per station.
pub fn decode(text: &str, source_id: &SourceId, now: DateTime<Utc>) -> Result<Decoded, SourceError> {
    let mut rows = 0;
    let mut by_station: BTreeMap<&str, Vec<ChannelRow<'_>>> = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        rows += 1;
        if let Some(row) = parse_row(line) {
            by_station.entry(row.station).or_default().push(row);
        }
    }
    if rows > 0 && by_station.is_empty() {
        return Err(SourceError::Decode(
            "the station service answered, but no row was a channel of the AM network".into(),
        ));
    }

    let mut observations = Vec::with_capacity(by_station.len());
    let mut unplaced = 0;
    for (station, rows) in &by_station {
        let first = &rows[0];
        let (lat, lon) = (first.lat, first.lon);
        if (lat == 0.0 && lon == 0.0) || !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            unplaced += 1;
            continue;
        }
        let mut channels: Vec<&str> = rows.iter().map(|r| r.channel).collect();
        channels.sort_unstable();
        channels.dedup();
        let model = model(&channels);
        let installed = rows.iter().filter_map(|r| r.start).min();
        let sample_rate = rows.iter().filter_map(|r| r.sample_rate).fold(None, |m: Option<f64>, r| Some(m.map_or(r, |m| m.max(r))));

        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("station", serde_json::json!(station));
        put("network", serde_json::json!("AM"));
        put("model", serde_json::json!(model));
        put("senses", serde_json::json!(senses(&channels)));
        put("channels", serde_json::json!(channels));
        put("sample_rate_hz", serde_json::json!(sample_rate));
        put("elevation_m", serde_json::json!(first.elevation_m));
        put(
            "installed",
            serde_json::json!(installed.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))),
        );
        put(
            "url",
            serde_json::json!(format!("https://stationview.raspberryshake.org/#?net=AM&sta={station}")),
        );

        observations.push(
            Observation::new(
                source_id.clone(),
                EntityId::new(EntityKind::Station, format!("AM.{station}")),
                now,
                Quality::Live,
            )
            .with_position(Position {
                lon,
                lat,
                alt_m: first.elevation_m,
                datum: AltitudeDatum::Geoid,
            })
            .with_label(format!("{model} {station}"))
            .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    Ok(Decoded {
        observations,
        channels: rows,
        unplaced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "#Network|Station|Location|Channel|Latitude|Longitude|Elevation|Depth|Azimuth|Dip|SensorDescription|Scale|ScaleFreq|ScaleUnits|SampleRate|StartTime|EndTime
AM|R00C3|00|EHZ|51.47747747747748|-3.6602824535612903|10.0|0.0|0.0|-90.0|Velocity|399650000.0|5.0|M/S|100.0|2023-01-01T19:53:05.863|
AM|R0A1B|00|EHZ|55.9|-3.2|80.0|0.0|0.0|-90.0|Velocity|399650000.0|5.0|M/S|100.0|2019-05-02T10:00:00.000|
AM|R0A1B|00|ENZ|55.9|-3.2|80.0|0.0|0.0|-90.0|Acceleration|1.0|5.0|M/S**2|100.0|2019-05-02T10:00:00.000|
AM|R0A1B|00|ENN|55.9|-3.2|80.0|0.0|0.0|0.0|Acceleration|1.0|5.0|M/S**2|100.0|2019-05-02T10:00:00.000|
AM|R0A1B|00|ENE|55.9|-3.2|80.0|0.0|0.0|90.0|Acceleration|1.0|5.0|M/S**2|100.0|2019-05-02T10:00:00.000|
AM|RB00M|00|HDF|52.0|-1.0|100.0|0.0|0.0|0.0|Atmospheric Pressure|56000.0|1.0|PA|100.0|2021-01-01T00:00:00.000|
AM|RNULL|00|EHZ|0.0|0.0|0.0|0.0|0.0|-90.0|Velocity|399650000.0|5.0|M/S|100.0|2021-01-01T00:00:00.000|
AM|R3D00|00|EHZ|50.0|-5.0|30.0|0.0|0.0|-90.0|Velocity|399650000.0|5.0|M/S|100.0|2020-01-01T00:00:00.000|
AM|R3D00|00|EHN|50.0|-5.0|30.0|0.0|0.0|0.0|Velocity|399650000.0|5.0|M/S|100.0|2020-01-01T00:00:00.000|
AM|R3D00|00|EHE|50.0|-5.0|30.0|0.0|0.0|90.0|Velocity|399650000.0|5.0|M/S|100.0|2020-01-01T00:00:00.000|
";

    #[test]
    fn channels_group_into_stations_and_name_the_model() {
        let now = Utc::now();
        let d = decode(FIXTURE, &SourceId::new("raspberry-shake"), now).unwrap();
        assert_eq!(d.channels, 10);
        assert_eq!(d.unplaced, 1, "Null Island is dropped");
        assert_eq!(d.observations.len(), 4);
        let by_key = |k: &str| d.observations.iter().find(|o| o.entity.key == k).unwrap();
        assert_eq!(by_key("AM.R00C3").attrs["model"], "Raspberry Shake 1D");
        assert_eq!(by_key("AM.R0A1B").attrs["model"], "Raspberry Shake 4D");
        assert_eq!(by_key("AM.RB00M").attrs["model"], "Raspberry Boom");
        assert_eq!(by_key("AM.R3D00").attrs["model"], "Raspberry Shake 3D");
        assert_eq!(by_key("AM.R0A1B").attrs["senses"], serde_json::json!(["ground velocity", "ground acceleration"]));
        assert_eq!(by_key("AM.R00C3").attrs["installed"], "2023-01-01T19:53:05Z");
        let o = by_key("AM.R00C3");
        assert_eq!(o.observed_at, now, "poll time is the observation time");
        assert_eq!(o.entity.kind, EntityKind::Station);
        assert_eq!(o.label.as_deref(), Some("Raspberry Shake 1D R00C3"));
        let p = o.position.unwrap();
        assert!((p.lat - 51.4775).abs() < 1e-3 && (p.lon + 3.6603).abs() < 1e-3);
    }

    #[test]
    fn a_body_with_rows_but_no_channels_is_a_decode_error_not_an_empty_network() {
        let err = decode("#header\nXX|foo|bar\n", &SourceId::new("raspberry-shake"), Utc::now()).unwrap_err();
        assert!(matches!(err, SourceError::Decode(_)), "{err}");
        // But an empty body is an empty network.
        assert!(decode("", &SourceId::new("raspberry-shake"), Utc::now()).unwrap().observations.is_empty());
    }
}
