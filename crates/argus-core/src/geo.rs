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

/// British National Grid (EPSG:27700, OSGB36 on the Airy 1830 ellipsoid) to
/// WGS-84 longitude and latitude.
///
/// Two steps: the inverse Transverse Mercator projection to OSGB36
/// geodetic coordinates, then a seven-parameter Helmert transformation to
/// WGS-84 via geocentric coordinates. Ordnance Survey's own guide gives the
/// parameters; the Helmert step is accurate to about five metres across
/// Great Britain, which is the difference between a region boundary and a
/// property boundary and is fine for the former. Several UK public feeds
/// serve grid coordinates, and read as degrees they put Scotland in the Gulf
/// of Guinea.
pub fn bng_to_wgs84(easting: f64, northing: f64) -> (f64, f64) {
    // Airy 1830.
    let a = 6_377_563.396;
    let b = 6_356_256.909;
    let f0 = 0.999_601_271_7;
    let (lat0, lon0) = (49.0_f64.to_radians(), (-2.0_f64).to_radians());
    let (n0, e0) = (-100_000.0, 400_000.0);
    let e2 = 1.0 - (b * b) / (a * a);
    let n = (a - b) / (a + b);

    // Iterate the meridional arc to find the footpoint latitude.
    let mut lat = lat0;
    let mut m = 0.0;
    loop {
        lat += (northing - n0 - m) / (a * f0);
        let (n2, n3) = (n * n, n * n * n);
        let ma = (1.0 + n + 1.25 * n2 + 1.25 * n3) * (lat - lat0);
        let mb = (3.0 * n + 3.0 * n2 + 2.625 * n3) * (lat - lat0).sin() * (lat + lat0).cos();
        let mc = (1.875 * n2 + 1.875 * n3) * (2.0 * (lat - lat0)).sin() * (2.0 * (lat + lat0)).cos();
        let md = 35.0 / 24.0 * n3 * (3.0 * (lat - lat0)).sin() * (3.0 * (lat + lat0)).cos();
        m = b * f0 * (ma - mb + mc - md);
        if (northing - n0 - m).abs() < 0.000_01 {
            break;
        }
    }

    let (sin_lat, cos_lat, tan_lat) = (lat.sin(), lat.cos(), lat.tan());
    let nu = a * f0 / (1.0 - e2 * sin_lat * sin_lat).sqrt();
    let rho = a * f0 * (1.0 - e2) / (1.0 - e2 * sin_lat * sin_lat).powf(1.5);
    let eta2 = nu / rho - 1.0;
    let (tan2, tan4) = (tan_lat * tan_lat, tan_lat.powi(4));
    let (nu3, nu5, nu7) = (nu.powi(3), nu.powi(5), nu.powi(7));

    let vii = tan_lat / (2.0 * rho * nu);
    let viii = tan_lat / (24.0 * rho * nu3) * (5.0 + 3.0 * tan2 + eta2 - 9.0 * tan2 * eta2);
    let ix = tan_lat / (720.0 * rho * nu5) * (61.0 + 90.0 * tan2 + 45.0 * tan4);
    let x = 1.0 / (cos_lat * nu);
    let xi = 1.0 / (cos_lat * 6.0 * nu3) * (nu / rho + 2.0 * tan2);
    let xii = 1.0 / (cos_lat * 120.0 * nu5) * (5.0 + 28.0 * tan2 + 24.0 * tan4);
    let xiia = 1.0 / (cos_lat * 5040.0 * nu7) * (61.0 + 662.0 * tan2 + 1320.0 * tan4 + 720.0 * tan2 * tan4);

    let de = easting - e0;
    let (de2, de3, de4, de5, de6, de7) = (de * de, de.powi(3), de.powi(4), de.powi(5), de.powi(6), de.powi(7));
    let lat_osgb = lat - vii * de2 + viii * de4 - ix * de6;
    let lon_osgb = lon0 + x * de - xi * de3 + xii * de5 - xiia * de7;

    // OSGB36 geodetic to geocentric, Helmert to WGS-84, back to geodetic.
    let (sin_lat, cos_lat) = (lat_osgb.sin(), lat_osgb.cos());
    let nu = a / (1.0 - e2 * sin_lat * sin_lat).sqrt();
    let x1 = nu * cos_lat * lon_osgb.cos();
    let y1 = nu * cos_lat * lon_osgb.sin();
    let z1 = (1.0 - e2) * nu * sin_lat;

    // OSGB36 → WGS84: the inverse of the published WGS84 → OSGB36 set.
    let (tx, ty, tz) = (446.448, -125.157, 542.060);
    let (rx, ry, rz) = (
        (0.1502 / 3600.0_f64).to_radians(),
        (0.2470 / 3600.0_f64).to_radians(),
        (0.8421 / 3600.0_f64).to_radians(),
    );
    let sc = 1.0 + (-20.4894 * 1e-6);
    let x2 = tx + sc * x1 - rz * y1 + ry * z1;
    let y2 = ty + rz * x1 + sc * y1 - rx * z1;
    let z2 = tz - ry * x1 + rx * y1 + sc * z1;

    // GRS80 / WGS-84.
    let a = 6_378_137.0;
    let b = 6_356_752.314_2;
    let e2 = 1.0 - (b * b) / (a * a);
    let p = (x2 * x2 + y2 * y2).sqrt();
    let mut lat = (z2 / (p * (1.0 - e2))).atan();
    for _ in 0..10 {
        let nu = a / (1.0 - e2 * lat.sin() * lat.sin()).sqrt();
        lat = ((z2 + e2 * nu * lat.sin()) / p).atan();
    }
    let lon = y2.atan2(x2);
    (lon.to_degrees(), lat.to_degrees())
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
    fn the_airy_transit_circle_lands_a_hundred_metres_east_of_the_wgs84_meridian() {
        // Greenwich Observatory's transit circle is at 0° in OSGB36 by
        // definition and about 102 m east of 0° in WGS-84 — one of the
        // best-known facts about the two datums. Ordnance Survey's worked
        // example for the same point: E 538 890, N 177 320.
        let (lon, lat) = bng_to_wgs84(538_890.0, 177_320.0);
        assert!((lon - -0.0015).abs() < 0.0005, "lon {lon}");
        assert!((lat - 51.4778).abs() < 0.0005, "lat {lat}");

        // Ordnance Survey's worked inverse example, Caister water tower:
        // E 651 409.903, N 313 177.270 is 52°39′27.2531″N 1°43′4.5177″E in
        // OSGB36; in WGS-84 it moves roughly 0.0015° north and 0.0012° west.
        let (lon, lat) = bng_to_wgs84(651_409.903, 313_177.270);
        assert!((lat - 52.6577).abs() < 0.001, "lat {lat}");
        assert!((lon - 1.7168).abs() < 0.001, "lon {lon}");
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
