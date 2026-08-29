//! Row types and the string mappings between `argus_core` enums and their
//! database spellings.
//!
//! These mappings are deliberately explicit rather than derived. The database
//! spellings are a wire format that both clients and the SQL migrations depend
//! on, so they must not silently follow a Rust identifier rename.

use argus_core::entity::{AltitudeDatum, EntityKind, Quality};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A row of live or historical entity state.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct EntityRow {
    pub entity_kind: String,
    pub entity_key: String,
    pub source_id: String,
    pub layer_id: String,
    pub observed_at: DateTime<Utc>,
    pub lon: Option<f64>,
    pub lat: Option<f64>,
    /// Non-point geography as GeoJSON, when the entity has a shape as well as
    /// (or instead of) a point.
    pub geom: Option<serde_json::Value>,
    pub alt_m: Option<f64>,
    pub alt_datum: Option<String>,
    pub course_deg: Option<f32>,
    pub heading_deg: Option<f32>,
    pub speed_mps: Option<f32>,
    pub vrate_mps: Option<f32>,
    pub quality: String,
    pub label: Option<String>,
    pub attrs: serde_json::Value,
}

/// One sample in an entity's history.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct TrackPoint {
    pub at: DateTime<Utc>,
    pub lon: Option<f64>,
    pub lat: Option<f64>,
    pub alt_m: Option<f64>,
    pub alt_datum: Option<String>,
    pub course_deg: Option<f32>,
    pub speed_mps: Option<f32>,
}

/// A registered source and its current health.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct SourceRow {
    pub source_id: String,
    pub layer_id: String,
    pub display_name: String,
    pub entity_kind: String,
    pub cost_class: String,
    pub state: String,
    pub state_since: DateTime<Utc>,
    pub last_success: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_lag_ms: Option<i32>,
    pub observations: i64,
    pub attribution: serde_json::Value,
}

pub const fn cost_class_str(c: argus_core::CostClass) -> &'static str {
    match c {
        argus_core::CostClass::Free => "free",
        argus_core::CostClass::Metered => "metered",
        argus_core::CostClass::Local => "local",
    }
}

/// Flatten a health value into the three columns `sources` stores it in.
///
/// The mapping is lossy on purpose — the database keeps what an operator or a
/// client needs to render a status chip, not the full enum. The `state` strings
/// must match the CHECK constraint in 0001_core.sql.
pub fn health_columns(
    health: &argus_core::SourceHealth,
) -> (&'static str, Option<String>, Option<i32>) {
    use argus_core::SourceHealth as H;
    match health {
        H::Live { .. } => ("live", None, None),
        H::Delayed { lag, .. } => (
            "delayed",
            None,
            // Saturate rather than wrap: a feed reporting a lag beyond ~24 days
            // is broken, and a wrapped negative would read as a feed from the
            // future.
            Some(lag.num_milliseconds().clamp(0, i32::MAX as i64) as i32),
        ),
        H::Stale { last_error, .. } => ("stale", Some(last_error.clone()), None),
        H::Degraded { reason, .. } => ("degraded", Some(reason.clone()), None),
        H::KeyRequired { config_key } => (
            "key_required",
            Some(format!("set {config_key} to enable this source")),
            None,
        ),
        H::HardwareAbsent { description } => (
            "hardware_absent",
            Some(format!("requires {description}")),
            None,
        ),
        H::Unknown => ("unknown", None, None),
        H::Failed { error, .. } => ("failed", Some(error.clone()), None),
    }
}

pub const fn quality_str(q: Quality) -> &'static str {
    match q {
        Quality::Live => "live",
        Quality::Delayed => "delayed",
        Quality::Modeled => "modeled",
        Quality::Estimated => "estimated",
        Quality::Stale => "stale",
    }
}

pub fn parse_quality(s: &str) -> Option<Quality> {
    Some(match s {
        "live" => Quality::Live,
        "delayed" => Quality::Delayed,
        "modeled" => Quality::Modeled,
        "estimated" => Quality::Estimated,
        "stale" => Quality::Stale,
        _ => return None,
    })
}

pub const fn alt_datum_str(d: AltitudeDatum) -> &'static str {
    match d {
        AltitudeDatum::Wgs84Ellipsoid => "wgs84_ellipsoid",
        AltitudeDatum::Geoid => "geoid",
        AltitudeDatum::AboveGround => "above_ground",
        AltitudeDatum::Barometric => "barometric",
    }
}

