//! Geomagnetic activity over the UK, from AuroraWatch UK's magnetometers.
//!
//! AuroraWatch UK (Lancaster University) runs the alert people follow for
//! aurora over Britain: green, yellow, amber, red, decided every hour from
//! the disturbance a magnetometer measures in nanotesla against thresholds
//! of 50, 100 and 200. It is a national scalar — the data-source log
//! parked it for that reason — but it is measured somewhere: the alerting
//! site is the Sumburgh Head magnetometer on Shetland, and the API defines
//! 26 sites across three projects with a position each. Each site that
//! publishes an activity document is a station here, carrying its last 24
//! hourly readings, and the alerting one carries the national alert level
//! and what it means for seeing aurora. So the scalar has a place on the
//! globe: the instrument that decides it.
//!
//! What the whole network says, read before this was designed: of the 26
//! sites defined, five have an activity document at all (the API 404s the
//! rest, hence [`HttpClient::get_bytes_if_present`]), and of those five
//! only Sumburgh Head was current — Crooktree a month behind, Lancaster
//! two years, Sidmouth eight. They are emitted as they are; a station whose
//! last document is from 2018 is dated 2018, and the 24-hour station
//! horizon keeps it off the live view. Observed-at is the document's
//! `updated` stamp — when it was assembled, minutes ago for a live site —
//! because the newest hour's value is a running one, revised as the hour
//! goes on; dating the row by the start of that hour would read as a lag
//! of an hour on a feed that is not late.
//!
//! The site list is read from the project documents every poll rather than
//! from the API's directory listings, which are nginx HTML. Requests per
//! poll: three project files, one all-site status, and one activity
//! document per defined site — about thirty small XML fetches every
//! fifteen minutes, against a document that changes hourly.
//! CC BY-NC-SA 3.0.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::geo::BoundingBox;
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

const API: &str = "http://aurorawatch-api.lancs.ac.uk/0.2";
/// The three magnetometer projects the API defines.
const PROJECTS: &[&str] = &["awn", "samnet", "bgs_sch"];
/// The documents update at the end of each hour.
const CADENCE_SECS: u64 = 15 * 60;

const UNITED_KINGDOM: BoundingBox = BoundingBox {
    west: -8.7,
    south: 49.8,
    east: 1.8,
    north: 60.9,
};

pub struct AuroraWatch {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl AuroraWatch {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("aurorawatch"),
                layer_id: LayerId::new("geomagnetic-activity"),
                display_name: "Geomagnetic activity (AuroraWatch UK)".into(),
                kind: EntityKind::Station,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Fixed { bbox: UNITED_KINGDOM },
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "AuroraWatch UK, Lancaster University".into(),
                    url: "https://aurorawatch.lancs.ac.uk/".into(),
                    license: "CC BY-NC-SA 3.0".into(),
                    notice: Some("Geomagnetic data from AuroraWatch UK, Space and Plasma Physics group, Lancaster University (CC BY-NC-SA 3.0)".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for AuroraWatch {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let status = self.http.get_bytes(&format!("{API}/status/all-site-status.xml")).await?;
        let alert = decode_alert(&status)?;
        let mut sites = Vec::new();
        for project in PROJECTS {
            let bytes = self.http.get_bytes(&format!("{API}/project/{project}.xml")).await?;
            sites.extend(decode_project(&bytes, project)?);
        }
        let mut observations = Vec::new();
        let mut silent = 0;
        for site in &sites {
            let url = format!("{API}/status/project/{}/{}-activity.xml", site.project, site.abbreviation.to_lowercase());
            let Some(bytes) = self.http.get_bytes_if_present(&url).await? else {
                silent += 1;
                continue;
            };
            match decode_activity(&bytes) {
                Ok(activity) => observations.push(observation(site, &activity, &alert, &self.descriptor.id)),
                Err(err) => tracing::warn!(source = %self.descriptor.id, site = %site.id, %err, "activity document did not decode"),
            }
        }
        if observations.is_empty() {
            return Err(SourceError::Decode(format!("none of {} sites published an activity document", sites.len())));
        }
        tracing::info!(source = %self.descriptor.id, sites = sites.len(), publishing = observations.len(), silent, alert = %alert.status, "geomagnetic activity read");
        Ok(observations)
    }
}

// --- wire format --------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CurrentStatus {
    #[serde(rename = "site_status", default)]
    sites: Vec<SiteStatus>,
}

#[derive(Debug, Deserialize)]
struct SiteStatus {
    #[serde(rename = "@site_id")]
    site_id: String,
    #[serde(rename = "@status_id")]
    status_id: String,
    #[serde(rename = "@alerting", default)]
    alerting: Option<String>,
}

/// The national alert as the API states it: the level, and which site
/// decides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub status: String,
    pub alerting_site: String,
}

