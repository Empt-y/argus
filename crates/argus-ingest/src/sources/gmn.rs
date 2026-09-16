//! Meteors: every trajectory the Global Meteor Network solved, from its
//! daily summary files.
//!
//! The GMN is a few thousand video cameras run by volunteers on every
//! continent, each watching the night sky, and a meteor seen by two or more
//! of them can be triangulated: where it began to glow, where it went out,
//! how fast it came in, and from those the orbit it was on before the Earth
//! got in the way. 4,031 of them on one September night. Each is an event
//! with a real line on the ground — ten kilometres long at the median, a
//! hundred kilometres up — and the begin and end heights are carried as
//! attributes because the store's geometry is two-dimensional.
//!
//! ## Files, not an API
//!
//! `traj_summary_data/daily/` holds one file per day of solar longitude,
//! named for the date and the longitude range, plus two aliases: `yesterday`
//! for the last complete day, and `latest_daily` for the day still being
//! built — eight rows at six in the morning, thousands by the next. A poll
//! fetches both aliases, which between them cover everything new, and the
//! store's dedupe on `(kind, key, observed_at, source)` makes the overlap
//! free. The first poll after a start also reads the directory index and
//! fetches every dated file inside the event horizon, so a fresh deployment
//! shows a week of meteors rather than a day.
//!
//! The format is a semicolon-separated table with a four-line commented
//! header, 86 columns, 43 of them `+/-` uncertainties. Columns are found by
//! header name with the sigma that follows each value skipped, so a column
//! added upstream moves nothing. Every value in a day's file was numeric
//! except the shower code, which is `...` for a sporadic — 3,463 of 4,031.
//!
//! Lines end in `\n\r` — newline *then* carriage return, the wrong way
//! round — so every line after the first begins with a `\r` that a terminal
//! hides and `str::lines` leaves in place. Every line is trimmed before it
//! is looked at; without that, the header is never found and a whole file
//! decodes to nothing.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{NaiveDate, NaiveDateTime, Utc};
use geo_types::{Coord, Geometry, LineString};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

const DAILY_DIR: &str = "https://globalmeteornetwork.org/data/traj_summary_data/daily/";

/// The day being built is regenerated through the morning as stations
/// upload; an hour sees it grow without re-reading four megabytes for the
/// same eight rows.
const CADENCE_SECS: u64 = 3600;

pub struct GmnMeteors {
    descriptor: SourceDescriptor,
    http: HttpClient,
    /// Whether the dated files inside the horizon have been fetched once.
    backfilled: AtomicBool,
}

impl GmnMeteors {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("gmn-meteors"),
                layer_id: LayerId::new("meteors"),
                display_name: "Meteors (Global Meteor Network)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "Global Meteor Network".into(),
                    url: "https://globalmeteornetwork.org/".into(),
                    license: "CC BY 4.0".into(),
                    notice: Some("Meteor trajectories from the Global Meteor Network".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            backfilled: AtomicBool::new(false),
        }
    }

    async fn fetch(&self, name: &str) -> Result<String, SourceError> {
        let bytes = self.http.get_bytes(&format!("{DAILY_DIR}{name}")).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[async_trait::async_trait]
impl Source for GmnMeteors {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let now = Utc::now();
        let mut files: Vec<String> = vec![
            "traj_summary_latest_daily.txt".into(),
            "traj_summary_yesterday.txt".into(),
        ];
        // Once per process: the dated files back to the event horizon, so a
        // fresh start shows the week and not the day. Read from the index
        // rather than computed, because the file names carry a solar
        // longitude range that is not worth reproducing.
        if !self.backfilled.load(Ordering::Relaxed) {
            match self.fetch("").await {
                Ok(index) => {
                    let horizon = EntityKind::Event
                        .live_horizon()
                        .expect("events have a horizon");
                    let oldest = (now - horizon).date_naive();
                    // The `yesterday` alias is the last *complete* day, and a
                    // day of solar longitude runs from four in the morning to
                    // four the next, so at six on the 16th it was the file
                    // dated the 14th. The backfill stops a day before that.
                    let newest = (now - chrono::Duration::days(3)).date_naive();
                    files.extend(dated_files(&index, oldest, newest));
                    self.backfilled.store(true, Ordering::Relaxed);
                }
                Err(err) => tracing::warn!(
                    source = %self.descriptor.id,
                    %err,
                    "could not read the daily index; showing today and yesterday only"
                ),
            }
        }

        let mut all = Vec::new();
        let mut last_error = None;
        for name in &files {
            match self.fetch(name).await {
                Ok(text) => match decode(&text, &self.descriptor.id) {
                    Ok(mut d) => {
                        if d.malformed > 0 {
                            tracing::debug!(source = %self.descriptor.id, file = %name, malformed = d.malformed, "rows skipped");
                        }
                        all.append(&mut d.observations);
                    }
                    Err(err) => {
                        tracing::warn!(source = %self.descriptor.id, file = %name, %err, "file did not decode");
                        last_error = Some(err);
                    }
                },
                Err(err) => {
                    tracing::warn!(source = %self.descriptor.id, file = %name, %err, "file did not fetch");
                    last_error = Some(err);
                }
            }
        }
        if all.is_empty()
            && let Some(err) = last_error
        {
            return Err(err);
        }
        Ok(all)
    }
}

