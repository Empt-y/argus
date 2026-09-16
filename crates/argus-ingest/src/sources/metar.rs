//! Aerodrome weather: the latest METAR from every reporting aerodrome on
//! Earth, with its TAF attached, from the Aviation Weather Center's bulk
//! cache files.
//!
//! A METAR is the routine surface observation an aerodrome issues every hour
//! or half hour — wind, visibility, present weather, cloud layers, temperature,
//! pressure — and a SPECI is the same message issued off-schedule because
//! something changed. A TAF is the aerodrome's forecast, issued four times a
//! day and amended when it goes wrong. The station is the aerodrome; the
//! observation is its latest report; the forecast is an attribute of the
//! station, because a TAF without the aerodrome it belongs to is not a thing
//! anyone asks for.
//!
//! ## The query API thins by area, so this does not use it
//!
//! `api/data/metar?bbox=` is what the research note pointed at, and for the
//! UK box it returns 62 stations, which looked complete. It is not. The same
//! box split in two returns 90, split in four returns 83 for the southern
//! half alone, and the whole of Europe returns 100 — one airport per country,
//! roughly. The endpoint decimates by bounding-box area for AWC's own map
//! tiles; its `help=true` error names `zoom` and `density` parameters the
//! public spec does not document. The bulk cache is the whole network in one
//! 250 KB gzip: 5,128 stations when this was written, against 110 in the UK
//! box the API would have thinned to 62. So the layer is global for the same
//! reason the buoys are — one request returns everything — and the coverage
//! is honest.
//!
//! ## Traps found by counting the whole file, not reading one row
//!
//! - 23 stations carry `-99.9900` for both coordinates and `9999` for their
//!   elevation, and one has both coordinates empty. Most are `KQ`-prefixed US
//!   military sites deployed somewhere the position is deliberately not
//!   published; AWC's station table hides them the same way. An unplaced
//!   station is dropped and counted, never drawn at 99°S.
//! - `flight_category` is the literal string `null` on 368 rows, not an empty
//!   cell. Those are reports too damaged to categorise (`//// R/////// //`).
//! - Visibility is `10+` on 2,045 rows and `6+` on 1,741: a floor, not a
//!   value, because the report said "ten or more". The number is kept
//!   comparable and the floor is flagged separately.
//! - The four max/min temperature columns are not usable. `maxT24hr_c` is in
//!   tenths (`289` at a station reading 26 °C) and `maxT_c` gives Svalbard a
//!   27 °C maximum on a 5 °C afternoon. Seventeen rows between them, all
//!   wrong; they are skipped rather than published.
//! - `vert_vis_ft` is in hundreds of feet despite its name (`VV002` → `2`).
//!   The same value appears correctly scaled as the base of an `OVX` cloud
//!   layer, which is what is carried.
//! - Four raw reports contain commas. The file is real CSV with quoting, not
//!   split-on-comma.
//!
//! Units are kept as AWC publishes them and named in the keys: temperatures
//! in °C, wind in knots, visibility in statute miles, altimeter in inches of
//! mercury, sea-level pressure in hPa, cloud bases in feet above ground.
//! Converting a whole-number altimeter setting into hectopascals would print
//! precision the report never carried.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const METAR_URL: &str = "https://aviationweather.gov/data/cache/metars.cache.csv.gz";
const TAF_URL: &str = "https://aviationweather.gov/data/cache/tafs.cache.xml.gz";
const STATIONS_URL: &str = "https://aviationweather.gov/data/cache/stations.cache.json.gz";

/// METARs are issued on the hour and half hour, SPECIs whenever conditions
/// change, and AWC rebuilds the cache every minute. Five minutes sees a SPECI
/// while it is still the current report.
const CADENCE_SECS: u64 = 300;

/// TAFs are issued every six hours and amended at any time; half an hour is
/// well inside the life of a forecast and one twelfth of the METAR traffic.
const TAF_TTL: Duration = Duration::from_secs(30 * 60);

