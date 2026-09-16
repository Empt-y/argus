//! Fireballs: the bright meteors that US government sensors detect from
//! orbit, from JPL's Center for Near-Earth Object Studies.
//!
//! CNEOS publishes every fireball reported by the sensors that watch for
//! other things — the date to the second, the peak brightness position
//! and altitude, the total radiated energy, the impact energy in kilotons
//! of TNT, and for a third of them the velocity vector. `req-loc=true`
//! asks only for the records with a position: 887 of them since 1988 in
//! one 80 KB answer, forty or so a year. The one from April 1988 is left
//! out on purpose — the store refuses anything before 1990 as a decode
//! failure, and one record is not worth loosening that for.
//!
//! An [`EntityKind::Event`] dated by the detection, so the layer is usually
//! empty in the live view and fills in as the DVR scrubs back; a Chelyabinsk
//! is news for a week and history after. Six-hourly: JPL adds an event days
//! after it happens, and there is nothing to be gained by asking more often.
//! Distinct from the `meteors` layer, which is ground cameras seeing
//! grain-of-sand meteors by the thousand; these are the boulders.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

const API_URL: &str = "https://ssd-api.jpl.nasa.gov/fireball.api?req-loc=true&vel-comp=true";

const CADENCE_SECS: u64 = 6 * 3600;

