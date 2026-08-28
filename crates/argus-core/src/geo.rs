//! Geographic primitives shared across crates: bounding boxes, great-circle
//! distance, and the web-mercator tile maths the MVT server needs.

use crate::entity::Position;
use serde::{Deserialize, Serialize};

/// Mean Earth radius (IUGG), metres. Adequate for the distance work here;
/// anything needing better than ~0.5% error should use a geodesic solver.
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// An axis-aligned longitude/latitude box.
///
/// May cross the antimeridian, in which case `west > east`. Every consumer must
/// handle that — a naive `lon >= west && lon <= east` silently returns nothing
/// for a box over the Pacific, and that bug is invisible until someone watches
/// shipping near Fiji.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BoundingBox {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

impl BoundingBox {
    pub fn new(west: f64, south: f64, east: f64, north: f64) -> Self {
        Self {
            west,
            south,
            east,
            north,
        }
    }

    /// The whole planet.
    pub const GLOBAL: Self = Self {
        west: -180.0,
        south: -90.0,
        east: 180.0,
        north: 90.0,
    };

    /// Whether this box wraps across the antimeridian.
    pub fn crosses_antimeridian(&self) -> bool {
        self.west > self.east
    }

    pub fn contains(&self, lon: f64, lat: f64) -> bool {
        if lat < self.south || lat > self.north {
            return false;
        }
        if self.crosses_antimeridian() {
            lon >= self.west || lon <= self.east
        } else {
            lon >= self.west && lon <= self.east
        }
    }

    pub fn contains_position(&self, p: &Position) -> bool {
        self.contains(p.lon, p.lat)
    }

    /// Grow the box by a margin in degrees, clamped to valid latitudes.
    /// Longitude is deliberately not clamped: a widened box may legitimately
    /// come to cross the antimeridian, and `contains` handles that.
    #[must_use]
    pub fn expanded(&self, degrees: f64) -> Self {
        Self {
            west: normalize_lon(self.west - degrees),
            south: (self.south - degrees).max(-90.0),
            east: normalize_lon(self.east + degrees),
            north: (self.north + degrees).min(90.0),
        }
    }

    /// Split an antimeridian-crossing box into two ordinary ones, so it can be
    /// handed to a database or an upstream API that cannot express the wrap.
    pub fn split_at_antimeridian(&self) -> Vec<Self> {
        if self.crosses_antimeridian() {
            vec![
                Self::new(self.west, self.south, 180.0, self.north),
                Self::new(-180.0, self.south, self.east, self.north),
            ]
        } else {
            vec![*self]
        }
    }
}

/// Wrap a longitude into `-180.0..=180.0`.
pub fn normalize_lon(lon: f64) -> f64 {
    let mut l = (lon + 180.0) % 360.0;
    if l < 0.0 {
        l += 360.0;
    }
    l - 180.0
}

/// Great-circle distance in metres.
pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().asin()
}

/// Initial bearing in degrees from north, travelling from point 1 to point 2.
pub fn initial_bearing_deg(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dlon = (lon2 - lon1).to_radians();
    let y = dlon.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dlon.cos();
    (y.atan2(x).to_degrees() + 360.0) % 360.0
}

/// Move from a point along a bearing by a distance, on a sphere. This is the
/// dead-reckoning primitive: between polls, entities advance along their course
/// with this.
pub fn destination(lat: f64, lon: f64, bearing_deg: f64, distance_m: f64) -> (f64, f64) {
    let ang = distance_m / EARTH_RADIUS_M;
    let brg = bearing_deg.to_radians();
    let p1 = lat.to_radians();
    let l1 = lon.to_radians();

    let p2 = (p1.sin() * ang.cos() + p1.cos() * ang.sin() * brg.cos()).asin();
    let l2 = l1 + (brg.sin() * ang.sin() * p1.cos()).atan2(ang.cos() - p1.sin() * p2.sin());

    (p2.to_degrees(), normalize_lon(l2.to_degrees()))
}

/// Web-mercator tile coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TileCoord {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileCoord {
    pub fn new(z: u8, x: u32, y: u32) -> Self {
        Self { z, x, y }
    }

    /// Whether x and y are inside the valid range for this zoom. The tiler must
    /// reject out-of-range requests rather than computing a nonsense box.
    pub fn is_valid(&self) -> bool {
        if self.z > 24 {
            return false;
        }
        let n = 1u64 << self.z;
        u64::from(self.x) < n && u64::from(self.y) < n
    }

    /// Geographic extent of this tile.
    pub fn bounds(&self) -> BoundingBox {
        let n = f64::from(1u32 << self.z);
        let west = f64::from(self.x) / n * 360.0 - 180.0;
        let east = f64::from(self.x + 1) / n * 360.0 - 180.0;
        // Mercator y is non-linear in latitude, hence the sinh.
        let north = merc_y_to_lat(1.0 - 2.0 * f64::from(self.y) / n);
        let south = merc_y_to_lat(1.0 - 2.0 * f64::from(self.y + 1) / n);
        BoundingBox::new(west, south, east, north)
    }
}

