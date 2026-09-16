//! Road disruptions on London's red routes, from Transport for London's
//! Unified API.
//!
//! TfL manages the 580 km of major roads it calls the Transport for London
//! Road Network, and publishes every disruption on them: roadworks, utility
//! works, collisions, breakdowns, planned closures. 131 in one pull, 126 of
//! them works. Each has a point; 32 of the 131 also carried a polygon of
//! the affected area, which is kept as the geometry where it exists. Greater
//! London only, which is the coverage declared.
//!
//! ## Dated by the last update, as NWS alerts are dated by issue
//!
//! A disruption is an event that lasts weeks — the median works ran a month
//! — and the `Event` horizon is seven days, so dating it by its start would
//! hide most of the layer. TfL touches every active record daily: all 131
//! carried a `lastModifiedTime` from the day of the pull. That is the issue
//! time of the record as it now stands, and it is what the observation is
//! dated by; a record TfL stops touching leaves the map a week later, which
//! is what an event feed's horizon means.
//!
//! Two of 131 had already ended and are dropped, as expired SIGMETs are.
//! Three had not yet started: planned closures, kept and marked, because a
//! closure announced for next week is exactly the kind of thing a person
//! looks at a road map to learn.

use crate::geojson::{self, GeoJsonGeometry};
use crate::http::HttpClient;
use argus_core::BoundingBox;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

const FEED_URL: &str = "https://api.tfl.gov.uk/Road/all/Disruption";

/// Updates arrive through the working day; five minutes sees a collision
/// while it is still blocking the road, and the anonymous allowance is far
/// above twelve requests an hour.
const CADENCE_SECS: u64 = 300;