/// The dated files in the directory listing whose date falls in
/// `oldest..=newest`, as file names.
fn dated_files(index_html: &str, oldest: NaiveDate, newest: NaiveDate) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in index_html.split("href=\"").skip(1) {
        let Some(end) = chunk.find('"') else { continue };
        let name = &chunk[..end];
        let Some(rest) = name.strip_prefix("traj_summary_") else {
            continue;
        };
        if rest.len() < 8 || !name.ends_with(".txt") {
            continue;
        }
        let Ok(date) = NaiveDate::parse_from_str(&rest[..8], "%Y%m%d") else {
            continue;
        };
        if (oldest..=newest).contains(&date) {
            out.push(name.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

pub struct Decoded {
    pub observations: Vec<Observation>,
    /// Rows that did not have the header's field count or a readable
    /// identifier, time or position.
    pub malformed: usize,
}

/// The columns this driver reads, as the header names them. The header
/// repeats `+/-` after most values, which is why lookup is by first
/// occurrence of a name and the sigma is never named.
const NEEDED: [&str; 4] = ["Unique trajectory", "LatBeg", "LonBeg", "HtBeg"];

/// Decode one daily summary file.
pub fn decode(text: &str, source_id: &SourceId) -> Result<Decoded, SourceError> {
    let mut header_lines = text.lines().map(str::trim).filter(|l| l.starts_with('#'));
    // Line one is the generation stamp; line two names the columns; three
    // gives units; four is a rule.
    let _generated = header_lines.next();
    let names_line = header_lines
        .next()
        .ok_or_else(|| SourceError::Decode("GMN file has no column header".into()))?;
    let names: Vec<&str> = names_line[1..].split(';').map(str::trim).collect();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, name) in names.iter().enumerate() {
        // First occurrence wins: "Beginning" appears twice (Julian date,
        // then UTC), and the second is the one wanted, so it is special-cased.
        index.entry(name).or_insert(i);
    }
    for column in NEEDED {
        if !index.contains_key(column) {
            return Err(SourceError::Decode(format!(
                "GMN header has no `{column}` column; got {} columns starting {:?}",
                names.len(),
                names.iter().take(4).collect::<Vec<_>>()
            )));
        }
    }
    let utc_col = names
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == "Beginning")
        .nth(1)
        .map(|(i, _)| i)
        .ok_or_else(|| SourceError::Decode("GMN header has no UTC time column".into()))?;
    // The shower code is the second "IAU" column; the first is the number.
    let shower_col = names
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == "IAU")
        .nth(1)
        .map(|(i, _)| i);

    let mut decoded = Decoded {
        observations: Vec::new(),
        malformed: 0,
    };
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(';').map(str::trim).collect();
        if fields.len() != names.len() {
            decoded.malformed += 1;
            continue;
        }
        let get = |name: &str| -> Option<&str> { index.get(name).map(|&i| fields[i]) };
        let num = |name: &str| -> Option<f64> { get(name).and_then(|v| v.parse().ok()) };

        let Some(id) = get("Unique trajectory").filter(|s| !s.is_empty()) else {
            decoded.malformed += 1;
            continue;
        };
        let Some(began) = NaiveDateTime::parse_from_str(fields[utc_col], "%Y-%m-%d %H:%M:%S%.f")
            .ok()
            .map(|t| t.and_utc())
        else {
            decoded.malformed += 1;
            continue;
        };
        let (Some(lat_beg), Some(lon_beg), Some(ht_beg)) =
            (num("LatBeg"), num("LonBeg"), num("HtBeg"))
        else {
            decoded.malformed += 1;
            continue;
        };
        if !(-90.0..=90.0).contains(&lat_beg) || !(-180.0..=180.0).contains(&lon_beg) {
            decoded.malformed += 1;
            continue;
        }

        let shower = shower_col
            .map(|i| fields[i])
            .filter(|s| !s.is_empty() && *s != "...");

        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("trajectory", serde_json::json!(id));
        put("shower", serde_json::json!(shower));
        put(
            "shower_iau_no",
            serde_json::json!(num("IAU").filter(|n| *n >= 0.0).map(|n| n as i64)),
        );
        put("height_begin_km", serde_json::json!(ht_beg));
        put("height_end_km", serde_json::json!(num("HtEnd")));
        put("lat_end", serde_json::json!(num("LatEnd")));
        put("lon_end", serde_json::json!(num("LonEnd")));
        put("duration_s", serde_json::json!(num("Duration")));
        // Geocentric velocity is the one that says what kind of object it
        // was; the initial and average are what the cameras saw.
        put("v_geo_kms", serde_json::json!(num("Vgeo")));
        put("v_init_kms", serde_json::json!(num("Vinit")));
        put("v_avg_kms", serde_json::json!(num("Vavg")));
        put("peak_abs_mag", serde_json::json!(num("Peak")));
        put("peak_height_km", serde_json::json!(num("Peak Ht")));
        put("mass_kg", serde_json::json!(num("Mass kg")));
        // The heliocentric orbit, from the elements the network publishes.
        let mut orbit = serde_json::Map::new();
        for (key, col) in [
            ("a_au", "a"),
            ("e", "e"),
            ("i_deg", "i"),
            ("q_au", "q"),
            ("peri_deg", "peri"),
            ("node_deg", "node"),
            ("tisserand_j", "TisserandJ"),
        ] {
            if let Some(v) = num(col) {
                orbit.insert(key.into(), serde_json::json!(v));
            }
        }
        if !orbit.is_empty() {
            put("orbit", serde_json::Value::Object(orbit));
        }
        put("convergence_deg", serde_json::json!(num("Qc")));
        put("fit_err_arcsec", serde_json::json!(num("MedianFitErr")));
        put(
            "station_count",
            serde_json::json!(num("Num").map(|n| n as i64)),
        );
        if let Some(stations) = get("Participating") {
            let list: Vec<&str> = stations
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                put("stations", serde_json::json!(list));
            }
        }

        let label = match shower {
            Some(code) => format!("{code} meteor"),
            None => "Sporadic meteor".to_string(),
        };

        let mut obs = Observation::new(
            source_id.clone(),
            EntityId::new(EntityKind::Event, id),
            began,
            Quality::Live,
        )
        .with_position(Position {
            lon: lon_beg,
            lat: lat_beg,
            alt_m: Some(ht_beg * 1000.0),
            datum: AltitudeDatum::Wgs84Ellipsoid,
        })
        .with_label(label)
        .with_attrs(serde_json::Value::Object(attrs));

        // The ground track from where it lit to where it went out. Only
        // when the end is placed; a meteor with no end point is still an
        // event at its beginning.
        if let (Some(lat_end), Some(lon_end)) = (num("LatEnd"), num("LonEnd"))
            && (-90.0..=90.0).contains(&lat_end)
            && (-180.0..=180.0).contains(&lon_end)
        {
            obs = obs.with_geom(Geometry::LineString(LineString(vec![
                Coord {
                    x: lon_beg,
                    y: lat_beg,
                },
                Coord {
                    x: lon_end,
                    y: lat_end,
                },
            ])));
        }
        decoded.observations.push(obs);
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    fn source() -> SourceId {
        SourceId::new("gmn-meteors")
    }

    /// The header verbatim, and two rows from a live file: a sporadic seen
    /// by two Spanish stations and a nu Eridanid seen by three.
    const FILE: &str = "\
# Summary generated on 2026-09-16 06:06:14.542816+00:00 UTC
#  Unique trajectory;      Beginning      ;        Beginning          ;   IAU;   IAU;   Sol lon ;   App LST ;   RAgeo  ;   +/-  ;   DECgeo ;   +/-  ;  LAMgeo  ;   +/-  ;   BETgeo ;   +/-  ;    Vgeo  ;    +/- ;  LAMhel  ;   +/-  ;   BEThel ;   +/-  ;    Vhel  ;    +/- ;       a    ;   +/-  ;      e    ;   +/-  ;      i    ;   +/-  ;    peri   ;    +/-  ;    node   ;    +/-  ;     Pi    ;   +/-  ;      b    ;   +/-  ;      q    ;   +/-  ;      f    ;   +/-  ;      M    ;   +/-  ;       Q    ;   +/-  ;      n    ;   +/-  ;      T    ;   +/-  ; TisserandJ;   +/-  ;   RAapp  ;   +/-  ;   DECapp ;   +/-  ;  Azim +E ;   +/-  ;    Elev  ;   +/-  ;   Vinit  ;    +/- ;    Vavg  ;    +/- ;    LatBeg   ;   +/-  ;    LonBeg   ;   +/-  ;   HtBeg ;   +/-  ;    LatEnd   ;   +/-  ;    LonEnd   ;   +/-  ;   HtEnd ;   +/-  ; Duration;  Peak ;  Peak Ht;   F  ;  Mass kg;   Qc ; MedianFitErr; Beg in; End in;  Num;      Participating
#      identifier   ;     Julian date     ;         UTC Time          ;    No;  code;     deg   ;     deg   ;    deg   ;  sigma ;    deg   ;  sigma ;    deg   ;  sigma ;     deg  ;  sigma ;    km/s  ;   sigma;    deg   ;  sigma ;     deg  ;  sigma ;    km/s  ;   sigma;      AU    ;  sigma ;           ;  sigma ;    deg    ;  sigma ;     deg   ;   sigma ;     deg   ;   sigma ;    deg    ;  sigma ;    deg    ;  sigma ;     AU    ;  sigma ;    deg    ;  sigma ;     deg   ;  sigma ;      AU    ;  sigma ;   deg/day ;  sigma ;    years  ;  sigma ;           ;  sigma ;    deg   ;  sigma ;    deg   ;  sigma ; of N  deg;  sigma ;     deg  ;  sigma ;    km/s  ;   sigma;    km/s  ;   sigma;    +N deg   ;  sigma ;    +E deg   ;  sigma ;     km  ;  sigma ;    +N deg   ;  sigma ;    +E deg   ;  sigma ;     km  ;  sigma ;   sec   ; AbsMag;     km  ; param; tau=0.7%;  deg ;    arcsec   ;   FOV ;   FOV ; stat;         stations
# ------------------; --------------------; --------------------------; -----; -----; ----------; ----------; ---------; -------; ---------; -------; ---------; -------; ---------; -------; ---------; -------; ---------; -------; ---------; -------; ---------; -------; -----------; -------; ----------; -------; ----------; -------; ----------; --------; ----------; --------; ----------; -------; ----------; -------; ----------; -------; ----------; -------; ----------; -------; -----------; -------; ----------; -------; ----------; -------; ----------; -------; ---------; -------; ---------; -------; ---------; -------; ---------; -------; ---------; -------; ---------; -------; ------------; -------; ------------; -------; --------; -------; ------------; -------; ------------; -------; --------; -------; --------; ------; --------; -----; --------; -----; ------------; ------; ------; ----; ----------------------
20260915043756_p5wbM; 2461298.693017735612; 2026-09-15 04:37:56.732357;    -1;   ...; 172.004230;  62.793333;  60.38223;  0.3974; +47.16856;  0.5206;  68.05614;  0.1947; +25.96182;  0.5614;  60.35473;  0.9405;  51.54786;  0.8928; +44.66514;  1.1849;  37.58812;  0.6744;    2.525866;  0.8020;   0.667152;  0.0478; 131.092939;  1.4252; 233.844905;   2.4825; 172.005594;   0.0000;  45.850499;  2.4825; -37.481828;  2.2616;   0.840730;  0.0087; 306.153704;  2.4824; 350.928679;  1.9234;    4.211003;  1.5964;   0.245521;  0.0472;   4.014358;  2.4187;   1.378179;  0.3383;  61.38688;  0.3817; +47.18094;  0.5172; 353.16669;  1.6120;  81.93984;  0.5453;  61.35806; 14.6897;  57.77721;  1.0328;   39.185889;  0.0005;   -0.857781;  0.0005; 111.1352;    0.28;   39.168384;  0.0003;   -0.855132;  0.0003;  97.1024;    0.13;     0.24;  +0.80; 100.8557; 0.733; 6.78e-06; 56.71;        67.32;   True;   True;    2; ES001D,ES001M
20260914221145_x1AbC; 2461298.426215277985; 2026-09-14 22:11:45.100000;   337;   NUE; 171.750000;  40.100000;  68.10000;  0.2000; -01.20000;  0.2000;  60.00000;  0.1000; -20.00000;  0.2000;  66.00000;  0.5000;  50.00000;  0.5000; +30.00000;  0.5000;  38.00000;  0.5000;    5.000000;  1.0000;   0.900000;  0.0200; 150.000000;  1.0000; 200.000000;   1.0000; 171.750000;   0.0000;  11.750000;  1.0000; -20.000000;  1.0000;   0.500000;  0.0100; 300.000000;  1.0000; 340.000000;  1.0000;    9.500000;  1.0000;   0.100000;  0.0100;  11.000000;  1.0000;   1.000000;  0.1000;  69.00000;  0.2000; -01.00000;  0.2000; 100.00000;  1.0000;  40.00000;  0.5000;  67.00000;  1.0000;  65.00000;  1.0000;   52.100000;  0.0005;    5.200000;  0.0005; 105.0000;    0.30;   52.000000;  0.0003;    5.300000;  0.0003;  80.0000;    0.20;     0.60;  -1.50;  92.0000; 0.500; 1.20e-04; 30.00;        40.00;   True;   True;    3; NL001E,UK0026,UK004J
";

    #[test]
    fn every_row_becomes_an_event_with_a_line_from_beginning_to_end() {
        let d = decode(FILE, &source()).unwrap();
        assert_eq!(d.malformed, 0);
        assert_eq!(d.observations.len(), 2);
        let m = &d.observations[0];
        assert_eq!(m.entity.kind, EntityKind::Event);
        assert_eq!(m.entity.key, "20260915043756_p5wbM");
        assert_eq!(
            m.observed_at,
            "2026-09-15T04:37:56.732357Z"
                .parse::<DateTime<Utc>>()
                .unwrap()
        );
        let p = m.position.unwrap();
        assert_eq!((p.lon, p.lat), (-0.857781, 39.185889));
        assert_eq!(p.alt_m, Some(111_135.2));
        let Some(Geometry::LineString(line)) = m.geom.as_ref() else {
            panic!("expected a line");
        };
        assert_eq!(line.0.len(), 2);
        assert_eq!(
            line.0[1],
            Coord {
                x: -0.855132,
                y: 39.168384
            }
        );
    }

    #[test]
    fn a_sporadic_has_no_shower_and_a_member_names_its_code() {
        let d = decode(FILE, &source()).unwrap();
        let sporadic = &d.observations[0];
        assert!(
            sporadic.attrs.get("shower").is_none(),
            "`...` is not a shower"
        );
        assert!(
            sporadic.attrs.get("shower_iau_no").is_none(),
            "-1 is not a number"
        );
        assert_eq!(sporadic.label.as_deref(), Some("Sporadic meteor"));
        let nue = &d.observations[1];
        assert_eq!(nue.attrs["shower"], serde_json::json!("NUE"));
        assert_eq!(nue.attrs["shower_iau_no"], serde_json::json!(337));
        assert_eq!(nue.label.as_deref(), Some("NUE meteor"));
    }

    #[test]
    fn the_heights_and_orbit_are_carried_because_the_geometry_is_flat() {
        let d = decode(FILE, &source()).unwrap();
        let m = &d.observations[0];
        assert_eq!(m.attrs["height_begin_km"], serde_json::json!(111.1352));
        assert_eq!(m.attrs["height_end_km"], serde_json::json!(97.1024));
        assert_eq!(m.attrs["v_geo_kms"], serde_json::json!(60.35473));
        assert_eq!(m.attrs["mass_kg"], serde_json::json!(6.78e-06));
        assert_eq!(m.attrs["orbit"]["a_au"], serde_json::json!(2.525866));
        assert_eq!(m.attrs["orbit"]["e"], serde_json::json!(0.667152));
        assert_eq!(m.attrs["stations"], serde_json::json!(["ES001D", "ES001M"]));
        assert_eq!(m.attrs["station_count"], serde_json::json!(2));
    }

    #[test]
    fn a_column_added_upstream_moves_nothing_because_columns_are_named() {
        // Insert a new column (with its sigma) after Vgeo in the header and
        // both rows; everything read by name must be unchanged.
        let with_extra = FILE
            .replace("    Vgeo  ;    +/- ;  LAMhel", "    Vgeo  ;    +/- ;  Extra ;    +/- ;  LAMhel")
            .replace("    km/s  ;   sigma;    deg   ;  sigma ;     deg  ;  sigma ;    km/s  ;   sigma;      AU", "    km/s  ;   sigma;    x   ;  sigma ;    deg   ;  sigma ;     deg  ;  sigma ;    km/s  ;   sigma;      AU")
            .replace("  60.35473;  0.9405;", "  60.35473;  0.9405;  1.0;  0.1;")
            .replace("  66.00000;  0.5000;  50.00000", "  66.00000;  0.5000;  1.0;  0.1;  50.00000")
            .replace("; ---------; -------; ---------; -------; ---------; -------; ---------; -------; -----------", "; ---------; -------; ---------; -------; ---------; -------; ---------; -------; ---------; -------; -----------");
        let d = decode(&with_extra, &source()).unwrap();
        assert_eq!(
            d.malformed, 0,
            "{} rows did not match the widened header",
            d.malformed
        );
        assert_eq!(
            d.observations[0].attrs["height_end_km"],
            serde_json::json!(97.1024)
        );
        assert_eq!(
            d.observations[0].attrs["v_geo_kms"],
            serde_json::json!(60.35473)
        );
    }

    #[test]
    fn the_live_files_newline_then_carriage_return_endings_are_survived() {
        // `\n\r`, as served. Unhandled, the second line starts with `\r`,
        // the header is never found, and a day of meteors decodes to nothing.
        let as_served = FILE.replace('\n', "\n\r");
        let d = decode(&as_served, &source()).unwrap();
        assert_eq!(d.observations.len(), 2);
        assert_eq!(d.malformed, 0);
    }

    #[test]
    fn a_missing_position_column_is_a_loud_error() {
        let renamed = FILE.replace("LatBeg", "LatStart");
        let err = decode(&renamed, &source()).err().expect("decode error");
        assert!(
            matches!(err, SourceError::Decode(ref m) if m.contains("LatBeg")),
            "{err}"
        );
    }

    #[test]
    fn a_short_row_is_counted_not_guessed_at() {
        let truncated = format!(
            "{FILE}20260915000000_zzzzz; 2461298.5; 2026-09-15 00:00:00.000000;    -1;   ...\n"
        );
        let d = decode(&truncated, &source()).unwrap();
        assert_eq!(d.observations.len(), 2);
        assert_eq!(d.malformed, 1);
    }

    #[test]
    fn the_index_yields_the_dated_files_inside_the_window_and_not_the_aliases() {
        let html = r#"<a href="traj_summary_20260908_solrange_165.0-166.0.txt">x</a>
<a href="traj_summary_20260909_solrange_166.0-167.0.txt">x</a>
<a href="traj_summary_20260914_solrange_171.0-172.0.txt">x</a>
<a href="traj_summary_20260915_solrange_172.0-173.0.txt">x</a>
<a href="traj_summary_latest_daily.txt">x</a>
<a href="traj_summary_yesterday.txt">x</a>
<a href="traj_summary_monthly_202609.txt">x</a>"#;
        let files = dated_files(
            html,
            NaiveDate::from_ymd_opt(2026, 9, 9).unwrap(),
            NaiveDate::from_ymd_opt(2026, 9, 14).unwrap(),
        );
        assert_eq!(
            files,
            vec![
                "traj_summary_20260909_solrange_166.0-167.0.txt",
                "traj_summary_20260914_solrange_171.0-172.0.txt",
            ]
        );
    }
}
