//! NOAA National Data Buoy Center: the latest observation from every station
//! that is reporting, in one fixed-column text file.
//!
//! `latest_obs.txt` is the whole network in one request — 876 stations and
//! 22 KB when this was written — and it is *not* only buoys. Alongside the
//! moored and drifting buoys are C-MAN coastal stations, NOS water level
//! stations, NERRS estuary sites, Gulf of Mexico oil platforms reporting under
//! `K` call signs, Canadian and Korean partner stations, and the Stratus
//! ocean reference mooring at 22°S off Chile. The layer is the ocean's
//! surface weather, and the station is whatever is measuring it.
//!
//! ## The file is not fixed-width
//!
//! The header is laid out in columns, and the research note called the file
//! fixed-width. It is not, reliably: latitude carries three decimals on 868
//! rows and two on eight, a temperature of exactly thirty degrees prints as
//! `30` rather than `30.0`, and the station id runs from four characters to
//! seven (`4403587`, Whitefish Bay). Every row does have exactly 22
//! whitespace-separated fields, and that is the contract this decoder holds
//! to. A row with any other count is skipped and counted, not guessed at.
//!
//! ## `MM` is missing, and most of the file is `MM`
//!
//! Every measurement column uses `MM` for "not measured". Across the live file
//! it is the majority value in eleven of the fourteen columns: 840 of 876
//! stations report no tide, 832 no visibility, 789 no average wave period. A
//! station reporting only water temperature is a normal station, so every
//! measurement is optional and a station is kept if it reports *anything*.
//! One that reports nothing at all is not an observation and is skipped —
//! the same rule as a river gauge with no reading.
//!
//! ## Two files, one static
//!
//! The observation file carries no names or types. `station_table.txt` does —
//! owner, station type, a human name — for 1,938 stations, every one of the
//! 876 reporting ids among them once case is folded: the table writes `katp`
//! and `0y2w3`, the observations write `KATP` and `0Y2W3`. The table changes
//! when a station is commissioned, so it is fetched on the first poll and then
//! once a day, and a poll that cannot fetch it still emits every buoy without
//! its name rather than nothing at all.
//!
//! Units are kept as NDBC publishes them and named in the attribute keys:
//! wind in m/s, waves in metres, pressure in hPa, temperatures in °C — and
//! visibility in nautical miles and tide in feet, because converting a
//! one-decimal reading into a different unit prints precision the buoy never
//! measured.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{TimeZone, Utc};
use std::collections::HashMap;
use tokio::sync::RwLock;

const LATEST_OBS_URL: &str = "https://www.ndbc.noaa.gov/data/latest_obs/latest_obs.txt";
const STATION_TABLE_URL: &str = "https://www.ndbc.noaa.gov/data/stations/station_table.txt";

/// Stations report hourly, most at :00 or :48–:50, and NDBC rebuilds the
/// latest-observation file a few minutes after each cycle. Ten minutes catches
/// a new hour's readings within the hour without asking the same question
/// several times per answer.
const CADENCE_SECS: u64 = 600;

/// How long a fetched station table is trusted before it is refreshed.
const STATION_TABLE_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// The number of whitespace-separated fields on every data row of
/// `latest_obs.txt`: id, position, five time parts, fourteen measurements.
const FIELDS: usize = 22;

pub struct NdbcBuoys {
    descriptor: SourceDescriptor,
    http: HttpClient,
    stations: RwLock<Option<StationTable>>,
}

struct StationTable {
    fetched: std::time::Instant,
    by_id: HashMap<String, StationMeta>,
}

/// What the station table knows about a station that the observation file
/// does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationMeta {
    pub name: Option<String>,
    pub owner: Option<String>,
    pub station_type: Option<String>,
    pub note: Option<String>,
}