pub fn decode_alert(bytes: &[u8]) -> Result<Alert, SourceError> {
    let text = std::str::from_utf8(bytes).map_err(|e| SourceError::Decode(format!("not UTF-8: {e}")))?;
    let doc: CurrentStatus = quick_xml::de::from_str(text).map_err(|e| SourceError::Decode(format!("all-site-status: {e}")))?;
    // The alerting site is flagged; with one site listed, it is that one.
    let site = doc
        .sites
        .iter()
        .find(|s| s.alerting.as_deref() == Some("true"))
        .or_else(|| (doc.sites.len() == 1).then(|| &doc.sites[0]))
        .ok_or_else(|| SourceError::Decode("all-site-status names no alerting site".into()))?;
    Ok(Alert { status: site.status_id.clone(), alerting_site: site.site_id.clone() })
}

#[derive(Debug, Deserialize)]
struct Project {
    #[serde(rename = "site", default)]
    sites: Vec<ProjectSite>,
}

#[derive(Debug, Deserialize)]
struct ProjectSite {
    #[serde(rename = "@id")]
    id: String,
    #[serde(default)]
    abbreviation: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    latitude: String,
    #[serde(default)]
    longitude: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    start_datetime: Option<Stamp>,
    #[serde(default)]
    end_datetime: Option<Stamp>,
}

#[derive(Debug, Default, Deserialize)]
struct Stamp {
    #[serde(default)]
    datetime: String,
}

/// A magnetometer site as the project document defines it.
#[derive(Debug, Clone, PartialEq)]
pub struct Site {
    pub id: String,
    pub project: String,
    pub abbreviation: String,
    pub location: String,
    pub lat: f64,
    pub lon: f64,
    pub description: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
}

pub fn decode_project(bytes: &[u8], project: &str) -> Result<Vec<Site>, SourceError> {
    let text = std::str::from_utf8(bytes).map_err(|e| SourceError::Decode(format!("not UTF-8: {e}")))?;
    let doc: Project = quick_xml::de::from_str(text).map_err(|e| SourceError::Decode(format!("project {project}: {e}")))?;
    let nonempty = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    let sites = doc
        .sites
        .into_iter()
        .filter_map(|s| {
            let lat = s.latitude.trim().parse::<f64>().ok()?;
            let lon = s.longitude.trim().parse::<f64>().ok()?;
            let abbreviation = nonempty(&s.abbreviation)?;
            Some(Site {
                id: s.id,
                project: project.to_string(),
                abbreviation,
                location: s.location.trim().to_string(),
                lat,
                lon,
                description: s.description.as_deref().and_then(nonempty),
                since: s.start_datetime.as_ref().and_then(|d| nonempty(&d.datetime)),
                until: s.end_datetime.as_ref().and_then(|d| nonempty(&d.datetime)),
            })
        })
        .collect::<Vec<_>>();
    if sites.is_empty() {
        return Err(SourceError::Decode(format!("project {project} defines no placed site")));
    }
    Ok(sites)
}