/// Aerodrome names change when an aerodrome is renamed, which is to say
/// rarely. Once a day, as for the NDBC station table.
const STATIONS_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The header the cache file has carried since it was captured. Every column
/// is read by position — four of them are called `sky_cover` — so a header
/// that differs from this one means the positions have moved, and the right
/// answer is a loud decode error rather than a temperature stored as a wind.
const HEADER: [&str; 44] = [
    "raw_text",
    "station_id",
    "observation_time",
    "latitude",
    "longitude",
    "temp_c",
    "dewpoint_c",
    "wind_dir_degrees",
    "wind_speed_kt",
    "wind_gust_kt",
    "visibility_statute_mi",
    "altim_in_hg",
    "sea_level_pressure_mb",
    "corrected",
    "auto",
    "auto_station",
    "maintenance_indicator_on",
    "no_signal",
    "lightning_sensor_off",
    "freezing_rain_sensor_off",
    "present_weather_sensor_off",
    "wx_string",
    "sky_cover",
    "cloud_base_ft_agl",
    "sky_cover",
    "cloud_base_ft_agl",
    "sky_cover",
    "cloud_base_ft_agl",
    "sky_cover",
    "cloud_base_ft_agl",
    "flight_category",
    "three_hr_pressure_tendency_mb",
    "maxT_c",
    "minT_c",
    "maxT24hr_c",
    "minT24hr_c",
    "precip_in",
    "pcp3hr_in",
    "pcp6hr_in",
    "pcp24hr_in",
    "snow_in",
    "vert_vis_ft",
    "metar_type",
    "elevation_m",
];

/// The position AWC writes for a station whose position it will not publish.
const UNPLACED: f64 = -99.99;

pub struct Metars {
    descriptor: SourceDescriptor,
    http: HttpClient,
    tafs: Reference<HashMap<String, Taf>>,
    stations: Reference<HashMap<String, StationMeta>>,
}

/// A reference file that changes slowly: fetched on first use, trusted for a
/// TTL, and kept past it if the refresh fails. A METAR without its TAF is
/// still a METAR; a poll that emits nothing because a forecast file was slow
/// is a broken layer.
struct Reference<T> {
    ttl: Duration,
    cached: RwLock<Option<(Instant, Arc<T>)>>,
}

impl<T> Reference<T> {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            cached: RwLock::new(None),
        }
    }

    /// The cached value if it is within its TTL; otherwise whatever `fetch`
    /// returns, or the stale value if it fails, or nothing if there never was
    /// one.
    async fn get<F, Fut>(&self, what: &str, source: &SourceId, fetch: F) -> Option<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, SourceError>>,
    {
        if let Some((fetched, value)) = self.cached.read().await.as_ref()
            && fetched.elapsed() < self.ttl
        {
            return Some(value.clone());
        }
        match fetch().await {
            Ok(value) => {
                let value = Arc::new(value);
                *self.cached.write().await = Some((Instant::now(), value.clone()));
                Some(value)
            }
            Err(err) => {
                tracing::warn!(source = %source, %err, "could not refresh the {what}; keeping the last one");
                self.cached.read().await.as_ref().map(|(_, v)| v.clone())
            }
        }
    }
}

/// What the station table knows that the observation does not.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StationMeta {
    pub name: Option<String>,
    pub country: Option<String>,
    pub iata: Option<String>,
}

/// One aerodrome's current forecast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taf {
    pub raw: String,
    pub issued: Option<DateTime<Utc>>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
}

impl Metars {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("awc-metar"),
                layer_id: LayerId::new("metars"),
                display_name: "Aerodrome weather (METAR/TAF)".into(),
                kind: EntityKind::Station,
                cadence: Cadence::every(CADENCE_SECS),
                // One file, every aerodrome that reports, on every continent.
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
            tafs: Reference::new(TAF_TTL),
            stations: Reference::new(STATIONS_TTL),
        }
    }

    /// Fetch a cache file and hand back its text, inflating it if it arrived
    /// as gzip. The files are served as `application/octet-stream` with no
    /// `Content-Encoding`, so the HTTP layer will not inflate them, but if AWC
    /// ever starts doing so this must not try to inflate plain text.
    async fn fetch_text(&self, url: &str) -> Result<String, SourceError> {
        let bytes = self.http.get_bytes(url).await?;
        inflate(&bytes)
    }
}

/// Bytes to text, through gzip if the magic says so.
fn inflate(bytes: &[u8]) -> Result<String, SourceError> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut text = String::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_string(&mut text)
            .map_err(|e| SourceError::Decode(format!("gzip: {e}")))?;
        Ok(text)
    } else {
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }
}

#[async_trait::async_trait]
impl Source for Metars {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let text = self.fetch_text(METAR_URL).await?;
        let now = Utc::now();
        let tafs = self
            .tafs
            .get("TAF cache", &self.descriptor.id, || async {
                let text = self.fetch_text(TAF_URL).await?;
                decode_tafs(&text)
            })
            .await;
        let stations = self
            .stations
            .get("station table", &self.descriptor.id, || async {
                let text = self.fetch_text(STATIONS_URL).await?;
                decode_stations(&text)
            })
            .await;