impl NdbcBuoys {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("ndbc-buoys"),
                layer_id: LayerId::new("buoys"),
                display_name: "Marine buoys and coastal stations (NOAA NDBC)".into(),
                kind: EntityKind::Station,
                cadence: Cadence::every(CADENCE_SECS),
                // One request returns every station on Earth that NDBC relays,
                // from the Bering Sea to the Stratus mooring at 22°S. The
                // density is American, the extent is not.
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "NOAA National Data Buoy Center".into(),
                    url: "https://www.ndbc.noaa.gov/".into(),
                    license: "Public domain (US Government)".into(),
                    notice: None,
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            stations: RwLock::new(None),
        }
    }

    /// The station table, refreshed once a day, or an empty one if it cannot
    /// be fetched. A buoy without its name is still a buoy; a poll that emits
    /// nothing because a static reference file was slow is a broken layer.
    async fn station_table(&self) -> HashMap<String, StationMeta> {
        if let Some(table) = self.stations.read().await.as_ref()
            && table.fetched.elapsed() < STATION_TABLE_TTL
        {
            return table.by_id.clone();
        }
        match self.http.get_bytes(STATION_TABLE_URL).await {
            Ok(bytes) => {
                let by_id = decode_station_table(&String::from_utf8_lossy(&bytes));
                tracing::debug!(
                    source = %self.descriptor.id,
                    stations = by_id.len(),
                    "refreshed the NDBC station table"
                );
                *self.stations.write().await = Some(StationTable {
                    fetched: std::time::Instant::now(),
                    by_id: by_id.clone(),
                });
                by_id
            }
            Err(err) => {
                tracing::warn!(
                    source = %self.descriptor.id,
                    %err,
                    "could not fetch the NDBC station table; stations keep their ids as labels"
                );
                // A stale table beats none. It is only refreshed, never
                // discarded, on a failed fetch.
                self.stations
                    .read()
                    .await
                    .as_ref()
                    .map(|t| t.by_id.clone())
                    .unwrap_or_default()
            }
        }
    }
}

#[async_trait::async_trait]
impl Source for NdbcBuoys {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let bytes = self.http.get_bytes(LATEST_OBS_URL).await?;
        let text = String::from_utf8_lossy(&bytes);
        let stations = self.station_table().await;
        let decoded = decode(&text, &stations, &self.descriptor.id);
        if decoded.malformed > 0 {
            tracing::warn!(
                source = %self.descriptor.id,
                malformed = decoded.malformed,
                emitted = decoded.observations.len(),
                "NDBC rows did not have {FIELDS} fields and were skipped"
            );
        }
        Ok(decoded.observations)
    }
}

/// What a decode produced, and what it refused. The count of refused rows is
/// what turns a format change from a silently thinner layer into a warning.
pub struct Decoded {
    pub observations: Vec<Observation>,
    /// Rows that were not a comment and did not have [`FIELDS`] fields.
    pub malformed: usize,
    /// Rows that were well-formed but reported nothing at all.
    pub silent: usize,
}

/// Decode `latest_obs.txt` against the station table.
pub fn decode(text: &str, stations: &HashMap<String, StationMeta>, source_id: &SourceId) -> Decoded {
    let mut decoded = Decoded {
        observations: Vec::new(),
        malformed: 0,
        silent: 0,
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        if fields.len() != FIELDS {
            decoded.malformed += 1;
            continue;
        }
        match decode_row(&fields, stations, source_id) {
            Row::Observed(o) => decoded.observations.push(*o),
            Row::Silent => decoded.silent += 1,
            Row::Malformed => decoded.malformed += 1,
        }
    }
    decoded
}

enum Row {
    Observed(Box<Observation>),
    Silent,
    Malformed,
}

/// A measurement column: a number, or `MM`.
fn measurement(raw: &str) -> Option<f64> {
    if raw == "MM" {
        return None;
    }
    raw.parse().ok()
}

fn decode_row(fields: &[&str], stations: &HashMap<String, StationMeta>, source_id: &SourceId) -> Row {
    // Ids are alphanumeric and mixed case across the two files; the
    // observation file's upper case is the canonical form.
    let key = fields[0].to_ascii_uppercase();

    let (Ok(lat), Ok(lon)) = (fields[1].parse::<f64>(), fields[2].parse::<f64>()) else {
        return Row::Malformed;
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Row::Malformed;
    }

    // NDBC states its times as UTC. Any part failing to parse is a malformed
    // row rather than a station dated to the epoch.
    let time = fields[3..8]
        .iter()
        .map(|f| f.parse::<u32>())
        .collect::<Result<Vec<_>, _>>();
    let Ok(time) = time else {
        return Row::Malformed;
    };
    let Some(observed_at) = Utc
        .with_ymd_and_hms(time[0] as i32, time[1], time[2], time[3], time[4], 0)
        .single()
    else {
        return Row::Malformed;
    };

    // The fourteen measurement columns, in the file's order.
    const COLUMNS: [&str; 14] = [
        "wind_dir_deg",
        "wind_speed_ms",
        "gust_ms",
        "wave_height_m",
        "dominant_period_s",
        "average_period_s",
        "wave_dir_deg",
        "pressure_hpa",
        "pressure_tendency_hpa",
        "air_temp_c",
        "water_temp_c",
        "dewpoint_c",
        "visibility_nmi",
        "tide_ft",
    ];
    let mut attrs = serde_json::Map::new();
    let mut reported = 0usize;
    for (name, raw) in COLUMNS.iter().zip(&fields[8..]) {
        if let Some(v) = measurement(raw) {
            attrs.insert((*name).into(), serde_json::json!(v));
            reported += 1;
        }
    }
    // A station reporting only one thing is ordinary; one reporting nothing
    // has not observed anything and is left for the station horizon to retire.
    if reported == 0 {
        return Row::Silent;
    }

    let meta = stations.get(&key);
    attrs.insert("station".into(), serde_json::json!(key));
    if let Some(meta) = meta {
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                attrs.insert(k.into(), serde_json::json!(v));
            }
        };
        put("name", &meta.name);
        put("owner", &meta.owner);
        put("station_type", &meta.station_type);
        put("note", &meta.note);
    }

    let label = meta
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| key.clone());

    Row::Observed(Box::new(
        Observation::new(
            source_id.clone(),
            EntityId::new(EntityKind::Station, key),
            observed_at,
            Quality::Live,
        )
        .with_position(Position {
            lon,
            lat,
            alt_m: None,
            datum: AltitudeDatum::Geoid,
        })
        .with_label(label)
        .with_attrs(serde_json::Value::Object(attrs)),
    ))
}

