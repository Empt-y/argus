//! Ground elevation, served from a grid on this machine.
//!
//! The globe's terrain is normally somebody else's web service. Argus exists
//! because services go away, and because a system watching the sky over your
//! own house should not need an account somewhere to know how high the ground
//! is. England publishes 1 m LiDAR under the Open Government Licence — thirty
//! times finer than the global terrain it replaces — so self-hosting here is an
//! upgrade rather than a sacrifice.
//!
//! The grid is prepared offline by `tools/terrain/prepare_terrain.py`, which
//! leaves a flat `f32` array and a JSON sidecar. That the daemon needs no
//! geospatial library to read it is the point: answering "how high is the
//! ground here" is a multiply and an index.
//!
//! **The heights are orthometric.** The Environment Agency publishes above
//! Ordnance Datum Newlyn, which is a geoid, and Cesium wants heights above the
//! ellipsoid — a difference of about 46 m in southern England, or roughly the
//! height of a fifteen-storey building. Converting needs a geoid model, so the
//! datum travels with the data (see [`DemMeta::datum`]) and the client, which
//! already carries a verified EGM96 implementation, applies it. Serving these
//! numbers as if they were ellipsoidal would bury everything drawn on them.

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum DemError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

/// Georeferencing for a prepared grid, as written by the preparation tool.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DemMeta {
    pub width: usize,
    pub height: usize,
    /// North-west pixel corner, degrees.
    pub west: f64,
    pub north: f64,
    /// Degrees per pixel. `lat_step` is negative: rows run north to south.
    pub lon_step: f64,
    pub lat_step: f64,
    pub east: f64,
    pub south: f64,
    pub nodata: f32,
    /// `"orthometric"` or `"ellipsoidal"`. The client must not guess.
    pub datum: String,
    #[serde(default)]
    pub attribution: String,
    #[serde(default)]
    pub ground_metres: f64,
}

/// A loaded elevation grid.
pub struct Dem {
    meta: DemMeta,
    heights: Vec<f32>,
}

impl Dem {
    /// Load `<base>.json` and `<base>.bin`.
    pub fn load(base: &Path) -> Result<Self, DemError> {
        let meta_path = base.with_extension("json");
        let bin_path = base.with_extension("bin");

        let text = std::fs::read_to_string(&meta_path).map_err(|source| DemError::Read {
            path: meta_path.display().to_string(),
            source,
        })?;
        let meta: DemMeta = serde_json::from_str(&text).map_err(|source| DemError::Parse {
            path: meta_path.display().to_string(),
            source,
        })?;

        let bytes = std::fs::read(&bin_path).map_err(|source| DemError::Read {
            path: bin_path.display().to_string(),
            source,
        })?;
        let expected = meta.width * meta.height * 4;
        if bytes.len() != expected {
            return Err(DemError::Invalid(format!(
                "{} is {} bytes, expected {} for {}x{} f32",
                bin_path.display(),
                bytes.len(),
                expected,
                meta.width,
                meta.height
            )));
        }
        if meta.lat_step >= 0.0 || meta.lon_step <= 0.0 {
            return Err(DemError::Invalid(
                "grid must run west to east and north to south".into(),
            ));
        }

        let heights = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Ok(Self { meta, heights })
    }

    pub fn meta(&self) -> &DemMeta {
        &self.meta
    }

    /// Whether a point is inside the grid's footprint at all.
    pub fn contains(&self, lon: f64, lat: f64) -> bool {
        lon >= self.meta.west
            && lon <= self.meta.east
            && lat <= self.meta.north
            && lat >= self.meta.south
    }

    /// Whether a rectangle lies wholly inside the footprint.
    ///
    /// Whole, not partial, and the strictness is deliberate: a tile that is
    /// half covered would have to invent the other half, and a seam between two
    /// honest sources is better than a tile that is partly made up.
    pub fn covers(&self, west: f64, south: f64, east: f64, north: f64) -> bool {
        west >= self.meta.west
            && east <= self.meta.east
            && north <= self.meta.north
            && south >= self.meta.south
    }

    fn at(&self, col: usize, row: usize) -> Option<f32> {
        let v = *self.heights.get(row * self.meta.width + col)?;
        // Nodata is a real answer over water and outside the survey, and it is
        // a large negative sentinel — averaging it into a neighbour would carve
        // a trench through the coastline.
        if !v.is_finite() || (v - self.meta.nodata).abs() < 0.5 {
            None
        } else {
            Some(v)
        }
    }

    /// Bilinearly interpolated height, or `None` outside the grid or over
    /// nodata.
    pub fn sample(&self, lon: f64, lat: f64) -> Option<f32> {
        if !self.contains(lon, lat) {
            return None;
        }
        // Pixel centres sit half a step in from the declared corner.
        let x = (lon - self.meta.west) / self.meta.lon_step - 0.5;
        let y = (lat - self.meta.north) / self.meta.lat_step - 0.5;
        let x0 = x.floor();
        let y0 = y.floor();
        let fx = (x - x0) as f32;
        let fy = (y - y0) as f32;

        let cx = (x0.max(0.0) as usize).min(self.meta.width.saturating_sub(1));
        let cy = (y0.max(0.0) as usize).min(self.meta.height.saturating_sub(1));
        let cx1 = (cx + 1).min(self.meta.width - 1);
        let cy1 = (cy + 1).min(self.meta.height - 1);

        let q = [
            self.at(cx, cy),
            self.at(cx1, cy),
            self.at(cx, cy1),
            self.at(cx1, cy1),
        ];
        // A cell touching nodata falls back to the nearest neighbour that has a
        // value rather than blending toward the sentinel.
        if q.iter().any(Option::is_none) {
            return q.into_iter().flatten().next();
        }
        let (v00, v10, v01, v11) = (q[0]?, q[1]?, q[2]?, q[3]?);
        let top = v00 + (v10 - v00) * fx;
        let bottom = v01 + (v11 - v01) * fx;
        Some(top + (bottom - top) * fy)
    }