fn merc_y_to_lat(y: f64) -> f64 {
    (std::f64::consts::PI * y).sinh().atan().to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn antimeridian_boxes_contain_points_on_both_sides() {
        // The bug this exists to prevent: a box from 170E to -170E covers Fiji,
        // and a naive range check returns nothing at all for it.
        let bbox = BoundingBox::new(170.0, -20.0, -170.0, 20.0);
        assert!(bbox.crosses_antimeridian());
        assert!(bbox.contains(175.0, 0.0));
        assert!(bbox.contains(-175.0, 0.0));
        assert!(!bbox.contains(0.0, 0.0));
        assert!(!bbox.contains(175.0, 30.0));
    }

    #[test]
    fn ordinary_boxes_are_unaffected() {
        let bbox = BoundingBox::new(-98.0, 30.0, -97.0, 31.0);
        assert!(!bbox.crosses_antimeridian());
        assert!(bbox.contains(-97.74, 30.27));
        assert!(!bbox.contains(-96.0, 30.5));
    }

    #[test]
    fn crossing_boxes_split_into_two_queryable_halves() {
        let parts = BoundingBox::new(170.0, -20.0, -170.0, 20.0).split_at_antimeridian();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], BoundingBox::new(170.0, -20.0, 180.0, 20.0));
        assert_eq!(parts[1], BoundingBox::new(-180.0, -20.0, -170.0, 20.0));
        // A normal box passes through untouched.
        assert_eq!(
            BoundingBox::new(-98.0, 30.0, -97.0, 31.0)
                .split_at_antimeridian()
                .len(),
            1
        );
    }

    #[test]
    fn longitude_normalisation_wraps_both_directions() {
        assert!(approx(normalize_lon(190.0), -170.0, 1e-9));
        assert!(approx(normalize_lon(-190.0), 170.0, 1e-9));
        assert!(approx(normalize_lon(0.0), 0.0, 1e-9));
        assert!(approx(normalize_lon(540.0), -180.0, 1e-9));
    }

    #[test]
    fn haversine_matches_a_known_pair() {
        // LAX to JFK is ~3,974 km.
        let d = haversine_m(33.9416, -118.4085, 40.6413, -73.7781);
        assert!(approx(d / 1000.0, 3974.0, 10.0), "got {} km", d / 1000.0);
        assert_eq!(haversine_m(30.0, -97.0, 30.0, -97.0), 0.0);
    }

    #[test]
    fn dead_reckoning_round_trips_through_bearing_and_distance() {
        // Advance 10 km on 45°, then confirm distance and bearing come back.
        let (lat, lon) = destination(30.0, -97.0, 45.0, 10_000.0);
        assert!(approx(haversine_m(30.0, -97.0, lat, lon), 10_000.0, 1.0));
        assert!(approx(initial_bearing_deg(30.0, -97.0, lat, lon), 45.0, 0.05));
    }

    #[test]
    fn dead_reckoning_across_the_antimeridian_stays_in_range() {
        let (_, lon) = destination(0.0, 179.9, 90.0, 50_000.0);
        assert!((-180.0..=180.0).contains(&lon), "lon escaped range: {lon}");
        assert!(lon < 0.0, "should have wrapped to negative, got {lon}");
    }

    #[test]
    fn tile_zero_covers_the_world() {
        let b = TileCoord::new(0, 0, 0).bounds();
        assert!(approx(b.west, -180.0, 1e-6));
        assert!(approx(b.east, 180.0, 1e-6));
        // Web mercator clips at ~85.05°, not at the poles.
        assert!(approx(b.north, 85.0511, 1e-3));
        assert!(approx(b.south, -85.0511, 1e-3));
    }

    #[test]
    fn out_of_range_tiles_are_rejected() {
        assert!(TileCoord::new(0, 0, 0).is_valid());
        assert!(TileCoord::new(1, 1, 1).is_valid());
        assert!(!TileCoord::new(1, 2, 0).is_valid());
        assert!(!TileCoord::new(0, 0, 1).is_valid());
        assert!(!TileCoord::new(25, 0, 0).is_valid());
    }

    #[test]
    fn adjacent_tiles_tile_without_gaps() {
        let left = TileCoord::new(4, 5, 6).bounds();
        let right = TileCoord::new(4, 6, 6).bounds();
        assert!(approx(left.east, right.west, 1e-9));
        let below = TileCoord::new(4, 5, 7).bounds();
        assert!(approx(left.south, below.north, 1e-9));
    }
}