        let empty_tafs = HashMap::new();
        let empty_stations = HashMap::new();
        let decoded = decode(
            &text,
            tafs.as_deref().unwrap_or(&empty_tafs),
            stations.as_deref().unwrap_or(&empty_stations),
            &self.descriptor.id,
            now,
        )?;
        if decoded.malformed > 0 || decoded.unplaced > 0 {
            tracing::debug!(
                source = %self.descriptor.id,
                malformed = decoded.malformed,
                unplaced = decoded.unplaced,
                emitted = decoded.observations.len(),
                "METAR rows skipped"
            );
        }
        Ok(decoded.observations)
    }
}

/// What a decode produced, and what it refused.
pub struct Decoded {
    pub observations: Vec<Observation>,
    /// Rows without 44 fields, or whose time or id could not be read.
    pub malformed: usize,
    /// Rows for a station whose position AWC does not publish.
    pub unplaced: usize,
}

/// Decode `metars.cache.csv` against the forecasts and station table.
///
/// A header that is not the one this decoder was written for is an error for
/// the whole poll, not a warning: with columns read by position, a shifted
/// header means every value lands under the wrong name.
pub fn decode(
    text: &str,
    tafs: &HashMap<String, Taf>,
    stations: &HashMap<String, StationMeta>,
    source_id: &SourceId,
    now: DateTime<Utc>,
) -> Result<Decoded, SourceError> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());
    let mut records = reader.records();

    let header = records
        .next()
        .ok_or_else(|| SourceError::Decode("empty METAR file".into()))?
        .map_err(|e| SourceError::Decode(e.to_string()))?;
    if header.iter().ne(HEADER.iter().copied()) {
        return Err(SourceError::Decode(format!(
            "METAR header changed: {} columns, starting {:?}",
            header.len(),
            header.iter().take(3).collect::<Vec<_>>()
        )));
    }

    let mut decoded = Decoded {
        observations: Vec::new(),
        malformed: 0,
        unplaced: 0,
    };
    for record in records {
        let Ok(record) = record else {
            decoded.malformed += 1;
            continue;
        };
        if record.len() != HEADER.len() {
            decoded.malformed += 1;
            continue;
        }
        let fields: Vec<&str> = record.iter().collect();
        match decode_row(&fields, tafs, stations, source_id, now) {
            Row::Observed(o) => decoded.observations.push(*o),
            Row::Unplaced => decoded.unplaced += 1,
            Row::Malformed => decoded.malformed += 1,
        }
    }
    Ok(decoded)
}

enum Row {
    Observed(Box<Observation>),
    Unplaced,
    Malformed,
}

/// A numeric cell, or nothing. AWC leaves an unreported value empty.
fn number(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "null" {
        return None;
    }
    raw.parse().ok()
}

/// A flag cell: `TRUE` or empty, never `FALSE`.
fn flag(raw: &str) -> bool {
    raw.trim().eq_ignore_ascii_case("true")
}

/// A text cell, or nothing. `null` is spelled out in `flight_category`.
fn text(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    (!raw.is_empty() && raw != "null").then_some(raw)
}

