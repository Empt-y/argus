//! How recent an observation has to be to count as describing the present.
//!
//! One concept, used in two places that disagreed with each other. The live
//! queries had no horizon at all — a map left running showed every aircraft
//! ever recorded, most of them days old, drawn exactly like the ones in the
//! sky. The DVR queries had a flat fifteen-minute window for every kind, which
//! was too short for events: rewinding to yesterday showed no earthquakes
//! unless one happened inside that quarter of an hour.
//!
//! The net effect was an inversion — rewinding the DVR showed *fewer* contacts
//! than live did, because the past was being filtered honestly and the present
//! was not.
//!
//! The horizon lives on [`EntityKind`] so there is exactly one table of it, and
//! this module renders that table into SQL rather than hand-writing a `CASE`
//! that would drift the first time a kind is added.

use argus_core::EntityKind;

/// SQL predicate: true when `time_col` is within its kind's horizon of
/// `reference`.
///
/// `reference` is `now()` for the live queries and the DVR instant for the
/// past ones, which is what makes rewinding show the same population it would
/// have shown live at that moment.
///
/// Every fragment this interpolates is a compile-time constant from the enum —
/// a kind's wire spelling and a number of minutes — so nothing here carries a
/// value from a request.
pub fn within_horizon(kind_col: &str, time_col: &str, reference: &str) -> String {
    let mut sql = format!("(CASE {kind_col}");
    for kind in EntityKind::ALL {
        match kind.live_horizon() {
            Some(horizon) => sql.push_str(&format!(
                " WHEN '{}' THEN {time_col} > {reference} - INTERVAL '{} minutes'",
                kind.as_str(),
                horizon.num_minutes(),
            )),
            None => sql.push_str(&format!(" WHEN '{}' THEN TRUE", kind.as_str())),
        }
    }
    // An unrecognised kind is a bug somewhere upstream, and hiding the row
    // would hide the bug along with it. Show it and let it be noticed.
    sql.push_str(" ELSE TRUE END)");
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_is_named_so_none_falls_through_to_the_default() {
        let sql = within_horizon("entity_kind", "observed_at", "now()");
        for kind in EntityKind::ALL {
            assert!(
                sql.contains(&format!("WHEN '{}'", kind.as_str())),
                "{} is missing from the horizon table: {sql}",
                kind.as_str()
            );
        }
    }

    #[test]
    fn an_aircraft_expires_and_a_cable_does_not() {
        let sql = within_horizon("entity_kind", "observed_at", "now()");
        assert!(sql.contains("WHEN 'aircraft' THEN observed_at > now() - INTERVAL '15 minutes'"));
        assert!(sql.contains("WHEN 'feature' THEN TRUE"));
        // Events outlive everything that moves: an earthquake is a fact about a
        // moment, not a claim about now.
        assert!(sql.contains("WHEN 'event' THEN observed_at > now() - INTERVAL '10080 minutes'"));
    }

    #[test]
    fn the_reference_instant_is_substituted_everywhere_it_appears() {
        // The DVR passes a bind parameter here, and every arm has to use it or
        // rewinding filters against the wrong clock.
        let sql = within_horizon("t.entity_kind", "t.bucket", "$5");
        assert_eq!(sql.matches("$5").count(), EntityKind::ALL.len() - 1);
        assert!(!sql.contains("now()"));
    }
}
