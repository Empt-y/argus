//! What a geofence is watching for.
//!
//! The rule is JSON on the row rather than columns, so adding a predicate is a
//! code change here and not a migration. Everything is optional and an absent
//! predicate means "do not care", which makes `{}` a valid rule that fires on
//! anything entering the polygon — the shortest thing someone can type that
//! still does something useful.

use argus_core::EntityKind;
use serde::{Deserialize, Serialize};

/// When a fence fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The moment something crosses in. The default, and what "geofence"
    /// usually means.
    #[default]
    Enters,
    /// The moment something crosses out. Useful for the inverse watch: tell me
    /// when this stops being where it should be.
    Exits,
    /// Still inside after [`Rule::dwell_seconds`]. An aircraft crossing an
    /// approach fence is traffic; one that is still there four minutes later is
    /// holding, and that is the interesting one.
    Dwells,
}

/// How loud an alert is.
///
/// The spellings match the CHECK constraint on `alerts.severity`, and the
/// Android client maps them onto notification channels — which is why this is
/// an enum rather than free text: a typo would become a channel nobody has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Info,
    Notice,
    Warning,
    Critical,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Notice => "notice",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// The predicates a candidate has to satisfy, and what happens when it does.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Rule {
    pub trigger: Trigger,
    /// For [`Trigger::Dwells`]. Zero is treated as "the moment it entered",
    /// which makes a dwell rule with no duration behave like an entry rule
    /// rather than never firing.
    pub dwell_seconds: u64,
    /// Empty means any kind.
    pub kinds: Vec<EntityKind>,
    /// Empty means any layer.
    pub layers: Vec<String>,
    /// Altitude band, in metres, against whatever datum the source reported.
    ///
    /// Deliberately not normalised to a common datum here. A barometric
    /// altitude and a height above ground are different quantities, and
    /// silently converting one to the other to satisfy a rule would be the sort
    /// of quiet lie the rest of this system goes out of its way to avoid. A
    /// rule about aircraft on approach is written against what ADS-B reports.
    pub min_alt_m: Option<f64>,
    pub max_alt_m: Option<f64>,
    pub min_speed_mps: Option<f64>,
    pub max_speed_mps: Option<f64>,
    /// Case-insensitive substring of the entity's label — a callsign prefix, a
    /// vessel name.
    pub label_contains: Option<String>,
    /// Ignore anything the store has marked as modeled or estimated. Off by
    /// default: a propagated satellite pass over a fence is a real thing to
    /// want to know about.
    pub observed_only: bool,
    pub severity: Severity,
    /// Minimum gap between two alerts about the same entity from the same
    /// fence. Without it, one aircraft weaving along a boundary fires every
    /// time the store learns a new fix.
    pub cooldown_seconds: u64,
}

/// The subset of an entity a rule can look at.
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub kind: EntityKind,
    pub layer: &'a str,
    pub label: Option<&'a str>,
    pub alt_m: Option<f64>,
    pub speed_mps: Option<f64>,
    pub quality: &'a str,
}

impl Rule {
    /// The default cooldown when a rule does not name one.
    pub const DEFAULT_COOLDOWN_SECONDS: u64 = 300;

    pub fn cooldown(&self) -> chrono::Duration {
        chrono::Duration::seconds(if self.cooldown_seconds == 0 {
            Self::DEFAULT_COOLDOWN_SECONDS as i64
        } else {
            self.cooldown_seconds as i64
        })
    }