fn decode_row(
    f: &[&str],
    tafs: &HashMap<String, Taf>,
    stations: &HashMap<String, StationMeta>,
    source_id: &SourceId,
    now: DateTime<Utc>,
) -> Row {
    let Some(icao) = text(f[1]) else {
        return Row::Malformed;
    };
    let key = icao.to_ascii_uppercase();

    let Ok(observed_at) = f[2].trim().parse::<DateTime<Utc>>() else {
        return Row::Malformed;
    };

    // An empty coordinate is not zero. Tarbes (`LFBT`) sits at 0.000°E and
    // the file prints its longitude as an empty cell — probably a formatter
    // dropping a zero — but reading empty as zero would also place every
    // genuinely unknown position on the meridian, so it is unplaced with the
    // rest.
    let (Some(lat), Some(lon)) = (number(f[3]), number(f[4])) else {
        return Row::Unplaced;
    };
    if lat == UNPLACED && lon == UNPLACED {
        return Row::Unplaced;
    }
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Row::Malformed;
    }

    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };

    put("station", serde_json::json!(key));
    put("report_type", serde_json::json!(text(f[42])));
    put("temp_c", serde_json::json!(number(f[5])));
    put("dewpoint_c", serde_json::json!(number(f[6])));
    put("wind_dir_deg", serde_json::json!(number(f[7])));
    put("wind_speed_kt", serde_json::json!(number(f[8])));
    put("gust_kt", serde_json::json!(number(f[9])));

    // `10+` is "ten statute miles or more". The number stays comparable and
    // the floor is flagged, rather than storing a string a client cannot
    // sort by or dropping the distinction between 10 and at-least-10.
    if let Some(vis) = text(f[10]) {
        let or_more = vis.ends_with('+');
        if let Some(mi) = number(vis.trim_end_matches('+')) {
            put("visibility_mi", serde_json::json!(mi));
            if or_more {
                put("visibility_or_more", serde_json::json!(true));
            }
        }
    }

    put("altimeter_inhg", serde_json::json!(number(f[11])));
    put("sea_level_pressure_hpa", serde_json::json!(number(f[12])));
    put("pressure_tendency_hpa", serde_json::json!(number(f[31])));
    put("weather", serde_json::json!(text(f[21])));

    // Up to four cloud layers, lowest first as the report lists them. An
    // `OVX` layer is sky obscured, and its base is the vertical visibility.
    let clouds: Vec<serde_json::Value> = [22, 24, 26, 28]
        .into_iter()
        .filter_map(|i| {
            let cover = text(f[i])?;
            let mut layer = serde_json::Map::new();
            layer.insert("cover".into(), serde_json::json!(cover));
            if let Some(base) = number(f[i + 1]) {
                layer.insert("base_ft".into(), serde_json::json!(base));
            }
            Some(serde_json::Value::Object(layer))
        })
        .collect();
    if !clouds.is_empty() {
        put("clouds", serde_json::Value::Array(clouds));
    }

    put("flight_category", serde_json::json!(text(f[30])));
    put("precip_in", serde_json::json!(number(f[36])));
    put("precip_3h_in", serde_json::json!(number(f[37])));
    put("precip_6h_in", serde_json::json!(number(f[38])));
    put("precip_24h_in", serde_json::json!(number(f[39])));
    put("snow_in", serde_json::json!(number(f[40])));
    put("elevation_m", serde_json::json!(number(f[43])));

    // Report flags, present only when set. `no_signal` is AWC's name for the
    // `NOSIG` trend group — "no significant change expected" — and has nothing
    // to do with signal; every one of the 746 flagged reports carried NOSIG.
    // `maintenance` is the `$` a US automated station appends when it needs
    // attention.
    for (name, col) in [
        ("auto", 14),
        ("corrected", 13),
        ("maintenance", 16),
        ("nosig", 17),
    ] {
        if flag(f[col]) {
            put(name, serde_json::json!(true));
        }
    }
    let sensors_off: Vec<&str> = [
        ("lightning", 18),
        ("freezing_rain", 19),
        ("present_weather", 20),
    ]
    .into_iter()
    .filter(|(_, col)| flag(f[*col]))
    .map(|(name, _)| name)
    .collect();
    if !sensors_off.is_empty() {
        put("sensors_off", serde_json::json!(sensors_off));
    }

    if let Some(meta) = stations.get(&key) {
        put("name", serde_json::json!(meta.name));
        put("country", serde_json::json!(meta.country));
        put("iata", serde_json::json!(meta.iata));
    }

    // The forecast, if there is a current one. An expired TAF is not a
    // forecast, whatever the file still lists — the same rule as an expired
    // SIGMET. One not yet in force is the next forecast and is what a pilot
    // would read, so it stays.
    if let Some(taf) = tafs.get(&key)
        && taf.valid_to.is_none_or(|t| t >= now)
    {
        put("taf", serde_json::json!(taf.raw));
        put(
            "taf_issued",
            serde_json::json!(taf.issued.map(|t| t.to_rfc3339())),
        );
        put(
            "taf_valid_from",
            serde_json::json!(taf.valid_from.map(|t| t.to_rfc3339())),
        );
        put(
            "taf_valid_to",
            serde_json::json!(taf.valid_to.map(|t| t.to_rfc3339())),
        );
    }

    put("raw", serde_json::json!(text(f[0])));

    // The ICAO code is the label. Pilots and controllers know aerodromes by
    // it, and "Bastia/Poretta Arpt, OC, FR" is a caption, not a label.
    Row::Observed(Box::new(
        Observation::new(
            source_id.clone(),
            EntityId::new(EntityKind::Station, key.clone()),
            observed_at,
            Quality::Live,
        )
        .with_position(Position {
            lon,
            lat,
            alt_m: None,
            datum: AltitudeDatum::Geoid,
        })
        .with_label(key)
        .with_attrs(serde_json::Value::Object(attrs)),
    ))
}

// --- TAF cache -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TafResponse {
    data: TafData,
}

#[derive(Debug, Deserialize)]
struct TafData {
    #[serde(rename = "TAF", default)]
    tafs: Vec<TafRecord>,
}