#[derive(Debug, Deserialize)]
struct SiteActivity {
    #[serde(default)]
    updated: Option<Stamp>,
    #[serde(rename = "lower_threshold", default)]
    thresholds: Vec<Threshold>,
    #[serde(rename = "activity", default)]
    readings: Vec<Reading>,
}

#[derive(Debug, Deserialize)]
struct Threshold {
    #[serde(rename = "@status_id")]
    status_id: String,
    #[serde(rename = "$text", default)]
    value: String,
}

#[derive(Debug, Deserialize)]
struct Reading {
    #[serde(rename = "@status_id", default)]
    status_id: String,
    #[serde(default)]
    datetime: String,
    #[serde(default)]
    value: String,
}

/// One site's last day of hourly activity.
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub updated: DateTime<Utc>,
    /// `(status, nanotesla)` from the lowest level up.
    pub thresholds: Vec<(String, f64)>,
    /// `(hour, nanotesla, status)`, oldest first.
    pub hours: Vec<(DateTime<Utc>, f64, String)>,
}

/// The API writes `2026-09-17T11:59:59+0000`, an offset without a colon,
/// which RFC 3339 parsing refuses.
fn stamp(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%z") {
        return Some(t.with_timezone(&Utc));
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok().map(|t| t.and_utc())
}

pub fn decode_activity(bytes: &[u8]) -> Result<Activity, SourceError> {
    let text = std::str::from_utf8(bytes).map_err(|e| SourceError::Decode(format!("not UTF-8: {e}")))?;
    let doc: SiteActivity = quick_xml::de::from_str(text).map_err(|e| SourceError::Decode(format!("site activity: {e}")))?;
    let mut hours: Vec<(DateTime<Utc>, f64, String)> = doc
        .readings
        .iter()
        .filter_map(|r| Some((stamp(&r.datetime)?, r.value.trim().parse::<f64>().ok()?, r.status_id.clone())))
        .collect();
    hours.sort_by_key(|h| h.0);
    if hours.is_empty() {
        return Err(SourceError::Decode("activity document has no readings".into()));
    }
    let updated = doc
        .updated
        .as_ref()
        .and_then(|u| stamp(&u.datetime))
        .unwrap_or_else(|| hours.last().map(|h| h.0).unwrap_or_else(Utc::now));
    let mut thresholds: Vec<(String, f64)> = doc
        .thresholds
        .iter()
        .filter_map(|t| Some((t.status_id.clone(), t.value.trim().parse::<f64>().ok()?)))
        .collect();
    thresholds.sort_by(|a, b| a.1.total_cmp(&b.1));
    Ok(Activity { updated, thresholds, hours })
}

