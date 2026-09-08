//! GeoJSON geometry, converted into the `geo_types` shapes the store speaks.
//!
//! This lives here rather than in a driver because it is the second time it was
//! needed: the NWS alert feed carries inline alert polygons, and the Environment
//! Agency serves flood area outlines as standalone FeatureCollections. Both are
//! the same handful of shapes and the same conversion, and two copies of a
//! ring-winding helper is how they quietly stop agreeing.
//!
//! Only the area shapes are modelled. Every consumer so far wants a polygon —
//! an alert area, a flood extent — and a `Point` variant that nothing
//! constructs would be dead code that still has to be maintained.

use geo_types::{Coord, Geometry, LineString, MultiPolygon, Polygon};
use serde::Deserialize;

/// The area geometries the feeds actually emit.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum GeoJsonGeometry {
    Polygon { coordinates: Vec<Vec<[f64; 2]>> },
    MultiPolygon { coordinates: Vec<Vec<Vec<[f64; 2]>>> },
}

/// A `FeatureCollection` wrapping one or more geometries.
///
/// The Environment Agency serves each flood area this way — a collection of one
/// feature whose geometry is the outline — where NWS puts the geometry inline
/// on the alert.
#[derive(Debug, Deserialize)]
pub struct FeatureCollection {
    #[serde(default)]
    pub features: Vec<GeoJsonFeature>,
}

#[derive(Debug, Deserialize)]
pub struct GeoJsonFeature {
    pub geometry: Option<GeoJsonGeometry>,
}

impl FeatureCollection {
    /// Every geometry in the collection, merged into one shape.
    ///
    /// A flood area is one place even when it is drawn as several disjoint
    /// polygons, so the parts are combined rather than the first one being
    /// picked and the rest dropped — which would silently shrink the area a
    /// warning covers.
    pub fn merged(&self) -> Option<Geometry<f64>> {
        let mut polygons: Vec<Polygon<f64>> = Vec::new();
        for feature in &self.features {
            match feature.geometry.as_ref().and_then(convert) {
                Some(Geometry::Polygon(p)) => polygons.push(p),
                Some(Geometry::MultiPolygon(mp)) => polygons.extend(mp.0),
                _ => {}
            }
        }
        match polygons.len() {
            0 => None,
            1 => Some(Geometry::Polygon(
                polygons.into_iter().next().expect("checked length"),
            )),
            _ => Some(Geometry::MultiPolygon(MultiPolygon(polygons))),
        }
    }
}

/// Convert one GeoJSON geometry.
pub fn convert(g: &GeoJsonGeometry) -> Option<Geometry<f64>> {
    match g {
        GeoJsonGeometry::Polygon { coordinates } => {
            let (outer, holes) = coordinates.split_first()?;
            Some(Geometry::Polygon(Polygon::new(
                ring(outer),
                holes.iter().map(|h| ring(h)).collect(),
            )))
        }
        GeoJsonGeometry::MultiPolygon { coordinates } => {
            let polys: Vec<Polygon<f64>> = coordinates
                .iter()
                .filter_map(|rings| {
                    let (outer, holes) = rings.split_first()?;
                    Some(Polygon::new(
                        ring(outer),
                        holes.iter().map(|h| ring(h)).collect(),
                    ))
                })
                .collect();
            (!polys.is_empty()).then_some(Geometry::MultiPolygon(MultiPolygon(polys)))
        }
    }
}

pub fn ring(points: &[[f64; 2]]) -> LineString<f64> {
    LineString(points.iter().map(|p| Coord { x: p[0], y: p[1] }).collect())
}

/// Mean of the outer ring's vertices — a label anchor, not a true centroid.
///
/// Good enough to hang a marker on and far cheaper than an area-weighted
/// centroid. It is deliberately not presented as the feature's location: the
/// polygon is the thing.
pub fn centroid(g: &Geometry<f64>) -> Option<(f64, f64)> {
    let exterior = match g {
        Geometry::Polygon(p) => p.exterior(),
        Geometry::MultiPolygon(mp) => mp.0.first()?.exterior(),
        _ => return None,
    };
    let pts: Vec<&Coord<f64>> = exterior.0.iter().collect();
    if pts.is_empty() {
        return None;
    }
    let n = pts.len() as f64;
    let lon = pts.iter().map(|c| c.x).sum::<f64>() / n;
    let lat = pts.iter().map(|c| c.y).sum::<f64>() / n;
    Some((lon, lat))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_collection_of_several_polygons_becomes_one_multipolygon() {
        // A flood area drawn as three disjoint pieces is still one place, and
        // keeping only the first would shrink the warned area silently.
        let fc: FeatureCollection = serde_json::from_str(
            r#"{"type":"FeatureCollection","features":[
                 {"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}},
                 {"geometry":{"type":"Polygon","coordinates":[[[5,5],[6,5],[6,6],[5,5]]]}}
               ]}"#,
        )
        .expect("a feature collection");
        let Some(Geometry::MultiPolygon(mp)) = fc.merged() else {
            panic!("expected a multipolygon");
        };
        assert_eq!(mp.0.len(), 2);
    }

    #[test]
    fn a_multipolygon_feature_is_flattened_rather_than_nested() {
        let fc: FeatureCollection = serde_json::from_str(
            r#"{"type":"FeatureCollection","features":[
                 {"geometry":{"type":"MultiPolygon","coordinates":[
                    [[[0,0],[1,0],[1,1],[0,0]]],
                    [[[5,5],[6,5],[6,6],[5,5]]]]}}
               ]}"#,
        )
        .expect("a feature collection");
        let Some(Geometry::MultiPolygon(mp)) = fc.merged() else {
            panic!("expected a multipolygon");
        };
        assert_eq!(mp.0.len(), 2, "the parts are lifted, not wrapped again");
    }

    #[test]
    fn a_feature_with_no_geometry_is_skipped_not_fatal() {
        let fc: FeatureCollection = serde_json::from_str(
            r#"{"type":"FeatureCollection","features":[
                 {"geometry":null},
                 {"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}}
               ]}"#,
        )
        .expect("a feature collection");
        assert!(matches!(fc.merged(), Some(Geometry::Polygon(_))));
    }

    #[test]
    fn an_empty_collection_has_no_geometry() {
        let fc: FeatureCollection =
            serde_json::from_str(r#"{"type":"FeatureCollection","features":[]}"#)
                .expect("a feature collection");
        assert!(fc.merged().is_none());
    }

    #[test]
    fn holes_survive_the_conversion() {
        let g: GeoJsonGeometry = serde_json::from_str(
            r#"{"type":"Polygon","coordinates":[
                 [[0,0],[10,0],[10,10],[0,10],[0,0]],
                 [[2,2],[3,2],[3,3],[2,2]]]}"#,
        )
        .expect("a polygon");
        let Some(Geometry::Polygon(p)) = convert(&g) else {
            panic!("expected a polygon");
        };
        assert_eq!(p.interiors().len(), 1, "an island inside a flood area is not flooded");
    }

    #[test]
    fn the_centroid_anchors_inside_the_shape() {
        let g: GeoJsonGeometry = serde_json::from_str(
            r#"{"type":"Polygon","coordinates":[[[0,0],[10,0],[10,10],[0,10],[0,0]]]}"#,
        )
        .expect("a polygon");
        let (lon, lat) = centroid(&convert(&g).expect("converted")).expect("a centroid");
        assert!((0.0..=10.0).contains(&lon) && (0.0..=10.0).contains(&lat));
    }
}