/// One `<TAF>`; the `<forecast>` children are not read. The raw text is what
/// a pilot reads and what a client shows; decoding change groups into
/// structured fields is a project of its own and not what this layer is for.
#[derive(Debug, Deserialize)]
struct TafRecord {
    raw_text: Option<String>,
    station_id: Option<String>,
    issue_time: Option<String>,
    valid_time_from: Option<String>,
    valid_time_to: Option<String>,
}

/// Decode `tafs.cache.xml` to a map by aerodrome. 2,971 forecasts for 2,971
/// distinct aerodromes when captured; if the file ever carried an amendment
/// beside the original, the later issue wins.
pub fn decode_tafs(text: &str) -> Result<HashMap<String, Taf>, SourceError> {
    let response: TafResponse =
        quick_xml::de::from_str(text).map_err(|e| SourceError::Decode(format!("TAF xml: {e}")))?;
    let mut by_station: HashMap<String, Taf> = HashMap::new();
    for record in response.data.tafs {
        let (Some(station), Some(raw)) = (record.station_id, record.raw_text) else {
            continue;
        };
        let stamp = |s: Option<String>| s.and_then(|s| s.trim().parse::<DateTime<Utc>>().ok());
        let taf = Taf {
            raw: raw.trim().to_string(),
            issued: stamp(record.issue_time),
            valid_from: stamp(record.valid_time_from),
            valid_to: stamp(record.valid_time_to),
        };
        let key = station.trim().to_ascii_uppercase();
        match by_station.get(&key) {
            Some(existing) if existing.issued > taf.issued => {}
            _ => {
                by_station.insert(key, taf);
            }
        }
    }
    Ok(by_station)
}

// --- station table ---------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StationRecord {
    #[serde(rename = "icaoId")]
    icao_id: Option<String>,
    #[serde(rename = "iataId")]
    iata_id: Option<String>,
    site: Option<String>,
    country: Option<String>,
}