    /// Whether this candidate is the sort of thing the fence cares about.
    ///
    /// Geometry is not tested here — that is the engine's job, and it is the
    /// expensive half. This is the cheap filter that runs first.
    pub fn matches(&self, candidate: &Candidate<'_>) -> bool {
        if !self.kinds.is_empty() && !self.kinds.contains(&candidate.kind) {
            return false;
        }
        if !self.layers.is_empty() && !self.layers.iter().any(|l| l == candidate.layer) {
            return false;
        }
        if self.observed_only && candidate.quality != "live" && candidate.quality != "delayed" {
            return false;
        }
        // An absent altitude fails an altitude rule rather than passing it. A
        // rule that says "below 1500 m" is asking about aircraft whose height
        // is known; treating unknown as satisfying it would alert on every
        // vessel and satellite that crossed the box.
        if let Some(min) = self.min_alt_m
            && candidate.alt_m.is_none_or(|a| a < min)
        {
            return false;
        }
        if let Some(max) = self.max_alt_m
            && candidate.alt_m.is_none_or(|a| a > max)
        {
            return false;
        }
        if let Some(min) = self.min_speed_mps
            && candidate.speed_mps.is_none_or(|s| s < min)
        {
            return false;
        }
        if let Some(max) = self.max_speed_mps
            && candidate.speed_mps.is_none_or(|s| s > max)
        {
            return false;
        }
        if let Some(needle) = &self.label_contains {
            let Some(label) = candidate.label else {
                return false;
            };
            if !label.to_lowercase().contains(&needle.to_lowercase()) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> Candidate<'static> {
        Candidate {
            kind: EntityKind::Aircraft,
            layer: "flights",
            label: Some("BAW123"),
            alt_m: Some(1000.0),
            speed_mps: Some(80.0),
            quality: "live",
        }
    }

    #[test]
    fn an_empty_rule_is_valid_and_matches_anything() {
        let rule: Rule = serde_json::from_value(serde_json::json!({})).expect("{} is a rule");
        assert!(rule.matches(&candidate()));
        assert_eq!(rule.trigger, Trigger::Enters);
        assert_eq!(rule.severity, Severity::Info);
    }

    #[test]
    fn an_unknown_predicate_is_refused_rather_than_ignored() {
        // A rule with a typo that silently matches everything is worse than one
        // that fails to save: the fence looks armed and is not.
        let err = serde_json::from_value::<Rule>(serde_json::json!({ "min_altitude": 100 }));
        assert!(err.is_err(), "unknown fields must not be dropped");
    }

    #[test]
    fn altitude_and_speed_bands_bound_on_both_sides() {
        let rule = Rule {
            max_alt_m: Some(1500.0),
            min_speed_mps: Some(50.0),
            ..Rule::default()
        };
        assert!(rule.matches(&candidate()));
        assert!(!rule.matches(&Candidate { alt_m: Some(9000.0), ..candidate() }));
        assert!(!rule.matches(&Candidate { speed_mps: Some(10.0), ..candidate() }));
    }

    #[test]
    fn an_unknown_altitude_fails_an_altitude_rule() {
        // Otherwise a fence watching for low aircraft alerts on every satellite
        // and vessel that crosses it.
        let rule = Rule { max_alt_m: Some(1500.0), ..Rule::default() };
        assert!(!rule.matches(&Candidate { alt_m: None, ..candidate() }));
    }

    #[test]
    fn a_label_filter_is_case_insensitive() {
        let rule = Rule { label_contains: Some("baw".into()), ..Rule::default() };
        assert!(rule.matches(&candidate()));
        assert!(!rule.matches(&Candidate { label: Some("EZY22"), ..candidate() }));
        assert!(!rule.matches(&Candidate { label: None, ..candidate() }));
    }

    #[test]
    fn observed_only_excludes_modeled_positions() {
        let rule = Rule { observed_only: true, ..Rule::default() };
        assert!(rule.matches(&candidate()));
        assert!(!rule.matches(&Candidate { quality: "modeled", ..candidate() }));
        // ...but it is off by default, because a propagated satellite pass over
        // a fence is a real thing to want to know about.
        assert!(Rule::default().matches(&Candidate { quality: "modeled", ..candidate() }));
    }

    #[test]
    fn a_rule_with_no_cooldown_still_has_one() {
        assert_eq!(Rule::default().cooldown().num_seconds(), 300);
        let explicit = Rule { cooldown_seconds: 30, ..Rule::default() };
        assert_eq!(explicit.cooldown().num_seconds(), 30);
    }
}
