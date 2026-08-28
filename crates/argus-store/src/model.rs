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
