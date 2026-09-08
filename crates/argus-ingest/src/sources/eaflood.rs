//! Environment Agency real-time flood monitoring: warnings, and the gauges
//! behind them.
//!
//! Two sources rather than one, from one API. A flood warning is an area with a
//! severity that people act on; a river gauge is one of five and a half
//! thousand dots reporting a level every fifteen minutes. They want different
//! cadences, different zoom ranges and different layer toggles, and a client
//! that wants "tell me when my village floods" should not have to take 5,000
//! stage readings with it.
//!
//! Together they are the companion to the storm overflow layer: the same rivers,
//! measured rather than discharged into.
//!
//! ## The scalar-or-array trap
//!
//! This API is JSON-LD flattened to JSON, and the flattening does not force
//! cardinality. Where a station has two values for a property, the field is an
//! *array*; where it has one, it is a bare scalar. It is not consistent per
//! field, per station or per poll — it depends on the underlying triples.
//!
//! Measured across a live pull: `lat` and `long` are floats on 4,894 stations
//! and an array on one (E85123, which carries two positions a hundred metres
//! apart). `status` is a string on 2,570 and an array on two. `RLOIid`,
//! `catchmentName`, `dateOpened` and `label` each do the same on one or two
//! records. One reading of 5,347 has an array `value`.
//!
//! Declared as `f64`, that single station fails to deserialise — and because
//! the items arrive as one array, serde fails the *whole document*. One station
//! with two positions would cost every river gauge in England. So every field
//! that can vary is [`OneOrMany`], which is the same lesson the SIGMET driver
//! learned from one null vertex: tolerance belongs at the smallest element.
//!
//! ## The gauges are not England only
//!
//! Alongside the English river gauges the Agency publishes the National Tide
//! Gauge Network, which rings the whole UK — Aberdeen, Leith, Wick, Ullapool,
//! Tobermory, Portrush. 21 of the 5,525 stations sit outside England, as far
//! north as Lerwick. The warnings genuinely are England only, so the two
//! sources declare different coverage.
//!
//! ## Warnings could not be verified against live data
//!
//! There were no flood warnings in force in England when this was written, and
//! none recently enough to appear at `min-severity=4` either. The warning
//! decoder is therefore built against the Environment Agency's own published
//! schema rather than a captured live response, and the live test asserts the
//! shape of whatever is in force *when there is something* rather than
//! demanding warnings exist. The gauge half was verified against 5,525 real
//! stations and 5,347 real readings.

use crate::geojson;
use crate::http::HttpClient;
use argus_core::entity::{EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use argus_core::BoundingBox;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use std::collections::HashMap;

const ROOT: &str = "https://environment.data.gov.uk/flood-monitoring";

/// Flood warnings are England only — Wales and Scotland run their own warning
/// services — and the 4,208 flood areas measure out at lon -5.55..1.76,
/// lat 50.10..55.77.
const ENGLAND: BoundingBox = BoundingBox {
    west: -6.5,
    south: 49.8,
    east: 2.1,
    north: 55.9,
};

/// The gauges are *not* England only, which is not what the API's name suggests
/// and cost a red test to discover.
///
/// Alongside its English river gauges the Agency publishes the National Tide
/// Gauge Network, which rings the whole of the UK: Aberdeen, Leith, Wick,
/// Ullapool, Kinlochbervie and Tobermory in Scotland, Portrush in Northern
/// Ireland, 21 stations in all. The live extent runs to 60.15N — Lerwick — and
/// down to 49.18N in the Isles of Scilly.
///
/// Declaring England here would have been a quiet lie in the layer catalogue
/// rather than a bug that breaks anything, which is the kind that survives
/// longest.
const UK_TIDAL_AND_RIVER: BoundingBox = BoundingBox {
    west: -8.0,
    south: 49.0,
    east: 2.1,
    north: 61.0,
};

/// The Environment Agency's own words, from the `meta.licence` field every
/// response carries.
fn ea_attribution() -> Attribution {
    Attribution {
        provider: "Environment Agency".into(),
        url: "https://environment.data.gov.uk/flood-monitoring/doc/reference".into(),
        license: "Open Government Licence v3.0".into(),
        notice: Some(
            "Contains Environment Agency information licensed under the Open Government \
             Licence v3.0"
                .into(),
        ),
    }
}

// --- shared wire helpers ---------------------------------------------------

/// Every endpoint answers with the same three-part envelope.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(default = "Vec::new")]
    items: Vec<T>,
}

