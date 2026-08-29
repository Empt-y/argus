//! Turning a magnitude into an area, honestly.
//!
//! An earthquake feed gives a point, a magnitude and a depth. It does not give
//! the area that felt it — but that area is the thing a person actually wants
//! to see on a map, and a dot at the epicentre of an M7 tells them almost
//! nothing about who was affected.
//!
//! USGS does publish the real answer for large events: a ShakeMap, with
//! measured-and-modelled intensity contours. Only a minority of events get one
//! — 12 of the 13 significant quakes in a sample month, out of hundreds of
//! events in the same period. For everything else there is no observed area,
//! and the choice is between a bare dot and an estimate.
//!
//! This is the estimate, and the rules it plays by:
//!
//!   * it is published as an attribute, never as geometry. The store holds what
//!     the network reported; a modelled circle is presentation, and baking one
//!     into the `geom` column would make it indistinguishable from a real
//!     ShakeMap contour a week later;
//!   * it is tagged as modelled, so a client cannot draw it like measured data
//!     without going out of its way;
//!   * it is first-order and says so. Depth is deliberately not in the
//!     relation, even though deep events really are felt more widely, because a
//!     depth term this crate cannot validate would add false precision rather
//!     than accuracy.

/// Radius, in metres, within which shaking from an event of this magnitude is
/// typically perceptible.
///
/// `10^((M - 1.5) / 2.5)` kilometres. A first-order fit to the familiar
/// magnitude/felt-area relations rather than a specific published model, chosen
/// because it lands in the right place across the range that matters:
///
/// | M | radius |
/// |---|--------|
/// | 3 | ~4 km |
/// | 4 | ~10 km |
/// | 5 | ~25 km |
/// | 6 | ~63 km |
/// | 7 | ~158 km |
/// | 8 | ~400 km |
///
/// Returns `None` below M2.5, where perceptible shaking is confined to a few
/// hundred metres and drawing a circle would imply a precision that is not
/// there — those events stay as points.
pub fn felt_radius_m(magnitude: f64) -> Option<f64> {
    const MIN_MAGNITUDE: f64 = 2.5;
    /// Nothing is felt beyond roughly this far, whatever the arithmetic says;
    /// the relation is a straight line in log space and runs away above M9.
    const MAX_RADIUS_M: f64 = 1_000_000.0;

    if !magnitude.is_finite() || magnitude < MIN_MAGNITUDE {
        return None;
    }
    let km = 10f64.powf((magnitude - 1.5) / 2.5);
    Some((km * 1000.0).min(MAX_RADIUS_M))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn km(m: Option<f64>) -> f64 {
        m.expect("a radius") / 1000.0
    }

    #[test]
    fn radii_land_where_the_documented_table_says() {
        assert!((km(felt_radius_m(4.0)) - 10.0).abs() < 0.5);
        assert!((km(felt_radius_m(5.0)) - 25.1).abs() < 0.5);
        assert!((km(felt_radius_m(6.0)) - 63.1).abs() < 1.0);
        assert!((km(felt_radius_m(7.0)) - 158.5).abs() < 2.0);
    }

    #[test]
    fn small_events_get_no_circle_at_all() {
        // Below M2.5 the felt area is a few hundred metres. A circle there
        // claims a precision the relation does not have, so these stay points.
        assert!(felt_radius_m(2.4).is_none());
        assert!(felt_radius_m(0.5).is_none());
        assert!(felt_radius_m(f64::NAN).is_none());
        assert!(felt_radius_m(2.5).is_some());
    }

    #[test]
    fn the_relation_grows_with_magnitude_until_the_bound() {
        // Strictly increasing up to the clamp, and never beyond it. The clamp
        // engages at M9 — a straight line in log space would put M10 at
        // 2,500 km, which is a quarter of the way round the planet and not a
        // claim this relation can support.
        let mut previous = 0.0;
        for tenth in 25..=90 {
            let radius = felt_radius_m(f64::from(tenth) / 10.0).expect("above the floor");
            assert!(radius > previous, "radius must grow with magnitude");
            previous = radius;
        }
        assert!((felt_radius_m(9.0).unwrap() - 1_000_000.0).abs() < 1.0);
        assert_eq!(felt_radius_m(10.0), Some(1_000_000.0), "bounded above M9");
        assert_eq!(felt_radius_m(12.0), Some(1_000_000.0));
    }
}
