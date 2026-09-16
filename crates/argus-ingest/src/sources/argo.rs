//! Argo floats: the last known position of every profiling float in the
//! global array, from the Coriolis GDAC's ERDDAP.
//!
//! An Argo float drifts at a kilometre's depth for about ten days, rises to
//! the surface measuring temperature and salinity on the way, reports its
//! profile and position by satellite, and sinks again. Four thousand of them
//! cover every ocean. The position is only ever known at the surface, so the
//! layer is each float's most recent surfacing: where it was, when, and on
//! which cycle.
//!
//! ## The profile data is not fetched, and the reason is measured
//!
//! ERDDAP's `ArgoFloats` table has one row per *pressure level*, and a
//! profile has hundreds. Asking for surface levels (`pres<=12`) over the
//! window this driver uses took 317 seconds; asking the server to pick the
//! shallowest level per profile (`orderByMin`) ran into its proxy's 300
//! second limit and came back as a 502. Asking for profile-level columns
//! with `distinct()` takes twenty seconds for thirty days, because the
//! server never has to look at a level. So the layer is where the floats
//! are, and a card links to the float's own page for what it measured.
//!
//! ## Poll time is the observation time, as for the outfalls
//!
//! A float surfaces every ten days; the `Station` horizon is a day. Dated by
//! its surfacing, 90% of the array would be hidden at any moment — 357 of
//! 4,315 floats had surfaced in the last 24 hours when this was written. The
//! same reasoning as the storm overflows applies: the GDAC is asserting this
//! position *now*, and how old the reading behind it is becomes
//! [`Quality::Stale`] plus a `surfaced_at` attribute, so a client draws a
//! five-day-old position as what it is rather than hiding it or passing it
//! off as fresh.
//!
//! ## What the whole window showed
//!
//! - 50 of 5,615 profiles in a twelve-day pull had no position at all, all
//!   of them `position_qc` 9 (missing). Two more were qc 4 (bad). Both are
//!   dropped; 8 (interpolated) and 2 (probably good) are kept and the flag
//!   is carried.
//! - 243 rows shared a float and cycle number with different times: a
//!   descending profile on the way down and an ascending one on the way up
//!   are the same cycle. The latest time per float wins, whatever the cycle.
//! - `platform_number` is a string on the wire (`"1901514"`), which is right
//!   — it is a WMO identifier, not a count — and is kept as one.
//! - Thirty days, because that is Argo's own definition of an active float.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::HashMap;

const BASE_URL: &str = "https://erddap.ifremer.fr/erddap/tabledap/ArgoFloats.json";

/// A profile takes hours to reach the GDAC after the float surfaces, and a
/// float surfaces every ten days. An hour sees each one within a fraction
/// of its cycle without asking a slow server the same question all day.
const CADENCE_SECS: u64 = 3600;

/// Argo counts a float as active if it has reported within thirty days.
const WINDOW: Duration = Duration::days(30);

/// The columns asked for, in the order the query names them. Read back by
/// name from the response's `columnNames`, so a reordering upstream is
/// harmless and a renaming is a loud decode error rather than a shifted
/// column.
const COLUMNS: [&str; 14] = [
    "platform_number",
    "cycle_number",
    "time",
    "latitude",
    "longitude",
    "platform_type",
    "project_name",
    "pi_name",
    "data_center",
    "data_mode",
    "position_qc",
    "direction",
    "positioning_system",
    "wmo_inst_type",
];