/// A field that is a scalar when there is one value and an array when there are
/// several. See the module docs — this is the single most important type here.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    /// The first value, which is the one to use.
    ///
    /// Deliberately not an average or a "reject the ambiguous record": where a
    /// station lists two positions they are metres apart, and picking one draws
    /// the gauge in very nearly the right place. Refusing it would lose the
    /// station over a disagreement too small to see on the map.
    fn first(&self) -> Option<&T> {
        match self {
            Self::One(v) => Some(v),
            Self::Many(v) => v.first(),
        }
    }
}

/// Parse an Environment Agency timestamp.
///
/// Readings carry a zone (`2026-09-08T14:45:00Z`); flood warnings do not
/// (`2015-02-02T19:32:00`), and the API documents its times as UTC. A naive
/// string is therefore read as UTC rather than as local time — on a machine in
/// BST, treating it as local would date every summer flood warning an hour
/// early and quietly shift it out of, or into, the freshness horizon.
fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(raw) {
        return Some(t.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .and_then(|n| Utc.from_local_datetime(&n).single())
}

// =============================================================================
// Flood warnings
// =============================================================================

/// Warnings are the urgent half. Five minutes is well inside the Agency's own
/// update rhythm without hammering a taxpayer-funded API.
const WARNING_CADENCE_SECS: u64 = 300;

/// Flood area outlines to fetch in one poll, at most.
///
/// Each is a separate request, and a widespread event can raise a hundred
/// warnings at once. The cache means this is only ever paid for areas never
/// seen before, and spreading it means a big event fills in over a few polls
/// instead of arriving as a burst of requests at a public service.
const POLYGON_FETCH_BUDGET: usize = 25;

pub struct EaFloodWarnings {
    descriptor: SourceDescriptor,
    http: HttpClient,
    areas: std::sync::Arc<dyn argus_core::GeometryCache>,
}

impl EaFloodWarnings {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("ea-flood-warnings"),
                layer_id: LayerId::new("flood-warnings"),
                display_name: "Flood warnings (Environment Agency)".into(),
                kind: EntityKind::Event,
                cadence: Cadence::every(WARNING_CADENCE_SECS),
                coverage: Coverage::Fixed { bbox: ENGLAND },
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: ea_attribution(),
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            areas: std::sync::Arc::new(argus_core::MemoryGeometryCache::new()),
        }
    }

    /// Back the flood area cache with something persistent.
    ///
    /// There are 4,208 flood areas and their outlines never change, so a
    /// deployment should fetch each one once in its life rather than once per
    /// restart.
    #[must_use]
    pub fn with_area_cache(
        mut self,
        cache: std::sync::Arc<dyn argus_core::GeometryCache>,
    ) -> Self {
        self.areas = cache;
        self
    }

    /// Resolve the outline for each warning, from cache or from the API.
    ///
    /// A warning with no outline is still emitted, positioned on the flood
    /// area's own point. A flood warning you can see the location of but not
    /// the extent of is worth far more than one that is missing entirely, and
    /// the next poll fills the shape in.
    async fn resolve_areas(&self, warnings: &mut [Pending]) -> usize {
        let mut fetched = 0usize;
        for pending in warnings.iter_mut() {
            let Some(code) = pending.area_code.clone() else {
                continue;
            };
            if let Some(geometry) = self.areas.get(&code).await {
                pending.observation = std::mem::replace(
                    &mut pending.observation,
                    placeholder(&self.descriptor.id),
                )
                .with_geom(geometry);
                continue;
            }
            if fetched >= POLYGON_FETCH_BUDGET {
                continue;
            }
            let url = pending
                .polygon_url
                .clone()
                .unwrap_or_else(|| format!("{ROOT}/id/floodAreas/{code}/polygon"));
            fetched += 1;
            match self.http.get_json::<geojson::FeatureCollection>(&url).await {
                Ok(fc) => {
                    if let Some(geometry) = fc.merged() {
                        self.areas.put(&code, &geometry).await;
                        pending.observation = std::mem::replace(
                            &mut pending.observation,
                            placeholder(&self.descriptor.id),
                        )
                        .with_geom(geometry);
                    }
                }
                Err(err) => {
                    // One unavailable outline is not a failed poll. The warning
                    // still goes out with its point.
                    tracing::warn!(
                        source = %self.descriptor.id,
                        area = %code,
                        %err,
                        "could not fetch a flood area outline; the warning keeps its point"
                    );
                }
            }
        }
        fetched
    }
}

