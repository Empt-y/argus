//! Active fire detections from NASA FIRMS: every hot pixel the three
//! VIIRS instruments saw in the last day, worldwide.
//!
//! FIRMS (Fire Information for Resource Management System) serves near
//! real-time detections from VIIRS on Suomi NPP, NOAA-20 and NOAA-21 —
//! 375 m pixels, each satellite covering the whole Earth twice a day —
//! through an area API that answers a CSV for a region and a day count.
//! `world` and one day is about 70,000 rows and 5.5 MB per satellite, in
//! two seconds. The three together were about 200,000 detections on the
//! September day this was written, most of them agricultural burning
//! across Africa and central Asia, and the whole set is fetched every half
//! hour because new passes are processed within about three hours of the
//! overpass and there is no way to ask for "since".
//!
//! ## What a detection is
//!
//! A pixel whose thermal signature says something in it is burning: a
//! wildfire, a gas flare, a field being cleared, a steelworks. The
//! instrument cannot tell which, and the layer does not pretend to. Each
//! carries its brightness temperature, fire radiative power, and the
//! algorithm's own confidence (`l`, `n`, `h`), dated by the acquisition
//! time to the minute. A [`EntityKind::Event`]: what was seen, when.
//!
//! FIRMS has no id for a detection. The key is the satellite, position
//! and acquisition time, which is unique within a day's file; the driver
//! remembers what it has written for a day and emits only what is new, so
//! a poll after a quiet half hour writes nothing. A restart re-emits the
//! day once.
//!
//! ## The key
//!
//! Needs a MAP_KEY, free from firms.modaps.eosdis.nasa.gov, distinct from
//! an Earthdata token, in the URL path. 5,000 transactions per ten
//! minutes; a world day costs about thirty-six, so a poll is about 110.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Quota, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::collections::HashMap;
use std::sync::Mutex;

/// The key the config supplies under `[sources.firms] credentials`.
pub const MAP_KEY: &str = "map_key";

const AREA_URL: &str = "https://firms.modaps.eosdis.nasa.gov/api/area/csv";

/// The near-real-time VIIRS products, one per satellite.
const PRODUCTS: [&str; 3] = ["VIIRS_SNPP_NRT", "VIIRS_NOAA20_NRT", "VIIRS_NOAA21_NRT"];

/// Half an hour: new passes land every few hours and the whole day is
/// re-read each time, so this is the cost knob.
const CADENCE_SECS: u64 = 1800;

/// How long a detection's key is remembered, so a day's re-read writes
/// only what is new. Longer than the day the API answers for.
const REMEMBER: Duration = Duration::hours(36);

pub struct Fires {
    descriptor: SourceDescriptor,
    http: HttpClient,
    map_key: Option<String>,
    /// Detection key → when it was first written.
    seen: Mutex<HashMap<String, DateTime<Utc>>>,
}