pub struct Fireballs {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl Fireballs {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("cneos-fireballs"),
                layer_id: LayerId::new("fireballs"),
                display_name: "Fireballs (NASA/JPL CNEOS)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "NASA/JPL Center for Near-Earth Object Studies".into(),
                    url: "https://cneos.jpl.nasa.gov/fireballs/".into(),
                    license: "Public domain (NASA)".into(),
                    notice: Some("Fireball data courtesy of NASA/JPL CNEOS; reported by US Government sensors".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for Fireballs {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let feed: Feed = self.http.get_json(API_URL).await?;
        let now = Utc::now();
        let decoded = decode(&feed, &self.descriptor.id, now)?;
        tracing::info!(source = %self.descriptor.id, fireballs = decoded.observations.len(), unplaced = decoded.unplaced, before_1990 = decoded.before_floor, "fireballs read");
        Ok(decoded.observations)
    }
}

// --- wire format -----------------------------------------------------------

/// `fields` names the columns; `data` is rows of strings or nulls.
#[derive(Debug, Deserialize)]
pub struct Feed {
    #[serde(default)]
    fields: Vec<String>,
    #[serde(default)]
    data: Vec<Vec<Option<String>>>,
}

#[derive(Debug)]
pub struct Decoded {
    pub observations: Vec<Observation>,
    pub unplaced: usize,
    /// Older than the store accepts.
    pub before_floor: usize,
}

/// The store's floor is 1990; the same, so a record from 1988 is skipped
/// here rather than refused there as a driver bug.
const EARLIEST: DateTime<Utc> = chrono::DateTime::<Utc>::from_naive_utc_and_offset(
    chrono::NaiveDate::from_ymd_opt(1990, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap(),
    Utc,
);

pub fn decode(feed: &Feed, source_id: &SourceId, now: DateTime<Utc>) -> Result<Decoded, SourceError> {
    let col = |name: &str| feed.fields.iter().position(|f| f == name);
    let (Some(c_date), Some(c_lat), Some(c_lat_dir), Some(c_lon), Some(c_lon_dir)) = (col("date"), col("lat"), col("lat-dir"), col("lon"), col("lon-dir")) else {
        return Err(SourceError::Decode(format!("fireball API fields changed: {:?}", feed.fields)));
    };
    let c_energy = col("energy");
    let c_impact = col("impact-e");
    let c_alt = col("alt");
    let c_vel = col("vel");
    let (c_vx, c_vy, c_vz) = (col("vx"), col("vy"), col("vz"));

    let mut observations = Vec::with_capacity(feed.data.len());
    let mut unplaced = 0;
    let mut before_floor = 0;
    for row in &feed.data {
        let field = |c: Option<usize>| c.and_then(|i| row.get(i)).and_then(|v| v.as_deref()).map(str::trim).filter(|s| !s.is_empty());
        let num = |c: Option<usize>| field(c).and_then(|s| s.parse::<f64>().ok());
        let Some(date) = field(Some(c_date)) else { continue };
        let Some(at) = chrono::NaiveDateTime::parse_from_str(date, "%Y-%m-%d %H:%M:%S").ok().map(|t| t.and_utc()) else { continue };
        if at < EARLIEST {
            before_floor += 1;
            continue;
        }
        let (Some(lat), Some(lon)) = (num(Some(c_lat)), num(Some(c_lon))) else {
            unplaced += 1;
            continue;
        };
        let lat = if field(Some(c_lat_dir)) == Some("S") { -lat } else { lat };
        let lon = if field(Some(c_lon_dir)) == Some("W") { -lon } else { lon };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            unplaced += 1;
            continue;
        }
        let impact_kt = num(c_impact);
        let alt_km = num(c_alt);
        let vel = num(c_vel);

        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("impact_energy_kt", serde_json::json!(impact_kt));
        put("radiated_energy_1e10_j", serde_json::json!(num(c_energy)));
        put("peak_altitude_km", serde_json::json!(alt_km));
        put("velocity_kms", serde_json::json!(vel));
        if let (Some(vx), Some(vy), Some(vz)) = (num(c_vx), num(c_vy), num(c_vz)) {
            put("velocity_ecef_kms", serde_json::json!([vx, vy, vz]));
        }
        put("detected", serde_json::json!(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));

        let label = match impact_kt {
            Some(kt) if kt >= 1.0 => format!("Fireball, {kt:.1} kt"),
            Some(kt) => format!("Fireball, {:.0} t", kt * 1000.0),
            None => "Fireball".to_string(),
        };
        observations.push(
            Observation::new(source_id.clone(), EntityId::new(EntityKind::Event, format!("cneos:{}", at.format("%Y%m%dT%H%M%S"))), at.min(now), Quality::Live)
                .with_position(Position { lon, lat, alt_m: alt_km.map(|km| km * 1000.0), datum: AltitudeDatum::Geoid })
                .with_label(label)
                .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    Ok(Decoded { observations, unplaced, before_floor })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"{"signature":{"source":"NASA/JPL Fireball Data API","version":"1.2"},"count":"3","fields":["date","energy","impact-e","lat","lat-dir","lon","lon-dir","alt","vel","vx","vy","vz"],"data":[
      ["2026-09-15 11:26:13","2.2","0.079","37.6","S","161.6","W","37.0",null,null,null,null],
      ["2013-02-15 03:20:33","375000","440","54.8","N","61.1","E","23.3","18.6","12.8","-13.3","-2.4"],
      ["2026-09-11 10:18:03","6.1","0.2",null,null,null,null,null,null,null,null,null],
      ["1988-04-15 03:03:10","1.0","0.04","10.0","N","20.0","E",null,null,null,null,null]
    ]}"#;

    #[test]
    fn a_fireball_is_an_event_at_its_second_with_hemispheres_applied() {
        let feed: Feed = serde_json::from_str(FEED).unwrap();
        let d = decode(&feed, &SourceId::new("cneos-fireballs"), Utc::now()).unwrap();
        assert_eq!(d.unplaced, 1);
        assert_eq!(d.before_floor, 1, "1988 is before the store's floor");
        assert_eq!(d.observations.len(), 2);
        let small = &d.observations[0];
        assert_eq!(small.entity.key, "cneos:20260915T112613");
        let p = small.position.unwrap();
        assert!((p.lat + 37.6).abs() < 1e-9 && (p.lon + 161.6).abs() < 1e-9, "S and W are negative");
        assert_eq!(p.alt_m, Some(37000.0));
        assert_eq!(small.label.as_deref(), Some("Fireball, 79 t"));
        let chelyabinsk = &d.observations[1];
        assert_eq!(chelyabinsk.label.as_deref(), Some("Fireball, 440.0 kt"));
        assert_eq!(chelyabinsk.attrs["velocity_kms"], 18.6);
        assert_eq!(chelyabinsk.attrs["velocity_ecef_kms"], serde_json::json!([12.8, -13.3, -2.4]));
        assert_eq!(chelyabinsk.observed_at, "2013-02-15T03:20:33Z".parse::<DateTime<Utc>>().unwrap());
    }
}