pub struct ArgoFloats {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl ArgoFloats {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("argo-floats"),
                layer_id: LayerId::new("argo-floats"),
                display_name: "Argo profiling floats".into(),
                kind: EntityKind::Station,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "Argo, via the Coriolis GDAC (Ifremer)".into(),
                    url: "https://argo.ucsd.edu/".into(),
                    license: "CC BY 4.0".into(),
                    notice: Some(
                        "These data were collected and made freely available by the International Argo Program and the national programs that contribute to it"
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

/// The query for every profile in the window. ERDDAP's constraint syntax
/// puts `>=` and `<=` in the query string, and Tomcat returns 400 unless
/// they and the commas are percent-encoded.
fn url(now: DateTime<Utc>) -> String {
    let from = (now - WINDOW).format("%Y-%m-%dT%H:%M:%SZ");
    let to = (now + Duration::days(1)).format("%Y-%m-%dT%H:%M:%SZ");
    format!(
        "{BASE_URL}?{}&time%3E%3D{from}&time%3C%3D{to}&distinct()",
        COLUMNS.join("%2C")
    )
}

#[async_trait::async_trait]
impl Source for ArgoFloats {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let now = Utc::now();
        let bytes = self.http.get_bytes(&url(now)).await?;
        let text = String::from_utf8_lossy(&bytes);
        let decoded = decode(&text, &self.descriptor.id, now)?;
        tracing::debug!(
            source = %self.descriptor.id,
            floats = decoded.observations.len(),
            unplaced = decoded.unplaced,
            "argo floats decoded"
        );
        Ok(decoded.observations)
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Response {
    table: Table,
}

#[derive(Debug, Deserialize)]
struct Table {
    #[serde(rename = "columnNames")]
    column_names: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
}

pub struct Decoded {
    pub observations: Vec<Observation>,
    /// Profiles with no usable position: `position_qc` 4 or 9, or a null.
    pub unplaced: usize,
}

/// One profile, read by column name.
struct Profile<'a> {
    float: &'a str,
    time: DateTime<Utc>,
    lon: f64,
    lat: f64,
    row: &'a [serde_json::Value],
}

pub fn decode(
    text: &str,
    source_id: &SourceId,
    now: DateTime<Utc>,
) -> Result<Decoded, SourceError> {
    let response: Response =
        serde_json::from_str(text).map_err(|e| SourceError::Decode(format!("ERDDAP json: {e}")))?;
    let table = response.table;

    let mut index = HashMap::new();
    for (i, name) in table.column_names.iter().enumerate() {
        index.insert(name.as_str(), i);
    }
    for column in COLUMNS {
        if !index.contains_key(column) {
            return Err(SourceError::Decode(format!(
                "ERDDAP response has no `{column}` column; got {:?}",
                table.column_names
            )));
        }
    }
    let col = |row: &[serde_json::Value], name: &str| -> serde_json::Value {
        row.get(index[name])
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };

    let mut unplaced = 0usize;
    // The latest profile per float. A cycle can appear twice (descending
    // and ascending), and a float can report several cycles in the window.
    let mut latest: HashMap<&str, Profile<'_>> = HashMap::new();
    for row in &table.rows {
        let Some(float) = row
            .get(index["platform_number"])
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Some(time) = col(row, "time")
            .as_str()
            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        else {
            continue;
        };
        let qc = col(row, "position_qc");
        let qc = qc.as_str().unwrap_or("").trim();
        let (Some(lon), Some(lat)) = (
            col(row, "longitude").as_f64(),
            col(row, "latitude").as_f64(),
        ) else {
            unplaced += 1;
            continue;
        };
        // 4 is bad, 9 is missing; a missing one also has null coordinates,
        // but a bad one has real-looking numbers somewhere the float is not.
        if qc == "4"
            || qc == "9"
            || !(-90.0..=90.0).contains(&lat)
            || !(-180.0..=180.0).contains(&lon)
        {
            unplaced += 1;
            continue;
        }
        let profile = Profile {
            float,
            time,
            lon,
            lat,
            row,
        };
        match latest.get(float) {
            Some(existing) if existing.time >= time => {}
            _ => {
                latest.insert(float, profile);
            }
        }
    }

    let horizon = EntityKind::Station
        .live_horizon()
        .expect("stations have a live horizon");
    let mut observations: Vec<Observation> = latest
        .into_values()
        .map(|p| {
            let text = |name: &str| -> Option<String> {
                col(p.row, name)
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            let age = now - p.time;
            // The GDAC is serving this position now; the reading behind it
            // may be a week old, and that is a different fact.
            let quality = if age > horizon {
                Quality::Stale
            } else {
                Quality::Live
            };

            let mut attrs = serde_json::Map::new();
            let mut put = |k: &str, v: serde_json::Value| {
                if !v.is_null() {
                    attrs.insert(k.to_string(), v);
                }
            };
            put("float", serde_json::json!(p.float));
            put("cycle", col(p.row, "cycle_number"));
            put(
                "surfaced_at",
                serde_json::json!(p.time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            );
            put("stale", serde_json::json!(quality == Quality::Stale));
            put("direction", serde_json::json!(text("direction")));
            put("platform_type", serde_json::json!(text("platform_type")));
            put("project", serde_json::json!(text("project_name")));
            put("pi", serde_json::json!(text("pi_name")));
            put("data_center", serde_json::json!(text("data_center")));
            // R real-time, A adjusted, D delayed-mode: how far through quality
            // control the profile has been.
            put("data_mode", serde_json::json!(text("data_mode")));
            put(
                "positioning_system",
                serde_json::json!(text("positioning_system")),
            );
            put("position_qc", serde_json::json!(text("position_qc")));
            put(
                "url",
                serde_json::json!(format!(
                    "https://fleetmonitoring.euro-argo.eu/float/{}",
                    p.float
                )),
            );

            Observation::new(
                source_id.clone(),
                EntityId::new(EntityKind::Station, p.float),
                now,
                quality,
            )
            .with_position(Position {
                lon: p.lon,
                lat: p.lat,
                alt_m: None,
                datum: AltitudeDatum::Geoid,
            })
            .with_label(p.float)
            .with_attrs(serde_json::Value::Object(attrs))
        })
        .collect();
    // A HashMap's order is arbitrary; a stable order makes a poll's output
    // comparable between runs and tests deterministic.
    observations.sort_by(|a, b| a.entity.key.cmp(&b.entity.key));

    Ok(Decoded {
        observations,
        unplaced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> SourceId {
        SourceId::new("argo-floats")
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an instant")
    }

    /// The live envelope with the column order the server sent, and five
    /// rows from a real pull: a float with two cycles, one of them twice
    /// (descending and ascending), a float with no position (qc 9), and one
    /// that surfaced within the day.
    const RESPONSE: &str = r#"{
  "table": {
    "columnNames": ["platform_number", "cycle_number", "time", "latitude", "longitude", "platform_type", "project_name", "pi_name", "data_center", "data_mode", "position_qc", "direction", "positioning_system", "wmo_inst_type"],
    "columnTypes": ["String", "int", "String", "double", "double", "String", "String", "String", "String", "String", "String", "String", "String", "String"],
    "columnUnits": [null, null, "UTC", "degrees_north", "degrees_east", null, null, null, null, null, null, null, null, null],
    "rows": [
      ["1901615", 246, "2026-09-05T09:40:00Z", -33.1, 12.4, "APEX", "US ARGO PROJECT", "Gregory C JOHNSON", "AO", "A", "1", "A", "GPS", "846"],
      ["1901615", 247, "2026-09-14T09:52:00Z", -33.4, 12.9, "APEX", "US ARGO PROJECT", "Gregory C JOHNSON", "AO", "R", "1", "D", "GPS", "846"],
      ["1901615", 247, "2026-09-14T21:12:00Z", -33.45, 12.95, "APEX", "US ARGO PROJECT", "Gregory C JOHNSON", "AO", "R", "1", "A", "GPS", "846"],
      ["2903020", 12, "2026-09-14T03:00:00Z", null, null, "ARVOR", "Argo Italy", "Someone", "IF", "R", "9", "A", "IRIDIUM", "844"],
      ["5906999", 3, "2026-09-16T06:48:40Z", 51.2, -20.3, "NAVIS_EBR", "Argo WHOI", "Susan WIJFFELS, Steven JAYNE, Pelle ROBBINS", "AO", "R", "1", "A", "GPS", "869"]
    ]
  }
}"#;

    const NOW: &str = "2026-09-16T11:30:00Z";

    #[test]
    fn one_observation_per_float_at_its_latest_surfacing() {
        let d = decode(RESPONSE, &source(), at(NOW)).unwrap();
        assert_eq!(d.observations.len(), 2, "two placed floats");
        assert_eq!(d.unplaced, 1, "the qc-9 profile has no position");
        let f = &d.observations[0];
        assert_eq!(f.entity.key, "1901615");
        assert_eq!(f.entity.kind, EntityKind::Station);
        // The ascending profile of cycle 247, not the descending one twelve
        // hours earlier and not cycle 246.
        assert_eq!(
            f.attrs["surfaced_at"],
            serde_json::json!("2026-09-14T21:12:00Z")
        );
        assert_eq!(f.attrs["cycle"], serde_json::json!(247));
        assert_eq!(f.attrs["direction"], serde_json::json!("A"));
        let p = f.position.unwrap();
        assert_eq!((p.lon, p.lat), (12.95, -33.45));
    }

    #[test]
    fn the_poll_time_is_the_observation_and_the_surfacing_decides_the_quality() {
        let d = decode(RESPONSE, &source(), at(NOW)).unwrap();
        for o in &d.observations {
            assert_eq!(
                o.observed_at,
                at(NOW),
                "dated by the poll, as the outfalls are"
            );
        }
        // Five hours old: within the station horizon, live.
        let fresh = &d.observations[1];
        assert_eq!(fresh.entity.key, "5906999");
        assert_eq!(fresh.quality, Quality::Live);
        assert_eq!(fresh.attrs["stale"], serde_json::json!(false));
        // 38 hours old: past it, and marked so rather than hidden.
        let old = &d.observations[0];
        assert_eq!(old.quality, Quality::Stale);
        assert_eq!(old.attrs["stale"], serde_json::json!(true));
    }

    #[test]
    fn a_renamed_column_is_a_loud_error_not_a_shifted_one() {
        let renamed = RESPONSE.replace("\"platform_number\"", "\"wmo_number\"");
        let err = decode(&renamed, &source(), at(NOW))
            .err()
            .expect("decode error");
        assert!(
            matches!(err, SourceError::Decode(ref m) if m.contains("platform_number")),
            "{err}"
        );
    }

    #[test]
    fn columns_are_read_by_name_so_reordering_is_harmless() {
        // Swap latitude and longitude in both the header and every row.
        let swapped = RESPONSE
            .replace("\"latitude\", \"longitude\"", "\"longitude\", \"latitude\"")
            .replace("-33.45, 12.95", "12.95, -33.45")
            .replace("-33.4, 12.9", "12.9, -33.4")
            .replace("-33.1, 12.4", "12.4, -33.1")
            .replace("51.2, -20.3", "-20.3, 51.2");
        let d = decode(&swapped, &source(), at(NOW)).unwrap();
        let p = d.observations[0].position.unwrap();
        assert_eq!((p.lon, p.lat), (12.95, -33.45));
    }

    #[test]
    fn a_bad_position_is_dropped_even_though_it_has_numbers() {
        let bad = RESPONSE.replace(
            "51.2, -20.3, \"NAVIS_EBR\", \"Argo WHOI\", \"Susan WIJFFELS, Steven JAYNE, Pelle ROBBINS\", \"AO\", \"R\", \"1\"",
            "51.2, -20.3, \"NAVIS_EBR\", \"Argo WHOI\", \"Susan WIJFFELS, Steven JAYNE, Pelle ROBBINS\", \"AO\", \"R\", \"4\"",
        );
        let d = decode(&bad, &source(), at(NOW)).unwrap();
        assert_eq!(d.observations.len(), 1);
        assert_eq!(d.unplaced, 2);
    }

    #[test]
    fn the_wmo_number_stays_a_string_and_links_to_the_float_page() {
        let d = decode(RESPONSE, &source(), at(NOW)).unwrap();
        let f = &d.observations[0];
        assert_eq!(f.attrs["float"], serde_json::json!("1901615"));
        assert_eq!(f.label.as_deref(), Some("1901615"));
        assert_eq!(
            f.attrs["url"],
            serde_json::json!("https://fleetmonitoring.euro-argo.eu/float/1901615")
        );
        assert_eq!(f.attrs["project"], serde_json::json!("US ARGO PROJECT"));
        assert_eq!(f.attrs["data_mode"], serde_json::json!("R"));
    }

    #[test]
    fn the_query_encodes_what_tomcat_rejects_and_asks_for_thirty_days() {
        let u = url(at(NOW));
        assert!(u.contains("time%3E%3D2026-08-17T11:30:00Z"), "{u}");
        assert!(u.contains("time%3C%3D2026-09-17T11:30:00Z"), "{u}");
        assert!(u.ends_with("&distinct()"), "{u}");
        assert!(
            !u.contains(">") && !u.contains("<") && !u.contains(","),
            "{u}"
        );
        assert!(u.contains("platform_number%2Ccycle_number"), "{u}");
    }
}