impl Fires {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("firms"),
                layer_id: LayerId::new("fires"),
                display_name: "Active fires (NASA FIRMS, VIIRS)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::Required { config_key: MAP_KEY.into() },
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "NASA FIRMS".into(),
                    url: "https://firms.modaps.eosdis.nasa.gov/".into(),
                    license: "Public domain (NASA); attribution requested".into(),
                    notice: Some("We acknowledge the use of data and imagery from NASA's Fire Information for Resource Management System (FIRMS), part of NASA's Earth Science Data and Information System (ESDIS)".into()),
                },
                base_quality: Quality::Live,
                quota: Some(Quota {
                    limit: 5_000,
                    window: std::time::Duration::from_secs(600),
                    cost_per_poll: 120,
                }),
            },
            http,
            map_key: None,
            seen: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_map_key(mut self, key: Option<String>) -> Self {
        self.map_key = key.filter(|k| !k.trim().is_empty());
        self
    }

    /// Keep only detections not written before, and remember them.
    fn new_only(&self, obs: Vec<Observation>, now: DateTime<Utc>) -> Vec<Observation> {
        let mut seen = self.seen.lock().expect("seen lock poisoned");
        seen.retain(|_, t| now - *t < REMEMBER);
        obs.into_iter()
            .filter(|o| {
                if seen.contains_key(&o.entity.key) {
                    return false;
                }
                seen.insert(o.entity.key.clone(), now);
                true
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl Source for Fires {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let key = self.map_key.as_deref().ok_or_else(|| SourceError::Auth("no FIRMS map_key configured".into()))?;
        let now = Utc::now();
        let mut all = Vec::new();
        let mut failed = 0;
        let mut last_error = None;
        for product in PRODUCTS {
            let url = format!("{AREA_URL}/{key}/{product}/world/1");
            match self.http.get_bytes(&url).await {
                Ok(bytes) => match decode(&bytes, &self.descriptor.id) {
                    Ok(obs) => all.extend(obs),
                    Err(err) => {
                        failed += 1;
                        tracing::warn!(source = %self.descriptor.id, product, %err, "product did not decode");
                        last_error = Some(err);
                    }
                },
                Err(err) => {
                    failed += 1;
                    // The key never appears in a log line: the URL is not
                    // logged, only which product failed.
                    tracing::warn!(source = %self.descriptor.id, product, %err, "product failed");
                    last_error = Some(err);
                }
            }
        }
        if all.is_empty()
            && let Some(err) = last_error
        {
            return Err(err);
        }
        let total = all.len();
        let new = self.new_only(all, now);
        tracing::info!(source = %self.descriptor.id, detections = total, new = new.len(), failed, "fires read");
        Ok(new)
    }
}

// --- wire format -----------------------------------------------------------

/// The CSV: `latitude,longitude,bright_ti4,scan,track,acq_date,acq_time,
/// satellite,instrument,confidence,version,bright_ti5,frp,daynight`.
/// `acq_time` is HHMM as an unpadded integer, so `7` is 00:07.
pub fn decode(bytes: &[u8], source_id: &SourceId) -> Result<Vec<Observation>, SourceError> {
    let text = std::str::from_utf8(bytes).map_err(|e| SourceError::Decode(e.to_string()))?;
    if text.trim_start().starts_with('<') || text.starts_with("Invalid") {
        // FIRMS answers a bad key or an exhausted quota with a sentence,
        // not a status code.
        return Err(SourceError::Auth(format!("FIRMS said: {}", text.lines().next().unwrap_or("").chars().take(120).collect::<String>())));
    }
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(text.as_bytes());
    let headers = reader.headers().map_err(|e| SourceError::Decode(e.to_string()))?.clone();
    let col = |name: &str| headers.iter().position(|h| h == name);
    let (Some(c_lat), Some(c_lon), Some(c_date), Some(c_time)) = (col("latitude"), col("longitude"), col("acq_date"), col("acq_time")) else {
        return Err(SourceError::Decode(format!("FIRMS CSV without the expected columns: {}", headers.iter().collect::<Vec<_>>().join(","))));
    };
    let c_ti4 = col("bright_ti4");
    let c_ti5 = col("bright_ti5");
    let c_frp = col("frp");
    let c_scan = col("scan");
    let c_track = col("track");
    let c_sat = col("satellite");
    let c_inst = col("instrument");
    let c_conf = col("confidence");
    let c_dn = col("daynight");
    let c_ver = col("version");

    let mut out = Vec::new();
    for record in reader.records() {
        let r = record.map_err(|e| SourceError::Decode(e.to_string()))?;
        let field = |c: Option<usize>| c.and_then(|i| r.get(i)).map(str::trim).filter(|s| !s.is_empty());
        let num = |c: Option<usize>| field(c).and_then(|s| s.parse::<f64>().ok());
        let (Some(lat), Some(lon)) = (num(Some(c_lat)), num(Some(c_lon))) else { continue };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            continue;
        }
        let Some(date) = field(Some(c_date)).and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()) else { continue };
        let Some(hhmm) = field(Some(c_time)).and_then(|t| t.parse::<u32>().ok()) else { continue };
        let (hour, minute) = (hhmm / 100, hhmm % 100);
        let Some(at) = date.and_hms_opt(hour, minute, 0) else { continue };
        let at = at.and_utc();
        let satellite = field(c_sat).unwrap_or("?");
        let sat_name = match satellite {
            "N" => "Suomi NPP",
            "1" => "NOAA-20",
            "2" => "NOAA-21",
            other => other,
        };
        let confidence = field(c_conf);
        let daynight = field(c_dn);

        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("satellite", serde_json::json!(sat_name));
        put("instrument", serde_json::json!(field(c_inst)));
        put("brightness_k", serde_json::json!(num(c_ti4)));
        put("brightness_ti5_k", serde_json::json!(num(c_ti5)));
        put("frp_mw", serde_json::json!(num(c_frp)));
        put("scan_km", serde_json::json!(num(c_scan)));
        put("track_km", serde_json::json!(num(c_track)));
        put("confidence", serde_json::json!(confidence));
        put("daynight", serde_json::json!(daynight));
        put("version", serde_json::json!(field(c_ver)));
        put("acquired", serde_json::json!(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));

        let frp = num(c_frp);
        let label = match (frp, confidence) {
            (Some(f), Some("h")) => format!("Fire, {f:.0} MW, high confidence"),
            (Some(f), Some("l")) => format!("Hot spot, {f:.0} MW, low confidence"),
            (Some(f), _) => format!("Fire, {f:.0} MW"),
            (None, _) => "Fire".to_string(),
        };
        let key = format!("firms:{satellite}:{lat:.5}:{lon:.5}:{}:{hhmm:04}", date.format("%Y%m%d"));
        out.push(
            Observation::new(source_id.clone(), EntityId::new(EntityKind::Event, key), at, Quality::Live)
                .with_position(Position { lon, lat, alt_m: None, datum: AltitudeDatum::AboveGround })
                .with_label(label)
                .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "latitude,longitude,bright_ti4,scan,track,acq_date,acq_time,satellite,instrument,confidence,version,bright_ti5,frp,daynight
62.38906,76.63682,308.3,0.78,0.78,2026-09-16,7,N,VIIRS,n,2.0NRT,278.53,2.07,N
51.68335,-5.03251,299,0.54,0.51,2026-09-16,156,N,VIIRS,l,2.0NRT,285.95,1.35,N
-3.5,30.2,367.0,0.4,0.4,2026-09-16,1230,1,VIIRS,h,2.0NRT,300.1,45.8,D
";

    #[test]
    fn a_detection_is_an_event_at_its_acquisition_minute() {
        let obs = decode(CSV.as_bytes(), &SourceId::new("fires")).unwrap();
        assert_eq!(obs.len(), 3);
        let first = &obs[0];
        assert_eq!(first.observed_at, "2026-09-16T00:07:00Z".parse::<DateTime<Utc>>().unwrap(), "acq_time 7 is 00:07, not 07:00");
        assert_eq!(first.entity.key, "firms:N:62.38906:76.63682:20260916:0007");
        assert_eq!(first.attrs["satellite"], "Suomi NPP");
        assert_eq!(first.attrs["frp_mw"], 2.07);
        assert_eq!(obs[1].observed_at, "2026-09-16T01:56:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(obs[1].label.as_deref(), Some("Hot spot, 1 MW, low confidence"));
        assert_eq!(obs[2].label.as_deref(), Some("Fire, 46 MW, high confidence"));
        assert_eq!(obs[2].attrs["satellite"], "NOAA-20");
    }

    #[test]
    fn a_bad_key_is_an_auth_error_not_a_decode_error() {
        let err = decode(b"Invalid MAP_KEY.", &SourceId::new("fires")).unwrap_err();
        assert!(matches!(err, SourceError::Auth(_)), "{err}");
    }

    #[test]
    fn a_re_read_day_writes_only_what_is_new() {
        let http = HttpClient::new(std::time::Duration::from_secs(5)).unwrap();
        let f = Fires::new(http);
        let now = Utc::now();
        let first = f.new_only(decode(CSV.as_bytes(), &SourceId::new("fires")).unwrap(), now);
        assert_eq!(first.len(), 3);
        let again = f.new_only(decode(CSV.as_bytes(), &SourceId::new("fires")).unwrap(), now + Duration::minutes(30));
        assert!(again.is_empty());
        let later = f.new_only(decode(CSV.as_bytes(), &SourceId::new("fires")).unwrap(), now + REMEMBER + Duration::minutes(1));
        assert_eq!(later.len(), 3, "forgotten after the window");
    }
}