pub struct TflRoadDisruptions {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl TflRoadDisruptions {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("tfl-road-disruptions"),
                layer_id: LayerId::new("road-disruptions"),
                display_name: "Road disruptions (Transport for London)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Fixed {
                    bbox: BoundingBox::new(-0.52, 51.28, 0.34, 51.70),
                },
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "Transport for London".into(),
                    url: "https://tfl.gov.uk/".into(),
                    license: "TfL Open Data licence (OGL-based)".into(),
                    notice: Some("Powered by TfL Open Data. Contains OS data © Crown copyright and database rights".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for TflRoadDisruptions {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let feed: Vec<Disruption> = self.http.get_json(FEED_URL).await?;
        Ok(decode(feed, &self.descriptor.id, Utc::now()))
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Disruption {
    id: Option<String>,
    /// `"[lon,lat]"` — a JSON array inside a string.
    point: Option<String>,
    severity: Option<String>,
    category: Option<String>,
    #[serde(rename = "subCategory")]
    sub_category: Option<String>,
    comments: Option<String>,
    #[serde(rename = "currentUpdate")]
    current_update: Option<String>,
    #[serde(rename = "currentUpdateDateTime")]
    current_update_at: Option<String>,
    #[serde(rename = "corridorIds", default)]
    corridor_ids: Vec<String>,
    #[serde(rename = "startDateTime")]
    start: Option<String>,
    #[serde(rename = "endDateTime")]
    end: Option<String>,
    #[serde(rename = "lastModifiedTime")]
    last_modified: Option<String>,
    #[serde(rename = "levelOfInterest")]
    level_of_interest: Option<String>,
    location: Option<String>,
    status: Option<String>,
    #[serde(rename = "isProvisional")]
    provisional: Option<bool>,
    #[serde(rename = "hasClosures")]
    has_closures: Option<bool>,
    /// The location as GeoJSON, a `Point` on every record seen.
    geography: Option<serde_json::Value>,
    /// The affected area, on a quarter of records. Polygon or MultiPolygon.
    geometry: Option<GeoJsonGeometry>,
}

fn stamp(s: &Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().and_then(|s| s.parse().ok())
}

fn text(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// The point: from `point`, a JSON array serialised inside a string, or
/// failing that from `geography` when it is a GeoJSON Point.
fn point(d: &Disruption) -> Option<(f64, f64)> {
    if let Some(p) = text(&d.point)
        && let Ok([lon, lat]) = serde_json::from_str::<[f64; 2]>(p)
    {
        return Some((lon, lat));
    }
    let g = d.geography.as_ref()?;
    if g.get("type")?.as_str()? != "Point" {
        return None;
    }
    let c = g.get("coordinates")?.as_array()?;
    Some((c.first()?.as_f64()?, c.get(1)?.as_f64()?))
}

fn decode(feed: Vec<Disruption>, source_id: &SourceId, now: DateTime<Utc>) -> Vec<Observation> {
    feed.into_iter()
        .filter_map(|d| decode_one(d, source_id, now))
        .collect()
}

fn decode_one(d: Disruption, source_id: &SourceId, now: DateTime<Utc>) -> Option<Observation> {
    let id = text(&d.id)?.to_string();
    let (lon, lat) = point(&d)?;
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
        return None;
    }
    // Over, whatever the feed still lists.
    let end = stamp(&d.end);
    if end.is_some_and(|t| t < now) {
        return None;
    }
    let start = stamp(&d.start);
    // The record as it now stands. Never the start, which for works is
    // weeks ago and for a planned closure is in the future.
    let observed_at = stamp(&d.last_modified)
        .or_else(|| stamp(&d.current_update_at))
        .unwrap_or(now)
        .min(now);

    let planned = start.is_some_and(|t| t > now);

    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    let rfc =
        |t: Option<DateTime<Utc>>| t.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    put("disruption_id", serde_json::json!(id));
    put("category", serde_json::json!(text(&d.category)));
    put("sub_category", serde_json::json!(text(&d.sub_category)));
    put("severity", serde_json::json!(text(&d.severity)));
    put("status", serde_json::json!(text(&d.status)));
    put("planned", serde_json::json!(planned));
    put(
        "level_of_interest",
        serde_json::json!(text(&d.level_of_interest)),
    );
    put("location", serde_json::json!(text(&d.location)));
    put("description", serde_json::json!(text(&d.comments)));
    put("current_update", serde_json::json!(text(&d.current_update)));
    put(
        "updated_at",
        serde_json::json!(rfc(stamp(&d.current_update_at))),
    );
    put("start", serde_json::json!(rfc(start)));
    put("end", serde_json::json!(rfc(end)));
    put("provisional", serde_json::json!(d.provisional));
    put("closures", serde_json::json!(d.has_closures));
    let corridors: Vec<&str> = d
        .corridor_ids
        .iter()
        .map(String::as_str)
        .filter(|c| !c.is_empty())
        .collect();
    if !corridors.is_empty() {
        put("corridors", serde_json::json!(corridors));
    }
    put(
        "url",
        serde_json::json!(format!(
            "https://tfl.gov.uk/traffic/status/?disruption={id}"
        )),
    );

    // What it is, then how bad, so a fence label match on "collision" or
    // "closure" works and a card reads sensibly at a glance.
    let what = text(&d.sub_category)
        .or(text(&d.category))
        .unwrap_or("Road disruption");
    let label = match text(&d.severity) {
        Some(sev) if sev != "Minimal" && sev != "No impact" => format!("{what} ({sev})"),
        _ => what.to_string(),
    };

    let mut obs = Observation::new(
        source_id.clone(),
        EntityId::new(EntityKind::Event, id),
        observed_at,
        Quality::Live,
    )
    .with_position(Position {
        lon,
        lat,
        alt_m: None,
        datum: AltitudeDatum::AboveGround,
    })
    .with_label(label)
    .with_attrs(serde_json::Value::Object(attrs));
    if let Some(area) = d.geometry.as_ref().and_then(geojson::convert) {
        obs = obs.with_geom(area);
    }
    Some(obs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> SourceId {
        SourceId::new("tfl-road-disruptions")
    }
    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    const NOW: &str = "2026-09-16T12:00:00Z";

    /// Three live records, trimmed: utility works with a point only, a
    /// serious collision with an area polygon, and works that ended
    /// yesterday. Plus one planned closure starting next week.
    const FEED: &str = r#"[
      {"$type": "Tfl.Api.Presentation.Entities.RoadDisruption, Tfl.Api.Presentation.Entities", "id": "TIMS-233221", "url": "/Road/All/Disruption/TIMS-233221", "point": "[0.054973,51.47111]", "severity": "Minimal", "ordinal": 1, "category": "Works", "subCategory": "Utility works", "comments": "[A205] Academy Road (All directions) - Temporary signals in place to facilitate SGN works. ", "currentUpdate": "Traffic is slow moving on approaches.", "currentUpdateDateTime": "2026-09-16T11:27:10Z", "corridorIds": ["a205"], "startDateTime": "2026-08-11T07:53:00Z", "endDateTime": "2026-09-25T18:00:00Z", "lastModifiedTime": "2026-09-16T11:27:10Z", "levelOfInterest": "High", "location": "[A205] ACADEMY ROAD (SE18 ) (Greenwich)", "status": "Active", "geography": {"type": "Point", "coordinates": [0.054973, 51.47111]}, "isProvisional": false, "hasClosures": false, "roadDisruptionLines": [], "roadDisruptionImpactAreas": [], "recurringSchedules": []},
      {"id": "TIMS-240001", "point": "[-0.2215,51.5152]", "severity": "Serious", "category": "Collisions", "subCategory": "Vehicle collision", "comments": "[A40] Westway - Collision, lane two closed.", "currentUpdate": "", "currentUpdateDateTime": "2026-09-16T11:50:00Z", "corridorIds": ["a40"], "startDateTime": "2026-09-16T11:40:00Z", "endDateTime": "2026-09-16T14:00:00Z", "lastModifiedTime": "2026-09-16T11:50:00Z", "levelOfInterest": "High", "location": "[A40] WESTWAY (W10) (Kensington and Chelsea)", "status": "Active", "geography": {"type": "Point", "coordinates": [-0.2215, 51.5152]}, "isProvisional": false, "hasClosures": true, "geometry": {"type": "Polygon", "coordinates": [[[-0.2224, 51.5155], [-0.2223, 51.5153], [-0.2216, 51.5150], [-0.2174, 51.5153], [-0.2224, 51.5155]]]}},
      {"id": "TIMS-200000", "point": "[-0.1,51.5]", "severity": "Minimal", "category": "Works", "subCategory": "TfL works", "comments": "Finished.", "currentUpdate": "", "currentUpdateDateTime": "2026-09-15T08:00:00Z", "corridorIds": [], "startDateTime": "2026-09-01T00:00:00Z", "endDateTime": "2026-09-15T18:00:00Z", "lastModifiedTime": "2026-09-15T08:00:00Z", "levelOfInterest": "Low", "location": "SOMEWHERE", "status": "Active", "geography": {"type": "Point", "coordinates": [-0.1, 51.5]}, "isProvisional": false, "hasClosures": false},
      {"id": "TIMS-250000", "point": "[-0.05,51.48]", "severity": "Moderate", "category": "Works", "subCategory": "TfL works", "comments": "[A2] Full closure for resurfacing.", "currentUpdate": "", "currentUpdateDateTime": "2026-09-16T09:00:00Z", "corridorIds": ["a2"], "startDateTime": "2026-09-22T22:00:00Z", "endDateTime": "2026-09-23T05:00:00Z", "lastModifiedTime": "2026-09-16T09:00:00Z", "levelOfInterest": "Medium", "location": "[A2] OLD KENT ROAD", "status": "Active Long Term", "geography": {"type": "Point", "coordinates": [-0.05, 51.48]}, "isProvisional": true, "hasClosures": true}
    ]"#;

    fn feed() -> Vec<Disruption> {
        serde_json::from_str(FEED).unwrap()
    }

    #[test]
    fn an_ended_disruption_is_dropped_and_a_planned_one_is_kept_and_marked() {
        let obs = decode(feed(), &source(), at(NOW));
        assert_eq!(obs.len(), 3);
        assert!(obs.iter().all(|o| o.entity.key != "TIMS-200000"));
        let planned = obs.iter().find(|o| o.entity.key == "TIMS-250000").unwrap();
        assert_eq!(planned.attrs["planned"], serde_json::json!(true));
        assert_eq!(planned.attrs["closures"], serde_json::json!(true));
        assert_eq!(planned.attrs["provisional"], serde_json::json!(true));
        let live = obs.iter().find(|o| o.entity.key == "TIMS-233221").unwrap();
        assert_eq!(live.attrs["planned"], serde_json::json!(false));
    }

    #[test]
    fn dated_by_the_last_modification_and_never_in_the_future() {
        let obs = decode(feed(), &source(), at(NOW));
        let works = obs.iter().find(|o| o.entity.key == "TIMS-233221").unwrap();
        assert_eq!(
            works.observed_at,
            at("2026-09-16T11:27:10Z"),
            "not the August start"
        );
        // A planned closure's start is next week; its record is dated today.
        let planned = obs.iter().find(|o| o.entity.key == "TIMS-250000").unwrap();
        assert_eq!(planned.observed_at, at("2026-09-16T09:00:00Z"));
        assert!(planned.observed_at <= at(NOW));
    }

    #[test]
    fn the_point_is_read_out_of_its_string_and_the_area_is_kept_where_there_is_one() {
        let obs = decode(feed(), &source(), at(NOW));
        let works = obs.iter().find(|o| o.entity.key == "TIMS-233221").unwrap();
        let p = works.position.unwrap();
        assert_eq!((p.lon, p.lat), (0.054973, 51.47111));
        assert!(works.geom.is_none(), "a point-only record has no area");
        let crash = obs.iter().find(|o| o.entity.key == "TIMS-240001").unwrap();
        assert!(matches!(crash.geom, Some(geo_types::Geometry::Polygon(_))));
    }

    #[test]
    fn the_label_says_what_and_how_bad_unless_it_is_minimal() {
        let obs = decode(feed(), &source(), at(NOW));
        let works = obs.iter().find(|o| o.entity.key == "TIMS-233221").unwrap();
        assert_eq!(works.label.as_deref(), Some("Utility works"));
        let crash = obs.iter().find(|o| o.entity.key == "TIMS-240001").unwrap();
        assert_eq!(crash.label.as_deref(), Some("Vehicle collision (Serious)"));
        assert_eq!(crash.attrs["corridors"], serde_json::json!(["a40"]));
        assert!(
            crash.attrs.get("current_update").is_none(),
            "an empty update is no update"
        );
        assert_eq!(
            works.attrs["location"],
            serde_json::json!("[A205] ACADEMY ROAD (SE18 ) (Greenwich)")
        );
    }

    #[test]
    fn a_record_with_no_point_in_either_form_is_skipped() {
        let mut feed = feed();
        feed[0].point = Some("not json".into());
        feed[0].geography = None;
        let obs = decode(feed, &source(), at(NOW));
        assert_eq!(obs.len(), 2);
    }
}