    /// A `size` × `size` height grid over a tile rectangle, row-major from the
    /// north-west corner — the layout Cesium's heightmap terrain wants.
    ///
    /// Gaps are filled with the mean of the tile's valid samples, so a lake in
    /// the middle of a tile reads as flat rather than as a pit. A tile with no
    /// valid sample at all returns `None`, which the caller should treat as
    /// "not covered" rather than as a tile of zeroes at the bottom of the sea.
    pub fn heightmap(
        &self,
        west: f64,
        south: f64,
        east: f64,
        north: f64,
        size: usize,
    ) -> Option<Vec<f32>> {
        if size < 2 {
            return None;
        }
        let mut out = Vec::with_capacity(size * size);
        let mut sum = 0.0f64;
        let mut valid = 0usize;
        for row in 0..size {
            // Inclusive of both edges, so neighbouring tiles agree along a seam.
            let lat = north + (south - north) * (row as f64) / ((size - 1) as f64);
            for col in 0..size {
                let lon = west + (east - west) * (col as f64) / ((size - 1) as f64);
                match self.sample(lon, lat) {
                    Some(v) => {
                        sum += f64::from(v);
                        valid += 1;
                        out.push(v);
                    }
                    None => out.push(f32::NAN),
                }
            }
        }
        if valid == 0 {
            return None;
        }
        let mean = (sum / valid as f64) as f32;
        for v in &mut out {
            if v.is_nan() {
                *v = mean;
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A 3x3 grid over 0..3 degrees east, 0..-3 south, heights 0..8.
    fn fixture() -> (tempdir::Guard, std::path::PathBuf) {
        let dir = tempdir::Guard::new();
        let base = dir.path().join("t");
        let meta = serde_json::json!({
            "width": 3, "height": 3,
            "west": 0.0, "north": 3.0,
            "lon_step": 1.0, "lat_step": -1.0,
            "east": 3.0, "south": 0.0,
            "nodata": -9999.0,
            "datum": "orthometric",
        });
        std::fs::write(base.with_extension("json"), meta.to_string()).unwrap();
        let mut f = std::fs::File::create(base.with_extension("bin")).unwrap();
        for v in 0..9u32 {
            f.write_all(&(v as f32).to_le_bytes()).unwrap();
        }
        (dir, base)
    }

    #[test]
    fn samples_a_pixel_centre_exactly() {
        let (_g, base) = fixture();
        let dem = Dem::load(&base).unwrap();
        // Centre of the top-left pixel is (0.5, 2.5) and holds height 0.
        assert!((dem.sample(0.5, 2.5).unwrap() - 0.0).abs() < 1e-5);
        // Centre of the middle pixel holds height 4.
        assert!((dem.sample(1.5, 1.5).unwrap() - 4.0).abs() < 1e-5);
    }

    #[test]
    fn interpolates_between_centres() {
        let (_g, base) = fixture();
        let dem = Dem::load(&base).unwrap();
        // Halfway between heights 0 and 1 along the top row.
        assert!((dem.sample(1.0, 2.5).unwrap() - 0.5).abs() < 1e-5);
    }

    #[test]
    fn refuses_points_outside_the_grid() {
        let (_g, base) = fixture();
        let dem = Dem::load(&base).unwrap();
        assert!(dem.sample(3.5, 1.5).is_none());
        assert!(dem.sample(1.5, -0.5).is_none());
        assert!(!dem.covers(-0.1, 0.0, 1.0, 1.0));
        assert!(dem.covers(0.5, 0.5, 2.5, 2.5));
    }

    #[test]
    fn heightmap_corners_match_the_grid() {
        let (_g, base) = fixture();
        let dem = Dem::load(&base).unwrap();
        let hm = dem.heightmap(0.5, 0.5, 2.5, 2.5, 3).unwrap();
        assert_eq!(hm.len(), 9);
        // North-west corner first: the sample at (0.5, 2.5) is height 0.
        assert!((hm[0] - 0.0).abs() < 1e-5);
        // South-east corner last: (2.5, 0.5) is height 8.
        assert!((hm[8] - 8.0).abs() < 1e-5);
    }

    #[test]
    fn rejects_a_truncated_grid() {
        let (_g, base) = fixture();
        std::fs::write(base.with_extension("bin"), [0u8; 8]).unwrap();
        assert!(matches!(Dem::load(&base), Err(DemError::Invalid(_))));
    }

    /// A scratch directory that removes itself, so the tests need no crate.
    mod tempdir {
        pub struct Guard(std::path::PathBuf);
        impl Guard {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "argus-dem-test-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