/// A decoded warning, plus what is needed to attach its outline afterwards.
struct Pending {
    observation: Observation,
    area_code: Option<String>,
    polygon_url: Option<String>,
}

/// Only ever swapped out immediately; `Observation` has no `Default` and the
/// builder consumes `self`.
fn placeholder(source_id: &SourceId) -> Observation {
    Observation::new(
        source_id.clone(),
        EntityId::new(EntityKind::Event, String::new()),
        Utc::now(),
        Quality::Live,
    )
}

#[async_trait::async_trait]
impl Source for EaFloodWarnings {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let feed: Envelope<Flood> = self.http.get_json(&format!("{ROOT}/id/floods")).await?;
        let mut pending = decode_warnings(feed.items, &self.descriptor.id);
        self.resolve_areas(&mut pending).await;
        Ok(pending.into_iter().map(|p| p.observation).collect())
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Flood {
    description: Option<OneOrMany<String>>,
    #[serde(rename = "eaAreaName")]
    ea_area_name: Option<OneOrMany<String>>,
    #[serde(rename = "floodAreaID")]
    flood_area_id: Option<OneOrMany<String>>,
    #[serde(rename = "floodArea")]
    flood_area: Option<FloodArea>,
    #[serde(rename = "isTidal")]
    is_tidal: Option<bool>,
    /// The Agency's words: "Flood Alert", "Flood Warning", "Severe Flood
    /// Warning", "Warning no longer in force".
    severity: Option<OneOrMany<String>>,
    /// 1 severe, 2 warning, 3 alert, 4 no longer in force.
    #[serde(rename = "severityLevel")]
    severity_level: Option<i64>,
    #[serde(rename = "timeRaised")]
    time_raised: Option<String>,
    #[serde(rename = "timeMessageChanged")]
    time_message_changed: Option<String>,
    #[serde(rename = "timeSeverityChanged")]
    time_severity_changed: Option<String>,
    message: Option<OneOrMany<String>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct FloodArea {
    notation: Option<OneOrMany<String>>,
    polygon: Option<OneOrMany<String>>,
    lat: Option<OneOrMany<f64>>,
    long: Option<OneOrMany<f64>>,
    #[serde(rename = "riverOrSea")]
    river_or_sea: Option<OneOrMany<String>>,
    county: Option<OneOrMany<String>>,
}

fn decode_warnings(feed: Vec<Flood>, source_id: &SourceId) -> Vec<Pending> {
    feed.into_iter()
        .filter_map(|f| decode_warning(f, source_id))
        .collect()
}

fn decode_warning(f: Flood, source_id: &SourceId) -> Option<Pending> {
    // Severity 4 is "warning no longer in force". `/id/floods` does not
    // normally return them, but a lapsed warning drawn like a live one is the
    // worst thing this layer could do, so it is refused here rather than
    // trusted not to arrive.
    if f.severity_level == Some(4) {
        return None;
    }

    let area = f.flood_area.unwrap_or_default();
    let area_code = f
        .flood_area_id
        .as_ref()
        .and_then(OneOrMany::first)
        .or_else(|| area.notation.as_ref().and_then(OneOrMany::first))
        .cloned()?;

    let raised = f.time_raised.as_deref().and_then(parse_time);
    // The most recent time the Agency touched this warning — the analogue of
    // the `sent` field the NWS driver uses. Not `timeRaised` alone: a warning
    // in force for a fortnight and re-messaged this morning is current news,
    // and dating it from when it was first raised would push it past the
    // seven-day event horizon and take it off the map while it is still in
    // force.
    let observed_at = [
        f.time_severity_changed.as_deref(),
        f.time_message_changed.as_deref(),
        f.time_raised.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(parse_time)
    .max()?;

    let severity = f
        .severity
        .as_ref()
        .and_then(OneOrMany::first)
        .cloned()
        .unwrap_or_else(|| "Flood warning".to_string());
    let description = f
        .description
        .as_ref()
        .and_then(OneOrMany::first)
        .cloned()
        .unwrap_or_default();

    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("severity", serde_json::json!(severity));
    put("severity_level", serde_json::json!(f.severity_level));
    put("description", serde_json::json!(description));
    put("flood_area", serde_json::json!(area_code));
    put("ea_area", serde_json::json!(first_of(&f.ea_area_name)));
    put("county", serde_json::json!(first_of(&area.county)));
    put("river_or_sea", serde_json::json!(first_of(&area.river_or_sea)));
    put("tidal", serde_json::json!(f.is_tidal));
    put("raised", serde_json::json!(raised.map(|t| t.to_rfc3339())));
    put("message", serde_json::json!(first_of(&f.message)));

    // The area code plus the moment it was raised. A fresh warning on an area
    // that flooded last winter is a new thing to be told about, so it must be
    // a new entity rather than an update to the old one — the same reasoning
    // that makes each storm overflow discharge its own event.
    let key = format!(
        "{area_code}:{}",
        raised.map(|t| t.timestamp()).unwrap_or_default()
    );

    let mut observation = Observation::new(
        source_id.clone(),
        EntityId::new(EntityKind::Event, key),
        observed_at,
        Quality::Live,
    )
    .with_label(if description.is_empty() {
        severity
    } else {
        format!("{severity}: {description}")
    })
    .with_attrs(serde_json::Value::Object(attrs));

    if let (Some(lon), Some(lat)) = (
        area.long.as_ref().and_then(OneOrMany::first).copied(),
        area.lat.as_ref().and_then(OneOrMany::first).copied(),
    ) {
        observation = observation.with_position(Position {
            lon,
            lat,
            alt_m: None,
            datum: argus_core::entity::AltitudeDatum::Geoid,
        });
    }

    Some(Pending {
        observation,
        area_code: Some(area_code),
        polygon_url: area.polygon.as_ref().and_then(OneOrMany::first).cloned(),
    })
}

fn first_of(v: &Option<OneOrMany<String>>) -> Option<String> {
    v.as_ref().and_then(OneOrMany::first).cloned()
}

// =============================================================================
// River, tide and rainfall gauges
// =============================================================================

/// The Agency publishes on a fifteen-minute cycle and says so; asking more
/// often returns the same readings.
const GAUGE_CADENCE_SECS: u64 = 900;

/// Explicit, and not a round number chosen for looks.
///
/// `/id/stations` happened to return all 5,525 rows unasked, but
/// `/id/floodAreas` silently truncated the same style of request to 500 — a
/// complete-looking envelope holding an eighth of the data. Naming the limit
/// costs nothing and removes the question of which endpoints have a default.
const PAGE_LIMIT: usize = 20_000;

pub struct EaRiverGauges {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl EaRiverGauges {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("ea-river-gauges"),
                layer_id: LayerId::new("river-gauges"),
                display_name: "River and rainfall gauges (Environment Agency)".into(),
                kind: EntityKind::Station,
                cadence: Cadence::every(GAUGE_CADENCE_SECS),
                coverage: Coverage::Fixed { bbox: UK_TIDAL_AND_RIVER },
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: ea_attribution(),
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for EaRiverGauges {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        // Two requests for the whole country, which is what the Agency asks for
        // in its own documentation: one `readings?latest` call beats crawling
        // five thousand stations for one value each.
        let stations: Envelope<Station> = self
            .http
            .get_json(&format!("{ROOT}/id/stations?_limit={PAGE_LIMIT}"))
            .await?;
        let readings: Envelope<Reading> = self
            .http
            .get_json(&format!("{ROOT}/data/readings?latest&_limit={PAGE_LIMIT}"))
            .await?;
        Ok(decode_gauges(
            stations.items,
            readings.items,
            &self.descriptor.id,
        ))
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Station {
    notation: Option<OneOrMany<String>>,
    label: Option<OneOrMany<String>>,
    lat: Option<OneOrMany<f64>>,
    long: Option<OneOrMany<f64>>,
    #[serde(rename = "riverName")]
    river_name: Option<OneOrMany<String>>,
    #[serde(rename = "catchmentName")]
    catchment_name: Option<OneOrMany<String>>,
    town: Option<OneOrMany<String>>,
    #[serde(rename = "stationReference")]
    station_reference: Option<OneOrMany<String>>,
    #[serde(rename = "RLOIid")]
    rloi_id: Option<OneOrMany<String>>,
    status: Option<OneOrMany<String>>,
    #[serde(default)]
    measures: Vec<Measure>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
struct Measure {
    #[serde(rename = "@id")]
    id: Option<String>,
    parameter: Option<OneOrMany<String>>,
    #[serde(rename = "parameterName")]
    parameter_name: Option<OneOrMany<String>>,
    qualifier: Option<OneOrMany<String>>,
    #[serde(rename = "unitName")]
    unit_name: Option<OneOrMany<String>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Reading {
    measure: Option<String>,
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
    value: Option<OneOrMany<f64>>,
}

fn decode_gauges(
    stations: Vec<Station>,
    readings: Vec<Reading>,
    source_id: &SourceId,
) -> Vec<Observation> {
    // Measure URI -> its latest reading. The readings arrive as a flat national
    // list with no station on them, so this index is the join.
    let mut latest: HashMap<&str, (&Reading, DateTime<Utc>)> = HashMap::new();
    for reading in &readings {
        let (Some(measure), Some(at)) = (
            reading.measure.as_deref(),
            reading.date_time.as_deref().and_then(parse_time),
        ) else {
            continue;
        };
        // `?latest` should give one row per measure, but a duplicate would
        // otherwise be decided by iteration order rather than by time.
        latest
            .entry(measure)
            .and_modify(|held| {
                if at > held.1 {
                    *held = (reading, at);
                }
            })
            .or_insert((reading, at));
    }

    stations
        .iter()
        .filter_map(|s| decode_gauge(s, &latest, source_id))
        .collect()
}

fn decode_gauge(
    s: &Station,
    latest: &HashMap<&str, (&Reading, DateTime<Utc>)>,
    source_id: &SourceId,
) -> Option<Observation> {
    let key = s
        .notation
        .as_ref()
        .and_then(OneOrMany::first)
        .or_else(|| s.station_reference.as_ref().and_then(OneOrMany::first))
        .cloned()?;

    // 630 of the 5,525 stations publish no position at all. A gauge with no
    // location cannot be drawn or fenced, and giving it one would be inventing
    // data.
    let (lon, lat) = (
        s.long.as_ref().and_then(OneOrMany::first).copied()?,
        s.lat.as_ref().and_then(OneOrMany::first).copied()?,
    );

    // Each instrument at the station, with its newest reading.
    let mut instruments = Vec::new();
    let mut newest: Option<DateTime<Utc>> = None;
    for measure in &s.measures {
        let Some((reading, at)) = measure.id.as_deref().and_then(|id| latest.get(id)) else {
            continue;
        };
        let Some(value) = reading.value.as_ref().and_then(OneOrMany::first).copied() else {
            continue;
        };
        newest = Some(newest.map_or(*at, |held: DateTime<Utc>| held.max(*at)));
        instruments.push(serde_json::json!({
            "parameter": first_of(&measure.parameter),
            "name": first_of(&measure.parameter_name),
            "qualifier": first_of(&measure.qualifier),
            "unit": first_of(&measure.unit_name),
            "value": value,
            "at": at.to_rfc3339(),
        }));
    }

    // A gauge with nothing to report is not observed. Unlike the storm overflow
    // feeds — where the company is actively asserting a state and the record's
    // own stamp means different things at different companies — a reading here
    // carries the instant the water level was actually measured. A gauge whose
    // newest reading is three days old genuinely is not reporting, and letting
    // the station horizon retire it is the correct answer rather than a bug to
    // work around.
    let observed_at = newest?;

    let label = match (
        s.label.as_ref().and_then(OneOrMany::first),
        s.river_name.as_ref().and_then(OneOrMany::first),
    ) {
        (Some(name), Some(river)) if !river.is_empty() => format!("{name} ({river})"),
        (Some(name), _) => name.clone(),
        (None, _) => key.clone(),
    };

    let mut attrs = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        if !v.is_null() {
            attrs.insert(k.to_string(), v);
        }
    };
    put("station", serde_json::json!(key));
    put("river", serde_json::json!(first_of(&s.river_name)));
    put("catchment", serde_json::json!(first_of(&s.catchment_name)));
    put("town", serde_json::json!(first_of(&s.town)));
    put("rloi_id", serde_json::json!(first_of(&s.rloi_id)));
    // The status URI's last segment: statusActive, statusSuspended, statusClosed.
    put(
        "status",
        serde_json::json!(first_of(&s.status).and_then(|u| u
            .rsplit('/')
            .next()
            .map(|s| s.trim_start_matches("status").to_lowercase()))),
    );
    attrs.insert("readings".into(), serde_json::Value::Array(instruments));

    Some(
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
            datum: argus_core::entity::AltitudeDatum::Geoid,
        })
        .with_label(label)
        .with_attrs(serde_json::Value::Object(attrs)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("a test instant")
            .with_timezone(&Utc)
    }

    fn warnings_source() -> SourceId {
        SourceId::new("ea-flood-warnings")
    }

    fn gauges_source() -> SourceId {
        SourceId::new("ea-river-gauges")
    }

    /// The Environment Agency's own documented example of a flood item.
    const FLOOD_DOC_EXAMPLE: &str = r#"{
        "items": [{
            "@id": "http://environment.data.gov.uk/flood-monitoring/id/floods/91436",
            "description": "North Sea Coast from Whitby to Filey",
            "eaAreaName": "Yorkshire",
            "eaRegionName": "North East",
            "floodArea": {
                "notation": "122WAC953",
                "polygon": "http://environment.data.gov.uk/flood-monitoring/id/floodAreas/122WAC953/polygon",
                "lat": 54.45,
                "long": -0.55,
                "riverOrSea": "North Sea",
                "county": "North Yorkshire"
            },
            "floodAreaID": "122WAC953",
            "isTidal": true,
            "severity": "Flood Warning",
            "severityLevel": 2,
            "timeMessageChanged": "2015-02-02T19:32:00",
            "timeRaised": "2015-02-02T19:32:00",
            "timeSeverityChanged": "2015-02-02T19:32:00"
        }]
    }"#;

    #[test]
    fn the_agencys_own_documented_flood_example_decodes() {
        // There were no warnings in force when this driver was written, so the
        // published schema is the fixture. If the Agency changes it, the live
        // test is what will notice.
        let feed: Envelope<Flood> =
            serde_json::from_str(FLOOD_DOC_EXAMPLE).expect("the documented shape decodes");
        let pending = decode_warnings(feed.items, &warnings_source());
        assert_eq!(pending.len(), 1);
        let o = &pending[0].observation;
        assert_eq!(o.entity.kind, EntityKind::Event);
        assert_eq!(o.attrs["severity_level"], serde_json::json!(2));
        assert_eq!(o.attrs["river_or_sea"], serde_json::json!("North Sea"));
        assert_eq!(o.attrs["tidal"], serde_json::json!(true));
        assert_eq!(
            o.label.as_deref(),
            Some("Flood Warning: North Sea Coast from Whitby to Filey")
        );
        let p = o.position.expect("the flood area's point");
        assert!((p.lat - 54.45).abs() < 1e-9);
        assert_eq!(
            pending[0].area_code.as_deref(),
            Some("122WAC953"),
            "the area code is what the outline is cached under"
        );
    }

    #[test]
    fn a_timestamp_without_a_zone_is_read_as_utc_not_as_local_time() {
        // The flood endpoint omits the zone. On a machine in BST, reading it as
        // local would date every summer warning an hour out.
        assert_eq!(parse_time("2015-02-02T19:32:00"), Some(at("2015-02-02T19:32:00Z")));
        // Readings do carry one, and it must still be honoured.
        assert_eq!(parse_time("2026-09-08T14:45:00Z"), Some(at("2026-09-08T14:45:00Z")));
    }

    #[test]
    fn a_warning_is_dated_by_its_latest_update_not_by_when_it_was_first_raised() {
        // A warning in force for a fortnight and re-messaged this morning is
        // current. Dated from `timeRaised` it would fall past the seven-day
        // event horizon and vanish while still in force.
        let mut f: Flood = serde_json::from_str(
            r#"{"floodAreaID":"122WAC953","severity":"Flood Warning","severityLevel":2,
                "timeRaised":"2026-08-20T06:00:00"}"#,
        )
        .expect("a flood");
        f.time_message_changed = Some("2026-09-08T07:15:00".into());
        let pending = decode_warning(f, &warnings_source()).expect("a warning");
        assert_eq!(pending.observation.observed_at, at("2026-09-08T07:15:00Z"));
        // But the key still remembers when it began, so a re-raise is new.
        assert_eq!(
            pending.observation.entity.key,
            format!("122WAC953:{}", at("2026-08-20T06:00:00Z").timestamp())
        );
    }

    #[test]
    fn a_warning_no_longer_in_force_is_not_drawn_as_one_that_is() {
        let f: Flood = serde_json::from_str(
            r#"{"floodAreaID":"122WAC953","severity":"Warning no longer in force",
                "severityLevel":4,"timeRaised":"2026-09-08T06:00:00"}"#,
        )
        .expect("a flood");
        assert!(decode_warning(f, &warnings_source()).is_none());
    }

    #[test]
    fn a_fresh_warning_on_a_previously_flooded_area_is_a_new_thing_to_be_told_about() {
        let one: Flood = serde_json::from_str(
            r#"{"floodAreaID":"122WAC953","severityLevel":2,"timeRaised":"2026-01-04T06:00:00"}"#,
        )
        .expect("a flood");
        let two: Flood = serde_json::from_str(
            r#"{"floodAreaID":"122WAC953","severityLevel":2,"timeRaised":"2026-09-08T06:00:00"}"#,
        )
        .expect("a flood");
        let a = decode_warning(one, &warnings_source()).expect("a warning");
        let b = decode_warning(two, &warnings_source()).expect("a warning");
        assert_ne!(
            a.observation.entity.key, b.observation.entity.key,
            "the same area flooding again must alert again"
        );
    }

    // --- the scalar-or-array trap ---

    #[test]
    fn one_station_with_two_positions_does_not_cost_every_gauge_in_england() {
        // E85123 really does publish `lat: [51.19557, 51.196412]`. Declared as
        // f64 that one station fails to deserialise, and because the items are
        // one array, serde fails the entire document — 5,525 gauges lost to a
        // station that cannot make its mind up to within a hundred metres.
        let json = r#"{"items":[
            {"notation":"E85123","label":"Ilfracombe","lat":[51.19557,51.196412],
             "long":[-4.119951,-4.120133],"status":["http://x/def/core/statusActive",
             "http://x/def/core/statusSuspended"],"RLOIid":["10427","9154"],
             "measures":[{"@id":"m1","parameter":"level","parameterName":"Water Level",
                          "qualifier":"Stage","unitName":"mASD"}]},
            {"notation":"1029TH","label":"Bourton Dickler","lat":51.874767,"long":-1.740083,
             "riverName":"River Dikler","status":"http://x/def/core/statusActive",
             "measures":[{"@id":"m2","parameter":"level","parameterName":"Water Level",
                          "qualifier":"Stage","unitName":"mASD"}]}
        ]}"#;
        let stations: Envelope<Station> =
            serde_json::from_str(json).expect("both cardinalities decode");
        let readings: Envelope<Reading> = serde_json::from_str(
            r#"{"items":[{"measure":"m1","dateTime":"2026-09-08T15:00:00Z","value":1.2},
                         {"measure":"m2","dateTime":"2026-09-08T15:00:00Z","value":-0.338}]}"#,
        )
        .expect("readings decode");
        let obs = decode_gauges(stations.items, readings.items, &gauges_source());
        assert_eq!(obs.len(), 2, "the awkward station and the ordinary one");
        // The first of the two positions is used rather than the record refused.
        let ilfracombe = obs.iter().find(|o| o.entity.key == "E85123").expect("E85123");
        assert!((ilfracombe.position.expect("a position").lat - 51.19557).abs() < 1e-9);
        assert_eq!(ilfracombe.attrs["status"], serde_json::json!("active"));
    }

    #[test]
    fn an_array_valued_reading_is_read_rather_than_dropped() {
        // One reading of 5,347 carried an array value.
        let stations: Envelope<Station> = serde_json::from_str(
            r#"{"items":[{"notation":"X1","lat":52.0,"long":-1.0,
                 "measures":[{"@id":"m1","parameter":"flow","unitName":"m3/s"}]}]}"#,
        )
        .expect("a station");
        let readings: Envelope<Reading> = serde_json::from_str(
            r#"{"items":[{"measure":"m1","dateTime":"2026-09-08T15:00:00Z","value":[3.5,3.6]}]}"#,
        )
        .expect("an array-valued reading decodes");
        let obs = decode_gauges(stations.items, readings.items, &gauges_source());
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].attrs["readings"][0]["value"], serde_json::json!(3.5));
    }

    // --- gauges ---

    #[test]
    fn a_gauge_is_observed_when_the_water_was_measured() {
        let stations: Envelope<Station> = serde_json::from_str(
            r#"{"items":[{"notation":"1029TH","label":"Bourton Dickler","lat":51.87,"long":-1.74,
                 "riverName":"River Dikler","town":"Little Rissington",
                 "catchmentName":"Cotswolds",
                 "measures":[{"@id":"m1","parameter":"level","parameterName":"Water Level",
                              "qualifier":"Stage","unitName":"mASD"}]}]}"#,
        )
        .expect("a station");
        let readings: Envelope<Reading> = serde_json::from_str(
            r#"{"items":[{"measure":"m1","dateTime":"2026-09-08T14:45:00Z","value":-0.338}]}"#,
        )
        .expect("a reading");
        let obs = decode_gauges(stations.items, readings.items, &gauges_source());
        assert_eq!(obs[0].observed_at, at("2026-09-08T14:45:00Z"));
        assert_eq!(obs[0].label.as_deref(), Some("Bourton Dickler (River Dikler)"));
        assert_eq!(obs[0].attrs["readings"][0]["value"], serde_json::json!(-0.338));
        assert_eq!(obs[0].attrs["readings"][0]["unit"], serde_json::json!("mASD"));
        assert_eq!(obs[0].attrs["catchment"], serde_json::json!("Cotswolds"));
    }

    #[test]
    fn a_station_with_several_instruments_carries_all_of_them_and_the_newest_time() {
        let stations: Envelope<Station> = serde_json::from_str(
            r#"{"items":[{"notation":"S1","lat":52.0,"long":-1.0,"measures":[
                 {"@id":"m1","parameter":"level","qualifier":"Stage","unitName":"mASD"},
                 {"@id":"m2","parameter":"level","qualifier":"Downstream Stage","unitName":"mASD"},
                 {"@id":"m3","parameter":"rainfall","unitName":"mm"}]}]}"#,
        )
        .expect("a station");
        let readings: Envelope<Reading> = serde_json::from_str(
            r#"{"items":[
                 {"measure":"m1","dateTime":"2026-09-08T14:45:00Z","value":1.0},
                 {"measure":"m2","dateTime":"2026-09-08T15:00:00Z","value":0.5},
                 {"measure":"m3","dateTime":"2026-09-08T14:30:00Z","value":0.2}]}"#,
        )
        .expect("readings");
        let obs = decode_gauges(stations.items, readings.items, &gauges_source());
        assert_eq!(obs[0].attrs["readings"].as_array().expect("readings").len(), 3);
        assert_eq!(
            obs[0].observed_at,
            at("2026-09-08T15:00:00Z"),
            "the station is as fresh as its freshest instrument"
        );
    }

    #[test]
    fn a_gauge_with_no_position_is_skipped_rather_than_placed_at_null_island() {
        // 630 of 5,525 stations publish no lat/long at all.
        let stations: Envelope<Station> = serde_json::from_str(
            r#"{"items":[{"notation":"NOPOS","measures":[{"@id":"m1","parameter":"level"}]}]}"#,
        )
        .expect("a station");
        let readings: Envelope<Reading> = serde_json::from_str(
            r#"{"items":[{"measure":"m1","dateTime":"2026-09-08T15:00:00Z","value":1.0}]}"#,
        )
        .expect("a reading");
        assert!(decode_gauges(stations.items, readings.items, &gauges_source()).is_empty());
    }

    #[test]
    fn a_gauge_with_no_current_reading_is_not_reported_as_observed() {
        // Letting the station horizon retire a gauge that stopped reporting is
        // the correct behaviour, not something to paper over with a poll time.
        let stations: Envelope<Station> = serde_json::from_str(
            r#"{"items":[{"notation":"QUIET","lat":52.0,"long":-1.0,
                 "measures":[{"@id":"m1","parameter":"level"}]}]}"#,
        )
        .expect("a station");
        let readings: Envelope<Reading> =
            serde_json::from_str(r#"{"items":[]}"#).expect("no readings");
        assert!(decode_gauges(stations.items, readings.items, &gauges_source()).is_empty());
    }

    #[test]
    fn the_newest_of_two_readings_for_one_measure_wins_regardless_of_order() {
        let stations: Envelope<Station> = serde_json::from_str(
            r#"{"items":[{"notation":"S1","lat":52.0,"long":-1.0,
                 "measures":[{"@id":"m1","parameter":"level"}]}]}"#,
        )
        .expect("a station");
        let readings: Envelope<Reading> = serde_json::from_str(
            r#"{"items":[{"measure":"m1","dateTime":"2026-09-08T15:00:00Z","value":9.0},
                         {"measure":"m1","dateTime":"2026-09-08T14:00:00Z","value":1.0}]}"#,
        )
        .expect("readings");
        let obs = decode_gauges(stations.items, readings.items, &gauges_source());
        assert_eq!(obs[0].attrs["readings"][0]["value"], serde_json::json!(9.0));
        assert_eq!(obs[0].observed_at, at("2026-09-08T15:00:00Z"));
    }

    #[test]
    fn an_empty_envelope_is_an_empty_poll_and_not_an_error() {
        // England was entirely free of flood warnings on the day this was
        // written, and that is a normal Tuesday rather than a broken feed.
        let feed: Envelope<Flood> = serde_json::from_str(
            r#"{"@context":"x","meta":{"publisher":"Environment Agency"},"items":[]}"#,
        )
        .expect("an empty envelope decodes");
        assert!(decode_warnings(feed.items, &warnings_source()).is_empty());
    }
}