/// Decode `stations.cache.json`: 9,875 sites, 8,841 with an ICAO id, every
/// one of the 5,128 reporting aerodromes among them. The rest are buoys,
/// WMO synoptic sites and the like, which have no METAR to join to.
pub fn decode_stations(text: &str) -> Result<HashMap<String, StationMeta>, SourceError> {
    let records: Vec<StationRecord> = serde_json::from_str(text)
        .map_err(|e| SourceError::Decode(format!("station json: {e}")))?;
    Ok(records
        .into_iter()
        .filter_map(|r| {
            let icao = r.icao_id?;
            let clean = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
            Some((
                icao.trim().to_ascii_uppercase(),
                StationMeta {
                    name: clean(r.site),
                    country: clean(r.country),
                    iata: clean(r.iata_id),
                },
            ))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn source() -> SourceId {
        SourceId::new("awc-metar")
    }

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid instant")
    }

    /// 2026-09-16T10:15Z, a few minutes after the rows below were captured.
    const NOW: i64 = 1_789_553_700;

    const HEADER_LINE: &str = "raw_text,station_id,observation_time,latitude,longitude,temp_c,dewpoint_c,wind_dir_degrees,wind_speed_kt,wind_gust_kt,visibility_statute_mi,altim_in_hg,sea_level_pressure_mb,corrected,auto,auto_station,maintenance_indicator_on,no_signal,lightning_sensor_off,freezing_rain_sensor_off,present_weather_sensor_off,wx_string,sky_cover,cloud_base_ft_agl,sky_cover,cloud_base_ft_agl,sky_cover,cloud_base_ft_agl,sky_cover,cloud_base_ft_agl,flight_category,three_hr_pressure_tendency_mb,maxT_c,minT_c,maxT24hr_c,minT24hr_c,precip_in,pcp3hr_in,pcp6hr_in,pcp24hr_in,snow_in,vert_vis_ft,metar_type,elevation_m";

    /// Verbatim rows from the live file, chosen for the awkward ones: a
    /// three-layer sky with a quoted raw text, a `10+` visibility with NOSIG,
    /// an unplaced military site, Tarbes with its empty longitude, a `null`
    /// flight category, an obscured sky with vertical visibility, and a
    /// SPECI with a gust and a `$`.
    const LIVE_ROWS: &str = "\
\"SPECI PAVD 161007Z AUTO 10006KT 2SM -RA BR SCT002 BKN022 OVC027 09/09 A2986 RMK AO2 VIS 1 1/2V2 1/2 P0001 TSNO $\",PAVD,2026-09-16T10:07:00.000Z,61.1330,-146.2510,9,9,100,6,,2,29.86,,,TRUE,TRUE,TRUE,,TRUE,,,-RA BR,SCT,200,BKN,2200,OVC,2700,,,IFR,,,,,,0.01,,,,,,SPECI,21
\"METAR EGLL 161010Z AUTO 24008KT 9999 SCT037 20/12 Q1017 NOSIG\",EGLL,2026-09-16T10:10:00.000Z,51.4780,-0.4610,20,12,240,8,,6+,30.03,1017,,TRUE,,,TRUE,,,,,SCT,3700,,,,,,,VFR,,,,,,,,,,,,METAR,25
\"METAR KQEQ 161000Z AUTO 00000KT 10SM CLR 25/12 A2998\",KQEQ,2026-09-16T10:00:00.000Z,-99.9900,-99.9900,25,12,0,0,,10,29.98,,,TRUE,,,,,,,,,,,,,,,,VFR,,,,,,,,,,,,METAR,9999
\"METAR LFBT 161000Z AUTO 00000KT CAVOK 21/15 Q1018\",LFBT,2026-09-16T10:00:00.000Z,43.1890,,21,15,0,0,,6+,30.06,1018,,TRUE,,,,,,,,,,,,,,,,VFR,,,,,,,,,,,,METAR,359
\"METAR SCIC 161000Z AUTO 13003KT //// R/////// // NCD 08/07 Q1024\",SCIC,2026-09-16T10:00:00.000Z,-35.0000,-71.2000,8,7,130,3,,,,1024,,TRUE,,,,,,,,,,,,,,,,null,,,,,,,,,,,,METAR,230
\"SPECI KAWO 161004Z AUTO 00000KT 3/4SM BR VV002 09/09 A3020 RMK AO2\",KAWO,2026-09-16T10:04:00.000Z,48.1600,-122.1600,9,9,0,0,,0.75,30.2,,,TRUE,TRUE,,,,,,BR,OVX,200,,,,,,,LIFR,,,,,,,,,,,2,SPECI,42
\"SPECI CYZR 161008Z AUTO 36010G17KT 9SM BKN009 OVC035 20/19 A3022 RMK SLP234 DENSITY ALT 1000FT $\",CYZR,2026-09-16T10:08:00.000Z,42.9950,-82.3070,20,19,360,10,17,9,30.22,1023.4,,TRUE,,TRUE,,,,,,BKN,900,OVC,3500,,,,,IFR,,,,,,,,,,,,SPECI,181
";

    fn file() -> String {
        format!("{HEADER_LINE}\n{LIVE_ROWS}")
    }

    fn egll_taf(valid_to: i64) -> HashMap<String, Taf> {
        HashMap::from([(
            "EGLL".to_string(),
            Taf {
                raw: "TAF EGLL 160459Z 1606/1712 24008KT 9999 SCT035".into(),
                issued: Some(at(1_789_534_740)),
                valid_from: Some(at(1_789_538_400)),
                valid_to: Some(at(valid_to)),
            },
        )])
    }

    fn by_key(observations: &[Observation]) -> HashMap<String, &Observation> {
        observations
            .iter()
            .map(|o| (o.entity.key.clone(), o))
            .collect()
    }

    #[test]
    fn the_live_rows_decode_and_the_unplaced_ones_are_counted_not_drawn() {
        let d = decode(
            &file(),
            &HashMap::new(),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .expect("the captured header is the expected one");
        assert_eq!(d.malformed, 0);
        // KQEQ at -99.99,-99.99 and Tarbes with an empty longitude.
        assert_eq!(
            d.unplaced, 2,
            "unplaced rows are skipped, not put at 99°S or on the meridian"
        );
        assert_eq!(d.observations.len(), 5);
        for o in &d.observations {
            assert_eq!(o.entity.kind, EntityKind::Station);
            let p = o.position.expect("every emitted station has a position");
            assert!((-90.0..=90.0).contains(&p.lat) && (-180.0..=180.0).contains(&p.lon));
        }
    }

    #[test]
    fn a_header_that_moved_fails_the_poll_rather_than_mislabelling_every_value() {
        // Columns are read by position because four are called `sky_cover`.
        // Drop one and everything after it lands under the wrong name, which
        // must be a loud error, not a layer of temperatures stored as winds.
        let shifted = file().replacen("wind_gust_kt,", "", 1);
        let err = decode(
            &shifted,
            &HashMap::new(),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .err()
        .expect("a changed header is a decode error");
        assert!(matches!(err, SourceError::Decode(_)), "{err}");
    }

    #[test]
    fn a_raw_report_with_commas_stays_whole() {
        // Four raw texts in the live file contain commas. Split-on-comma would
        // shift the row and lose it to the field count.
        let d = decode(
            &file(),
            &HashMap::new(),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .unwrap();
        let obs = by_key(&d.observations);
        let raw = obs["PAVD"].attrs["raw"].as_str().unwrap();
        assert!(raw.contains("VIS 1 1/2V2 1/2"), "{raw}");
        assert_eq!(obs["PAVD"].attrs["report_type"], serde_json::json!("SPECI"));
    }

    #[test]
    fn cloud_layers_come_out_in_order_with_their_bases() {
        let d = decode(
            &file(),
            &HashMap::new(),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .unwrap();
        let obs = by_key(&d.observations);
        assert_eq!(
            obs["PAVD"].attrs["clouds"],
            serde_json::json!([
                {"cover": "SCT", "base_ft": 200.0},
                {"cover": "BKN", "base_ft": 2200.0},
                {"cover": "OVC", "base_ft": 2700.0},
            ])
        );
        // Obscured sky: the OVX layer carries the vertical visibility in feet,
        // which is why the mis-scaled `vert_vis_ft` column is not read.
        assert_eq!(
            obs["KAWO"].attrs["clouds"],
            serde_json::json!([{"cover": "OVX", "base_ft": 200.0}])
        );
        assert!(obs["KAWO"].attrs.get("vert_vis_ft").is_none());
        // A clear sky has no layers, and no empty array either.
        assert!(obs["SCIC"].attrs.get("clouds").is_none());
    }

    #[test]
    fn a_plus_visibility_is_a_comparable_number_and_a_flag() {
        let d = decode(
            &file(),
            &HashMap::new(),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .unwrap();
        let obs = by_key(&d.observations);
        assert_eq!(obs["EGLL"].attrs["visibility_mi"], serde_json::json!(6.0));
        assert_eq!(
            obs["EGLL"].attrs["visibility_or_more"],
            serde_json::json!(true)
        );
        assert_eq!(obs["KAWO"].attrs["visibility_mi"], serde_json::json!(0.75));
        assert!(obs["KAWO"].attrs.get("visibility_or_more").is_none());
    }

    #[test]
    fn the_literal_string_null_is_not_a_flight_category() {
        // 368 rows in the live file say `null`, spelled out, where the report
        // was too damaged to categorise. Storing the word would give a client
        // a fifth category.
        let d = decode(
            &file(),
            &HashMap::new(),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .unwrap();
        let obs = by_key(&d.observations);
        assert!(obs["SCIC"].attrs.get("flight_category").is_none());
        assert_eq!(
            obs["KAWO"].attrs["flight_category"],
            serde_json::json!("LIFR")
        );
    }

    #[test]
    fn flags_are_present_only_when_set_and_nosig_means_no_significant_change() {
        let d = decode(
            &file(),
            &HashMap::new(),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .unwrap();
        let obs = by_key(&d.observations);
        assert_eq!(obs["EGLL"].attrs["nosig"], serde_json::json!(true));
        assert_eq!(obs["EGLL"].attrs["auto"], serde_json::json!(true));
        assert!(obs["EGLL"].attrs.get("maintenance").is_none());
        assert_eq!(obs["CYZR"].attrs["maintenance"], serde_json::json!(true));
        assert_eq!(obs["CYZR"].attrs["gust_kt"], serde_json::json!(17.0));
        assert_eq!(
            obs["PAVD"].attrs["sensors_off"],
            serde_json::json!(["lightning"])
        );
        assert!(obs["CYZR"].attrs.get("sensors_off").is_none());
    }

    #[test]
    fn the_broken_temperature_extremes_are_not_published() {
        let d = decode(
            &file(),
            &HashMap::new(),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .unwrap();
        for o in &d.observations {
            for key in ["maxT_c", "minT_c", "maxT24hr_c", "minT24hr_c", "max_temp_c"] {
                assert!(o.attrs.get(key).is_none(), "{} carries {key}", o.entity.key);
            }
        }
    }

    #[test]
    fn a_current_taf_is_attached_and_an_expired_one_is_not() {
        let d = decode(
            &file(),
            &egll_taf(1_789_646_400),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .unwrap();
        let obs = by_key(&d.observations);
        assert!(
            obs["EGLL"].attrs["taf"]
                .as_str()
                .unwrap()
                .starts_with("TAF EGLL")
        );
        assert!(obs["EGLL"].attrs.get("taf_valid_to").is_some());
        assert!(
            obs["PAVD"].attrs.get("taf").is_none(),
            "no forecast, no attribute"
        );

        // The same forecast, once its validity has passed, is not a forecast.
        let d = decode(
            &file(),
            &egll_taf(NOW - 60),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .unwrap();
        let obs = by_key(&d.observations);
        assert!(obs["EGLL"].attrs.get("taf").is_none());
    }

    #[test]
    fn the_station_table_adds_a_name_but_the_label_stays_the_icao_code() {
        let stations = HashMap::from([(
            "EGLL".to_string(),
            StationMeta {
                name: Some("London/Heathrow Arpt".into()),
                country: Some("GB".into()),
                iata: Some("LHR".into()),
            },
        )]);
        let d = decode(&file(), &HashMap::new(), &stations, &source(), at(NOW)).unwrap();
        let obs = by_key(&d.observations);
        assert_eq!(
            obs["EGLL"].attrs["name"],
            serde_json::json!("London/Heathrow Arpt")
        );
        assert_eq!(obs["EGLL"].attrs["iata"], serde_json::json!("LHR"));
        assert_eq!(obs["EGLL"].label.as_deref(), Some("EGLL"));
        assert_eq!(
            obs["PAVD"].label.as_deref(),
            Some("PAVD"),
            "unnamed is still labelled"
        );
    }

    #[test]
    fn the_observation_time_is_the_report_time_in_utc() {
        let d = decode(
            &file(),
            &HashMap::new(),
            &HashMap::new(),
            &source(),
            at(NOW),
        )
        .unwrap();
        let obs = by_key(&d.observations);
        assert_eq!(obs["EGLL"].observed_at, at(1_789_553_400), "161010Z");
    }

    #[test]
    fn the_taf_cache_decodes_by_station_with_its_validity() {
        // A trimmed copy of the live XML: the CDATA raw text, the forecast
        // children that are not read, and the response envelope.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?><response version="2.0"><request_index>1</request_index><data_source name="taf"/><errors/><warnings/><data num_results="2"><TAF><raw_text><![CDATA[TAF MHTG 161010Z 1612/1712 VRB04KT FEW012 SCT028]]></raw_text><station_id>MHTG</station_id><issue_time>2026-09-16T10:10:00.000Z</issue_time><bulletin_time>2026-09-16T10:10:00.000Z</bulletin_time><valid_time_from>2026-09-16T12:00:00.000Z</valid_time_from><valid_time_to>2026-09-17T12:00:00.000Z</valid_time_to><latitude>14.0600</latitude><longitude>-87.2160</longitude><elevation_m>1001.0000</elevation_m><forecast><fcst_time_from>2026-09-16T12:00:00.000Z</fcst_time_from><fcst_time_to>2026-09-16T17:00:00.000Z</fcst_time_to><wind_dir_degrees>VRB</wind_dir_degrees><wind_speed_kt>4</wind_speed_kt><sky_condition sky_cover="FEW" cloud_base_ft_agl="1200"/></forecast></TAF><TAF><raw_text><![CDATA[TAF AMD EGLL 160930Z 1609/1712 24008KT 9999 SCT035]]></raw_text><station_id>EGLL</station_id><issue_time>2026-09-16T09:30:00.000Z</issue_time><valid_time_from>2026-09-16T09:00:00.000Z</valid_time_from><valid_time_to>2026-09-17T12:00:00.000Z</valid_time_to><remarks>AMD</remarks></TAF></data></response>"#;
        let tafs = decode_tafs(xml).expect("the live shape decodes");
        assert_eq!(tafs.len(), 2);
        let egll = &tafs["EGLL"];
        assert!(egll.raw.starts_with("TAF AMD EGLL"));
        assert_eq!(egll.valid_to, Some(at(1_789_646_400)));
        assert_eq!(tafs["MHTG"].issued, Some(at(1_789_553_400)));
    }

    #[test]
    fn the_station_table_keeps_only_sites_with_an_icao_code() {
        let json = r#"[
            {"id":"32012","icaoId":null,"iataId":null,"site":"Woods Hole Stratus Wave Station","country":null},
            {"id":"EGLL","icaoId":"EGLL","iataId":"LHR","site":"London/Heathrow Arpt","country":"GB"},
            {"id":"KQEQ","icaoId":"KQEQ","iataId":"","site":"MIL","country":null}
        ]"#;
        let stations = decode_stations(json).unwrap();
        assert_eq!(
            stations.len(),
            2,
            "a buoy with no ICAO id has no METAR to join"
        );
        assert_eq!(stations["EGLL"].iata.as_deref(), Some("LHR"));
        assert_eq!(stations["KQEQ"].iata, None, "an empty id is no id");
        assert_eq!(stations["KQEQ"].country, None);
    }

    #[test]
    fn gzip_is_inflated_and_plain_text_is_left_alone() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(b"hello,world").unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(inflate(&gz).unwrap(), "hello,world");
        assert_eq!(inflate(b"hello,world").unwrap(), "hello,world");
    }
}
