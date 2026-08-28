//! Coordinate transforms for orbital propagation.
//!
//! SGP4 answers in the TEME frame (True Equator, Mean Equinox) — an inertial
//! frame that does not rotate with the Earth. Turning that into a latitude and
//! longitude takes two steps: rotate into an Earth-fixed frame by the sidereal
//! angle, then convert Cartesian to geodetic on the WGS-84 ellipsoid.
//!
//! Getting the sidereal angle wrong is the classic failure here, and it is
//! quietly wrong rather than obviously wrong: the orbit still looks like an
//! orbit, it is simply in the wrong place, and by a smoothly varying amount that
//! reads as drift rather than as a bug.

use chrono::{DateTime, Datelike, Timelike, Utc};

/// WGS-84 semi-major axis, metres.
pub const WGS84_A: f64 = 6_378_137.0;
/// WGS-84 flattening.
pub const WGS84_F: f64 = 1.0 / 298.257_223_563;

/// First eccentricity squared.
fn e2() -> f64 {
    2.0 * WGS84_F - WGS84_F * WGS84_F
}

/// Julian date from a UTC instant.
///
/// UT1 is what sidereal time is strictly defined against, but UT1−UTC is bounded
/// to under a second by leap seconds, which is ~460 m of rotation at the
/// equator. That is far below the error already present in a public element set
/// propagated over hours, so UTC is used directly.
pub fn julian_date(t: DateTime<Utc>) -> f64 {
    let (y, m) = (t.year() as f64, t.month() as f64);
    let d = t.day() as f64;
    let (y, m) = if m <= 2.0 { (y - 1.0, m + 12.0) } else { (y, m) };
    let a = (y / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    let day_fraction = (t.hour() as f64
        + t.minute() as f64 / 60.0
        + (t.second() as f64 + f64::from(t.nanosecond()) / 1e9) / 3600.0)
        / 24.0;
    (365.25 * (y + 4716.0)).floor() + (30.6001 * (m + 1.0)).floor() + d + b - 1524.5
        + day_fraction
}

/// Greenwich Mean Sidereal Time, radians in `0..2π`.
///
/// IAU 1982 polynomial, the same one SGP4 implementations conventionally pair
/// with TEME.
pub fn gmst_rad(t: DateTime<Utc>) -> f64 {
    let tc = (julian_date(t) - 2_451_545.0) / 36_525.0;
    // Seconds of sidereal time.
    let mut secs = 67_310.548_41
        + (876_600.0 * 3600.0 + 8_640_184.812_866) * tc
        + 0.093_104 * tc * tc
        - 6.2e-6 * tc * tc * tc;
    secs = secs.rem_euclid(86_400.0);
    // 86400 sidereal seconds span 2π.
    let rad = secs * std::f64::consts::TAU / 86_400.0;
    rad.rem_euclid(std::f64::consts::TAU)
}

/// Rotate a TEME position into an Earth-fixed frame.
///
/// Pure rotation about the z axis by −GMST; TEME and ECEF share an origin and a
/// polar axis. Polar motion is neglected — it is a few tens of metres, well
/// under the propagation error.
pub fn teme_to_ecef(x: f64, y: f64, z: f64, gmst: f64) -> (f64, f64, f64) {
    let (s, c) = gmst.sin_cos();
    (x * c + y * s, -x * s + y * c, z)
}

/// Earth-fixed Cartesian to geodetic latitude, longitude and height.
///
/// Bowring's method: accurate to well under a millimetre for near-Earth orbits
/// and closed-form, so there is no iteration to fail to converge.
///
/// Returns `(lat_deg, lon_deg, alt_m)`, with altitude above the WGS-84
/// ellipsoid.
pub fn ecef_to_geodetic(x: f64, y: f64, z: f64) -> (f64, f64, f64) {
    let e2 = e2();
    let b = WGS84_A * (1.0 - WGS84_F);
    // Second eccentricity squared.
    let ep2 = (WGS84_A * WGS84_A - b * b) / (b * b);

    let p = (x * x + y * y).sqrt();
    let lon = y.atan2(x);

    // Directly over a pole, p collapses to zero and the parametric latitude is
    // undefined. Answer on the axis rather than returning NaN.
    if p < 1e-9 {
        let lat = if z >= 0.0 {
            std::f64::consts::FRAC_PI_2
        } else {
            -std::f64::consts::FRAC_PI_2
        };
        return (lat.to_degrees(), lon.to_degrees(), z.abs() - b);
    }

    let theta = (z * WGS84_A).atan2(p * b);
    let (st, ct) = theta.sin_cos();
    let lat = (z + ep2 * b * st * st * st).atan2(p - e2 * WGS84_A * ct * ct * ct);
    let n = WGS84_A / (1.0 - e2 * lat.sin() * lat.sin()).sqrt();
    let alt = p / lat.cos() - n;

    (lat.to_degrees(), lon.to_degrees(), alt)
}

/// TEME kilometres straight to geodetic degrees and metres.
pub fn teme_to_geodetic(
    x_km: f64,
    y_km: f64,
    z_km: f64,
    t: DateTime<Utc>,
) -> (f64, f64, f64) {
    let gmst = gmst_rad(t);
    let (x, y, z) = teme_to_ecef(x_km * 1000.0, y_km * 1000.0, z_km * 1000.0, gmst);
    ecef_to_geodetic(x, y, z)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn julian_date_matches_the_j2000_epoch() {
        // J2000.0 is 2000-01-01 12:00:00 TT = JD 2451545.0.
        let t = Utc.with_ymd_and_hms(2000, 1, 1, 12, 0, 0).unwrap();
        assert!(approx(julian_date(t), 2_451_545.0, 1e-6), "{}", julian_date(t));
    }

    #[test]
    fn julian_date_advances_by_one_per_day() {
        let a = Utc.with_ymd_and_hms(2026, 8, 28, 0, 0, 0).unwrap();
        let b = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).unwrap();
        assert!(approx(julian_date(b) - julian_date(a), 1.0, 1e-9));
    }

    #[test]
    fn gmst_is_in_range_and_advances_a_little_more_than_a_turn_per_day() {
        let t = Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, 0).unwrap();
        let g = gmst_rad(t);
        assert!((0.0..std::f64::consts::TAU).contains(&g), "out of range: {g}");

        // A sidereal day is ~236 s shorter than a solar day, so over 24 h of
        // civil time GMST gains that much extra rotation.
        let a = gmst_rad(t);
        let b = gmst_rad(t + chrono::Duration::days(1));
        let gained = (b - a).rem_euclid(std::f64::consts::TAU);
        let expected = 236.0 * std::f64::consts::TAU / 86_400.0;
        assert!(approx(gained, expected, 1e-3), "gained {gained}, want {expected}");
    }

    #[test]
    fn ecef_round_trips_through_geodetic_on_the_equator() {
        // A point on the equatorial surface at 0°E sits on the +x axis.
        let (lat, lon, alt) = ecef_to_geodetic(WGS84_A, 0.0, 0.0);
        assert!(approx(lat, 0.0, 1e-9));
        assert!(approx(lon, 0.0, 1e-9));
        assert!(approx(alt, 0.0, 1e-6));
    }

    #[test]
    fn the_poles_are_flattened_by_exactly_the_wgs84_amount() {
        let b = WGS84_A * (1.0 - WGS84_F);
        let (lat, _, alt) = ecef_to_geodetic(0.0, 0.0, b);
        assert!(approx(lat, 90.0, 1e-9), "lat {lat}");
        assert!(approx(alt, 0.0, 1e-6), "alt {alt}");
        // And the polar radius really is ~21.4 km shorter than the equatorial.
        assert!(approx(WGS84_A - b, 21_384.7, 1.0));
    }

    #[test]
    fn a_point_directly_over_a_pole_does_not_produce_nan() {
        // p collapses to zero on the axis; the closed form divides by it.
        let (lat, lon, alt) = ecef_to_geodetic(0.0, 0.0, 7_000_000.0);
        assert!(lat.is_finite() && lon.is_finite() && alt.is_finite());
        assert!(approx(lat, 90.0, 1e-9));
    }

    #[test]
    fn a_known_altitude_survives_the_round_trip() {
        // 400 km over the equator at 45°E — roughly ISS altitude.
        let lat0 = 0.0_f64;
        let lon0 = 45.0_f64;
        let alt0 = 400_000.0;
        let n = WGS84_A / (1.0 - e2() * lat0.to_radians().sin().powi(2)).sqrt();
        let x = (n + alt0) * lat0.to_radians().cos() * lon0.to_radians().cos();
        let y = (n + alt0) * lat0.to_radians().cos() * lon0.to_radians().sin();
        let z = (n * (1.0 - e2()) + alt0) * lat0.to_radians().sin();

        let (lat, lon, alt) = ecef_to_geodetic(x, y, z);
        assert!(approx(lat, lat0, 1e-9), "lat {lat}");
        assert!(approx(lon, lon0, 1e-9), "lon {lon}");
        assert!(approx(alt, alt0, 1e-3), "alt {alt}");
    }

    #[test]
    fn the_earth_rotation_moves_longitude_not_latitude() {
        // The same inertial position six hours later must sit ~90° further
        // west in Earth-fixed terms, at essentially the same latitude. This is
        // the check that catches a sidereal angle applied in the wrong
        // direction — the failure mode that reads as slow drift, not as a bug.
        let t0 = Utc.with_ymd_and_hms(2026, 8, 28, 0, 0, 0).unwrap();
        let (x, y, z) = (6_771.0, 0.0, 0.0); // km, on the +x axis
        let (lat0, lon0, alt0) = teme_to_geodetic(x, y, z, t0);
        let (lat1, lon1, alt1) =
            teme_to_geodetic(x, y, z, t0 + chrono::Duration::hours(6));

        assert!(approx(lat0, lat1, 1e-6), "latitude moved: {lat0} -> {lat1}");
        assert!(approx(alt0, alt1, 1e-3), "altitude moved");
        let delta = (lon0 - lon1).rem_euclid(360.0);
        assert!(
            approx(delta, 90.0, 0.5),
            "expected ~90° of westward rotation, got {delta}"
        );
    }
}