/// Decode `station_table.txt`: pipe-separated, one station per row, ids in
/// whatever case NDBC felt like that day.
///
/// `STATION_ID | OWNER | TTYPE | HULL | NAME | PAYLOAD | LOCATION | TIMEZONE |
/// FORECAST | NOTE`. Location is not read — the observation file carries the
/// position the reading was taken at, which for a drifting buoy is not the
/// one in the table.
pub fn decode_station_table(text: &str) -> HashMap<String, StationMeta> {
    let mut by_id = HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 10 || cols[0].is_empty() {
            continue;
        }
        let field = |i: usize| -> Option<String> {
            let v = cols[i];
            (!v.is_empty()).then(|| plain_text(v))
        };
        by_id.insert(
            cols[0].to_ascii_uppercase(),
            StationMeta {
                name: field(4),
                owner: field(1),
                station_type: field(2),
                note: field(9),
            },
        );
    }
    by_id
}

/// The table is not quite plain text. The location column is entity-escaped
/// (`&#176;` for every degree sign), and 341 of the 596 `NOTE` values are HTML
/// fragments — `<a href>` to a sister station, `<br>` and `<p>` between
/// sentences. A note is shown to a person in a card, so tags are flattened to
/// the text they wrap, with a space where a line or paragraph break was, and
/// the few entities that actually occur are decoded. Names are raw and carry
/// literal `&` (`McMoRan Oil & Gas`), which this leaves alone.
fn plain_text(s: &str) -> String {
    if !s.contains('<') && !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            // An unclosed `<` is text, not markup.
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let tag = rest[open + 1..open + close].trim_start_matches('/');
        let tag = tag.split(|c: char| c.is_ascii_whitespace()).next().unwrap_or("");
        if matches!(tag.to_ascii_lowercase().as_str(), "br" | "p" | "li" | "div") {
            out.push(' ');
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    let decoded = out
        .replace("&amp;", "&")
        .replace("&#176;", "°")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    fn source() -> SourceId {
        SourceId::new("ndbc-buoys")
    }

    /// Verbatim rows from the live file, chosen for the awkward ones: a seven
    /// character id, a station reporting only wind, one at 22°S, an integer
    /// temperature, a signed pressure tendency.
    const LIVE_ROWS: &str = "\
#STN       LAT      LON  YYYY MM DD hh mm WDIR WSPD   GST WVHT  DPD APD MWD   PRES  PTDY  ATMP  WTMP  DEWP  VIS   TIDE
#text      deg      deg   yr mo day hr mn degT  m/s   m/s   m   sec sec degT   hPa   hPa  degC  degC  degC  nmi     ft
46120    47.761 -122.397 2026 09 15 13 50  10   1.0    MM   MM  MM   MM  MM     MM    MM  14.1    MM  12.0   MM     MM
4403587  46.545  -84.769 2026 09 15 15 20  MM   7.5   9.2   MM  MM   MM  MM     MM    MM    MM    MM    MM   MM     MM
32ST0   -22.000  -85.000 2026 09 15 15 30 110   5.0    MM   MM  MM   MM  MM 1017.6    MM  18.4  19.2  12.0   MM     MM
KATP     27.195  -90.027 2026 09 15 16 15  50   4.1    MM   MM  MM   MM  MM     MM    MM    30    MM    25  8.7     MM
46069    33.657 -120.227 2026 09 15 16 00 330   5.0   7.0   MM  MM   MM  MM 1014.1  +1.8  18.2  20.2  17.3   MM     MM
51214   -14.296 -170.875 2026 09 15 14 00  MM    MM    MM  2.3  15  7.8 228     MM    MM  24.6  27.4    MM   MM     MM
";

    const LIVE_TABLE: &str = "\
# STATION_ID | OWNER | TTYPE | HULL | NAME | PAYLOAD | LOCATION | TIMEZONE | FORECAST | NOTE
#
4403587|BMI|Buoy||Whitefish Bay||46.545 N 84.769 W (46&#176;32'43\" N 84&#176;46'9\" W)|E|FZUS53.KAPX |
46120|WA|Buoy||Pt Wells, WA (U of Wash)||47.761 N 122.397 W (47&#176;45'40\" N 122&#176;23'50\" W)|P|FZUS56.KSEW |
katp|FA|Oil Platform||Green Canyon 787 / Atlantis (BP)||27.195 N 90.027 W (27&#176;11'42\" N 90&#176;1'37\" W)|C| |Data from this station are not quality controlled by NDBC.
22101|KO|Buoy||||37.230 N 126.020 E (37&#176;13'48\" N 126&#176;1'12\" E)|?| |
";

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("a test instant")
            .with_timezone(&Utc)
    }

    #[test]
    fn every_live_row_decodes_and_none_is_malformed() {
        let d = decode(LIVE_ROWS, &HashMap::new(), &source());
        assert_eq!(d.observations.len(), 6);
        assert_eq!(d.malformed, 0);
        assert_eq!(d.silent, 0);
        for o in &d.observations {
            assert_eq!(o.entity.kind, EntityKind::Station);
            assert!(o.position.is_some());
        }
    }

    #[test]
    fn the_stamp_is_read_as_utc() {
        let d = decode(LIVE_ROWS, &HashMap::new(), &source());
        let o = d.observations.iter().find(|o| o.entity.key == "46120").expect("46120");
        assert_eq!(o.observed_at, at("2026-09-15T13:50:00Z"));
    }

    #[test]
    fn mm_is_absent_and_the_present_columns_are_named_with_their_units() {
        let d = decode(LIVE_ROWS, &HashMap::new(), &source());
        let o = d.observations.iter().find(|o| o.entity.key == "46069").expect("46069");
        assert_eq!(o.attrs["wind_dir_deg"], serde_json::json!(330.0));
        assert_eq!(o.attrs["gust_ms"], serde_json::json!(7.0));
        assert_eq!(o.attrs["pressure_hpa"], serde_json::json!(1014.1));
        assert_eq!(
            o.attrs["pressure_tendency_hpa"],
            serde_json::json!(1.8),
            "the leading + is a sign, not a format error"
        );
        assert!(o.attrs.get("wave_height_m").is_none(), "MM must not become a value");
        assert!(o.attrs.get("tide_ft").is_none());

        // An integer where every other station prints a decimal.
        let katp = d.observations.iter().find(|o| o.entity.key == "KATP").expect("KATP");
        assert_eq!(katp.attrs["air_temp_c"], serde_json::json!(30.0));
        assert_eq!(katp.attrs["visibility_nmi"], serde_json::json!(8.7));
    }

    #[test]
    fn a_station_reporting_only_wind_is_still_a_station() {
        // 4403587 reported wind speed and gust and nothing else. That is most
        // of the Great Lakes on a calm day, not a broken row.
        let d = decode(LIVE_ROWS, &HashMap::new(), &source());
        let o = d.observations.iter().find(|o| o.entity.key == "4403587").expect("a seven-character id");
        assert_eq!(o.attrs["wind_speed_ms"], serde_json::json!(7.5));
        assert_eq!(o.attrs.as_object().expect("attrs").len(), 3, "wind, gust, station");
    }

    #[test]
    fn a_station_reporting_nothing_is_not_an_observation() {
        let text = "\
#STN LAT LON YYYY MM DD hh mm WDIR WSPD GST WVHT DPD APD MWD PRES PTDY ATMP WTMP DEWP VIS TIDE
QUIET  50.000  -1.000 2026 09 15 16 00  MM    MM    MM   MM  MM   MM  MM     MM    MM    MM    MM    MM   MM     MM
";
        let d = decode(text, &HashMap::new(), &source());
        assert!(d.observations.is_empty());
        assert_eq!(d.silent, 1);
        assert_eq!(d.malformed, 0);
    }

    #[test]
    fn a_row_with_the_wrong_number_of_fields_is_counted_not_guessed_at() {
        // Nineteen fields: a format change, or a truncated download. Either
        // way the columns no longer mean what the decoder thinks they mean.
        let text = "\
46069    33.657 -120.227 2026 09 15 16 00 330   5.0   7.0   MM  MM   MM  MM 1014.1  +1.8  18.2
46120    47.761 -122.397 2026 09 15 13 50  10   1.0    MM   MM  MM   MM  MM     MM    MM  14.1    MM  12.0   MM     MM
";
        let d = decode(text, &HashMap::new(), &source());
        assert_eq!(d.observations.len(), 1);
        assert_eq!(d.malformed, 1);
    }

    #[test]
    fn a_row_with_an_unparseable_position_or_time_is_malformed_not_placed_at_the_epoch() {
        let text = "\
BADPOS  MM      -1.000 2026 09 15 16 00  10   1.0    MM   MM  MM   MM  MM     MM    MM  14.1    MM  12.0   MM     MM
BADTIM  50.000  -1.000 2026 09 xx 16 00  10   1.0    MM   MM  MM   MM  MM     MM    MM  14.1    MM  12.0   MM     MM
BADDAY  50.000  -1.000 2026 09 31 16 00  10   1.0    MM   MM  MM   MM  MM     MM    MM  14.1    MM  12.0   MM     MM
";
        let d = decode(text, &HashMap::new(), &source());
        assert!(d.observations.is_empty());
        assert_eq!(d.malformed, 3);
    }

    #[test]
    fn the_station_table_joins_across_case_and_supplies_the_label() {
        let table = decode_station_table(LIVE_TABLE);
        assert_eq!(table.len(), 4);
        let d = decode(LIVE_ROWS, &table, &source());

        // The table says `katp`, the observations say `KATP`.
        let katp = d.observations.iter().find(|o| o.entity.key == "KATP").expect("KATP");
        assert_eq!(katp.label.as_deref(), Some("Green Canyon 787 / Atlantis (BP)"));
        assert_eq!(katp.attrs["station_type"], serde_json::json!("Oil Platform"));
        assert_eq!(katp.attrs["owner"], serde_json::json!("FA"));
        assert_eq!(
            katp.attrs["note"],
            serde_json::json!("Data from this station are not quality controlled by NDBC.")
        );

        let wells = d.observations.iter().find(|o| o.entity.key == "46120").expect("46120");
        assert_eq!(wells.label.as_deref(), Some("Pt Wells, WA (U of Wash)"));

        // Not in the table: the id is the label, and nothing is invented.
        let stratus = d.observations.iter().find(|o| o.entity.key == "32ST0").expect("32ST0");
        assert_eq!(stratus.label.as_deref(), Some("32ST0"));
        assert!(stratus.attrs.get("name").is_none());
    }

    #[test]
    fn a_station_in_the_table_with_no_name_falls_back_to_its_id() {
        // 22101 is a Korean partner buoy with an empty NAME column.
        let table = decode_station_table(LIVE_TABLE);
        let meta = table.get("22101").expect("22101");
        assert_eq!(meta.name, None);
        assert_eq!(meta.owner.as_deref(), Some("KO"));
    }

    #[test]
    fn html_in_the_table_is_flattened_to_text() {
        // 32ST0's real note: a link to the sister station that carries its
        // wave data. The link is markup a card cannot show; the id is not.
        let table = decode_station_table(
            "32st0|WH|Ocean Reference Station||Stratus|||| |Wave data from this station is \
             available at <a title=\"32012\" href=\"https://www.ndbc.noaa.gov/station_page.php?\
             station=32012\">32012</a>.\n\
             x1|O|Buoy||Smith &amp; Jones|||| |First.<br>Second.<p>Third &#176;N</p>\n\
             x2|O|Buoy||McMoRan Oil & Gas|||| |a < b\n",
        );
        assert_eq!(
            table["32ST0"].note.as_deref(),
            Some("Wave data from this station is available at 32012.")
        );
        assert_eq!(table["X1"].name.as_deref(), Some("Smith & Jones"));
        assert_eq!(table["X1"].note.as_deref(), Some("First. Second. Third °N"));
        // A raw ampersand and a stray less-than are text, and stay text.
        assert_eq!(table["X2"].name.as_deref(), Some("McMoRan Oil & Gas"));
        assert_eq!(table["X2"].note.as_deref(), Some("a < b"));
    }

    #[test]
    fn the_key_is_the_upper_cased_id_whatever_the_file_wrote() {
        let text = "\
katp     27.195  -90.027 2026 09 15 16 15  50   4.1    MM   MM  MM   MM  MM     MM    MM    30    MM    25  8.7     MM
";
        let d = decode(text, &HashMap::new(), &source());
        assert_eq!(d.observations[0].entity.key, "KATP");
    }
}