pub fn observation(site: &Site, activity: &Activity, alert: &Alert, source_id: &SourceId) -> Observation {
    let (hour, value, status) = activity.hours.last().cloned().expect("decode_activity refuses an empty document");
    let alerting = alert.alerting_site == site.id;
    let peak = activity.hours.iter().map(|h| h.1).fold(f64::MIN, f64::max);
    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("site", serde_json::json!(site.abbreviation));
    put("location", serde_json::json!(site.location));
    put("project", serde_json::json!(site.project.to_uppercase()));
    put("description", serde_json::json!(site.description));
    put("since", serde_json::json!(site.since));
    put("until", serde_json::json!(site.until));
    put("activity_nt", serde_json::json!(value));
    put("hour", serde_json::json!(hour.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
    put("status", serde_json::json!(status));
    put("peak_24h_nt", serde_json::json!(peak));
    put("alerting", serde_json::json!(alerting.then_some(true)));
    put("alert_level", serde_json::json!(alerting.then(|| alert.status.clone())));
    put("thresholds_nt", serde_json::json!(activity.thresholds.iter().map(|(s, v)| serde_json::json!({"status": s, "from_nt": v})).collect::<Vec<_>>()));
    put(
        "hours",
        serde_json::json!(activity.hours.iter().map(|(t, v, s)| serde_json::json!({"hour": t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true), "nt": v, "status": s})).collect::<Vec<_>>()),
    );
    put("url", serde_json::json!(format!("https://aurorawatch.lancs.ac.uk/summary/{}/{}/", site.project, site.abbreviation.to_lowercase())));
    let label = if alerting { format!("{} (AuroraWatch UK: {})", site.location.trim_end_matches(", UK"), alert.status) } else { site.location.trim_end_matches(", UK").to_string() };
    Observation::new(source_id.clone(), EntityId::new(EntityKind::Station, format!("{}:{}", site.project, site.abbreviation)), activity.updated, Quality::Live)
        .with_position(Position { lon: site.lon, lat: site.lat, alt_m: None, datum: AltitudeDatum::Geoid })
        .with_label(label)
        .with_attrs(serde_json::Value::Object(attrs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes'?>
<!DOCTYPE current_status PUBLIC "-//AuroraWatch-API//DTD REST 0.2.5//EN" "http://aurorawatch-api.lancs.ac.uk/0.2.5/aurorawatch-api.dtd">
<current_status api_version="0.2.5"><updated><datetime>2026-09-17T11:59:59+0000</datetime></updated><site_status alerting="true" project_id="project:AWN" site_id="site:AWN:SUM" site_url="http://aurorawatch-api.lancs.ac.uk/0.2.5/project/awn/sum.xml" status_id="green"/></current_status>"#;

    const PROJECT: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes'?>
<!DOCTYPE project PUBLIC "-//AuroraWatch-API//DTD REST 0.2.5//EN" "http://aurorawatch-api.lancs.ac.uk/0.2/aurorawatch-api.dtd">
<project api_version="0.2.5" id="project:AWN" url="http://aurorawatch-api.lancs.ac.uk/0.2/project/awn.xml"><name>AuroraWatch Magnetometer Network</name><abbreviation>AWN</abbreviation><url>http://aurorawatch.lancs.ac.uk/project-info/aurorawatchnet/</url><site id="site:AWN:SUM" project_id="project:AWN" url="http://aurorawatch-api.lancs.ac.uk/0.2/project/awn/sum.xml"><location>Sumburgh Head, UK</location><abbreviation>SUM</abbreviation><latitude>59.853</latitude><longitude>-1.276</longitude><start_datetime><datetime>2017-08-01T00:00</datetime></start_datetime><description lang="en">Raspberry Pi magnetometer system, hosted by the Shetland Amenity Trust.</description><copyright lang="en">Lancaster University.</copyright><data_type id="data_type:AWN:SUM:MagData" project_id="project:AWN" site_id="site:AWN:SUM" type="MagData"><description lang="en">Magnetic field</description></data_type></site><site id="site:AWN:ORM" project_id="project:AWN" url="x"><location>Ormskirk, UK</location><abbreviation>ORM</abbreviation><latitude>53.569195</latitude><longitude>-2.887264</longitude><start_datetime><datetime>2013-08-01T00:00</datetime></start_datetime><end_datetime><datetime>2017-04-14T00:00</datetime></end_datetime></site><site id="site:AWN:BAD" project_id="project:AWN" url="x"><location>Nowhere</location><abbreviation>BAD</abbreviation><latitude></latitude><longitude></longitude></site></project>"#;

    const ACTIVITY: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes'?>
<!DOCTYPE site_activity PUBLIC "-//AuroraWatch-API//DTD REST 0.2.5//EN" "http://aurorawatch-api.lancs.ac.uk/0.2.5/aurorawatch-api.dtd">
<site_activity api_version="0.2.5" project_id="project:AWN" site_id="site:AWN:SUM" site_url="http://aurorawatch-api.lancs.ac.uk/0.2.5/project/awn/sum.xml"><lower_threshold status_id="green">0</lower_threshold><lower_threshold status_id="yellow">50</lower_threshold><lower_threshold status_id="amber">100</lower_threshold><lower_threshold status_id="red">200</lower_threshold><updated><datetime>2026-09-17T11:59:59+0000</datetime></updated><activity status_id="green"><datetime>2026-09-17T09:00:00+0000</datetime><value>20.3</value></activity><activity status_id="yellow"><datetime>2026-09-17T10:00:00+0000</datetime><value>78.6</value></activity><activity status_id="green"><datetime>2026-09-17T11:00:00+0000</datetime><value>33.7</value></activity></site_activity>"#;

    #[test]
    fn the_alert_names_its_level_and_the_site_that_decides_it() {
        let a = decode_alert(STATUS.as_bytes()).unwrap();
        assert_eq!(a, Alert { status: "green".into(), alerting_site: "site:AWN:SUM".into() });
    }

    #[test]
    fn a_project_lists_its_placed_sites_and_a_closed_one_keeps_its_end_date() {
        let sites = decode_project(PROJECT.as_bytes(), "awn").unwrap();
        assert_eq!(sites.len(), 2, "the site with no coordinates is left out");
        assert_eq!(sites[0].abbreviation, "SUM");
        assert!((sites[0].lat - 59.853).abs() < 1e-6);
        assert_eq!(sites[0].description.as_deref(), Some("Raspberry Pi magnetometer system, hosted by the Shetland Amenity Trust."));
        assert_eq!(sites[1].until.as_deref(), Some("2017-04-14T00:00"));
    }

    #[test]
    fn a_stations_observation_is_the_running_hour_dated_when_the_document_was_assembled() {
        let sites = decode_project(PROJECT.as_bytes(), "awn").unwrap();
        let activity = decode_activity(ACTIVITY.as_bytes()).unwrap();
        assert_eq!(activity.hours.len(), 3);
        assert_eq!(activity.thresholds, vec![("green".to_string(), 0.0), ("yellow".to_string(), 50.0), ("amber".to_string(), 100.0), ("red".to_string(), 200.0)]);
        let alert = decode_alert(STATUS.as_bytes()).unwrap();
        let o = observation(&sites[0], &activity, &alert, &SourceId::new("aurorawatch"));
        assert_eq!(o.entity.key, "awn:SUM", "the project is in the key: SAMNET and the BGS schools both have a LAN2");
        assert_eq!(o.entity.kind, EntityKind::Station);
        assert_eq!(o.observed_at.to_rfc3339(), "2026-09-17T11:59:59+00:00", "the +0000 offset parses");
        assert_eq!(o.label.as_deref(), Some("Sumburgh Head (AuroraWatch UK: green)"));
        assert_eq!(o.attrs["activity_nt"], 33.7);
        assert_eq!(o.attrs["hour"], "2026-09-17T11:00:00Z");
        assert_eq!(o.attrs["status"], "green");
        assert_eq!(o.attrs["peak_24h_nt"], 78.6);
        assert_eq!(o.attrs["alerting"], true);
        assert_eq!(o.attrs["alert_level"], "green");
        assert_eq!(o.attrs["hours"].as_array().unwrap().len(), 3);
        assert_eq!(o.attrs["hours"][1]["status"], "yellow");
        let other = observation(&sites[1], &activity, &alert, &SourceId::new("aurorawatch"));
        assert!(other.attrs.get("alerting").is_none() && other.attrs.get("alert_level").is_none());
        assert_eq!(other.label.as_deref(), Some("Ormskirk"));
    }

    #[test]
    fn an_activity_document_without_readings_is_refused() {
        let empty = ACTIVITY.replace(r#"<activity status_id="green"><datetime>2026-09-17T09:00:00+0000</datetime><value>20.3</value></activity><activity status_id="yellow"><datetime>2026-09-17T10:00:00+0000</datetime><value>78.6</value></activity><activity status_id="green"><datetime>2026-09-17T11:00:00+0000</datetime><value>33.7</value></activity>"#, "");
        assert!(decode_activity(empty.as_bytes()).is_err());
    }
}