pub fn parse_alt_datum(s: &str) -> Option<AltitudeDatum> {
    Some(match s {
        "wgs84_ellipsoid" => AltitudeDatum::Wgs84Ellipsoid,
        "geoid" => AltitudeDatum::Geoid,
        "above_ground" => AltitudeDatum::AboveGround,
        "barometric" => AltitudeDatum::Barometric,
        _ => return None,
    })
}

pub fn parse_entity_kind(s: &str) -> Option<EntityKind> {
    Some(match s {
        "aircraft" => EntityKind::Aircraft,
        "vessel" => EntityKind::Vessel,
        "satellite" => EntityKind::Satellite,
        "event" => EntityKind::Event,
        "station" => EntityKind::Station,
        "feature" => EntityKind::Feature,
        "measure" => EntityKind::Measure,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The database CHECK constraints in 0001_core.sql enumerate exactly these
    /// strings. If a variant is added to the Rust enum without extending the
    /// domain, writes fail at runtime with a constraint violation — so pin the
    /// round trip here, where it is a compile-plus-test failure instead.
    #[test]
    fn every_quality_round_trips() {
        for q in [
            Quality::Live,
            Quality::Delayed,
            Quality::Modeled,
            Quality::Estimated,
            Quality::Stale,
        ] {
            assert_eq!(parse_quality(quality_str(q)), Some(q));
        }
    }

    #[test]
    fn every_alt_datum_round_trips() {
        for d in [
            AltitudeDatum::Wgs84Ellipsoid,
            AltitudeDatum::Geoid,
            AltitudeDatum::AboveGround,
            AltitudeDatum::Barometric,
        ] {
            assert_eq!(parse_alt_datum(alt_datum_str(d)), Some(d));
        }
    }

    #[test]
    fn every_entity_kind_round_trips() {
        for k in [
            EntityKind::Aircraft,
            EntityKind::Vessel,
            EntityKind::Satellite,
            EntityKind::Event,
            EntityKind::Station,
            EntityKind::Feature,
            EntityKind::Measure,
        ] {
            assert_eq!(parse_entity_kind(k.as_str()), Some(k));
        }
    }

    #[test]
    fn unknown_spellings_are_rejected_rather_than_defaulted() {
        assert_eq!(parse_quality("definitely-live"), None);
        assert_eq!(parse_alt_datum("msl"), None);
        assert_eq!(parse_entity_kind("submarine"), None);
    }
}

/// A layer as the registry knows it: several sources may feed one.
///
/// `state` is the *best* state among the layer's sources, because that is what
/// the layer can actually deliver — a chain whose primary is down but whose
/// fallback is answering is a live layer, and reporting it as stale because one
/// member is stale would be a lie in the pessimistic direction.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct LayerRow {
    pub layer_id: String,
    pub entity_kind: String,
    pub display_name: String,
    pub state: String,
    pub source_ids: Vec<String>,
    pub last_success: Option<DateTime<Utc>>,
    pub observations: i64,
    pub attribution: serde_json::Value,
    /// Live entities currently held for this layer. Zero with a healthy state
    /// is a real answer (an empty sky), not a fault.
    pub live_entities: i64,
}

/// One feature on its way into a vector tile.
///
/// Deliberately narrower than [`EntityRow`]: `attrs` is excluded. A tile is a
/// rendering payload, and shipping every source's raw JSON blob into one would
/// multiply its size for data no renderer reads. Clients that want the full
/// record fetch it by key.
#[derive(Debug, sqlx::FromRow)]
pub struct TileRow {
    pub entity_kind: String,
    pub entity_key: String,
    pub source_id: String,
    pub layer_id: String,
    pub observed_at: DateTime<Utc>,
    pub geometry: geozero::wkb::Decode<geo_types::Geometry<f64>>,
    pub alt_m: Option<f64>,
    pub course_deg: Option<f32>,
    pub heading_deg: Option<f32>,
    pub speed_mps: Option<f32>,
    pub vrate_mps: Option<f32>,
    pub quality: String,
    pub label: Option<String>,
}

/// A paired client. The token itself is not here and cannot be recovered — only
/// its hash was ever stored.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct DeviceRow {
    pub device_id: uuid::Uuid,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}
