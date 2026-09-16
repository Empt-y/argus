//! Carbon intensity of the electricity in each of Great Britain's fourteen
//! distribution regions, from National Grid ESO's Carbon Intensity API,
//! drawn on the regions' actual boundaries.
//!
//! The API gives, every half hour, a forecast intensity in gCO₂ per kWh and
//! the generation mix — nine fuels as percentages — for each of the fourteen
//! Distribution Network Operator licence areas, plus England, Scotland,
//! Wales and GB as aggregates. It gives no geometry. The boundaries come
//! from NESO's data portal as a 3 MB GeoJSON in British National Grid,
//! fetched once per deployment into the reference-geometry cache and
//! transformed to WGS-84 on the way in — read as degrees, a grid easting of
//! 400,000 puts Birmingham somewhere past Neptune's orbit.
//!
//! The join is by hand. The API's region ids and the portal's licence-area
//! ids are two different numberings of the same fourteen areas, matched
//! here by name and pinned in a table, because "South Scotland" in one is
//! "South and Central Scotland" in the other and "South England" is
//! "Southern England". Fourteen entries, checked once, never guessed.
//!
//! A `Measure`: a reading for an area, not a thing at a place. The
//! observation is dated by the half-hour period it describes, so a poll
//! that sees the same period again writes nothing, and the four aggregates
//! are carried as attributes of nothing — they have no area of their own
//! and a GB figure drawn over the whole island would hide the regions.

use crate::geojson;
use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::geo::bng_to_wgs84;
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use argus_core::{BoundingBox, GeometryCache};
use chrono::{DateTime, Utc};
use geo_types::{Coord, Geometry, LineString, MultiPolygon, Polygon};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

const INTENSITY_URL: &str = "https://api.carbonintensity.org.uk/regional";
const AREAS_URL: &str = "https://api.neso.energy/dataset/0e377f16-95e9-4c15-a1fc-49e06a39cfa0/resource/1c6a7dc0-1b6c-443a-bc67-5f7125649434/download";

/// A new half-hour period every thirty minutes; five minutes sees it within
/// a sixth of its life, and the repeats between cost nothing in the store.
const CADENCE_SECS: u64 = 300;

/// The API's region id, the licence area's `ID` in NESO's file, and the
/// name the API uses. The area ids are the 2024-05-03 boundary set.
const REGIONS: [(i64, i64, &str); 14] = [
    (1, 17, "North Scotland"),
    (2, 18, "South Scotland"),
    (3, 16, "North West England"),
    (4, 15, "North East England"),
    (5, 23, "Yorkshire"),
    (6, 13, "North Wales & Merseyside"),
    (7, 21, "South Wales"),
    (8, 14, "West Midlands"),
    (9, 11, "East Midlands"),
    (10, 10, "East England"),
    (11, 22, "South West England"),
    (12, 20, "South England"),
    (13, 12, "London"),
    (14, 19, "South East England"),
];

pub struct CarbonIntensity {
    descriptor: SourceDescriptor,
    http: HttpClient,
    areas: Arc<dyn GeometryCache>,
}

impl CarbonIntensity {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("carbon-intensity"),
                layer_id: LayerId::new("carbon-intensity"),
                display_name: "Grid carbon intensity by region (NESO)".into(),
                kind: EntityKind::Measure,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Fixed {
                    bbox: BoundingBox::new(-8.7, 49.8, 1.8, 60.9),
                },
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "National Energy System Operator, Carbon Intensity API".into(),
                    url: "https://carbonintensity.org.uk/".into(),
                    license: "CC BY 4.0".into(),
                    notice: Some("Carbon intensity data from NESO; DNO licence areas from the NESO data portal".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            areas: Arc::new(argus_core::MemoryGeometryCache::new()),
        }
    }

    /// Back the boundary cache with the store, so the 3 MB file is fetched
    /// once in the life of the deployment rather than once per restart.
    #[must_use]
    pub fn with_area_cache(mut self, cache: Arc<dyn GeometryCache>) -> Self {
        self.areas = cache;
        self
    }

    /// Every region's boundary, from the cache or, on the first miss, from
    /// the portal. One fetch serves all fourteen.
    async fn boundaries(&self) -> HashMap<i64, Geometry<f64>> {
        let mut out = HashMap::new();
        let mut missing = Vec::new();
        for (region, area, _) in REGIONS {
            match self.areas.get(&cache_key(area)).await {
                Some(g) => {
                    out.insert(region, g);
                }
                None => missing.push((region, area)),
            }
        }
        if missing.is_empty() {
            return out;
        }
        let bytes = match self.http.get_bytes(AREAS_URL).await {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(source = %self.descriptor.id, %err, "could not fetch the DNO licence areas; regions without a boundary are drawn as points");
                return out;
            }
        };
        match decode_areas(&String::from_utf8_lossy(&bytes)) {
            Ok(areas) => {
                for (region, area) in missing {
                    if let Some(g) = areas.get(&area) {
                        self.areas.put(&cache_key(area), g).await;
                        out.insert(region, g.clone());
                    } else {
                        tracing::warn!(source = %self.descriptor.id, region, area, "no licence area in the portal file for this region");
                    }
                }
            }
            Err(err) => {
                tracing::warn!(source = %self.descriptor.id, %err, "the DNO licence area file did not decode")
            }
        }
        out
    }
}

fn cache_key(area: i64) -> String {
    format!("neso-dno-area:{area}")
}

#[async_trait::async_trait]
impl Source for CarbonIntensity {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let feed: Feed = self.http.get_json(INTENSITY_URL).await?;
        let boundaries = self.boundaries().await;
        Ok(decode(feed, &boundaries, &self.descriptor.id, Utc::now()))
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Feed {
    data: Vec<Period>,
}

#[derive(Debug, Deserialize)]
struct Period {
    from: Option<String>,
    to: Option<String>,
    #[serde(default)]
    regions: Vec<Region>,
}

#[derive(Debug, Deserialize)]
struct Region {
    regionid: Option<i64>,
    dnoregion: Option<String>,
    intensity: Option<Intensity>,
    #[serde(default)]
    generationmix: Vec<Mix>,
}

#[derive(Debug, Deserialize)]
struct Intensity {
    forecast: Option<f64>,
    actual: Option<f64>,
    index: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Mix {
    fuel: Option<String>,
    perc: Option<f64>,
}

/// NESO's licence-area file: a FeatureCollection of MultiPolygons in
/// EPSG:27700 with an `ID` property.
#[derive(Debug, Deserialize)]
struct AreaFile {
    features: Vec<AreaFeature>,
}

#[derive(Debug, Deserialize)]
struct AreaFeature {
    properties: AreaProps,
    geometry: Option<geojson::GeoJsonGeometry>,
}

#[derive(Debug, Deserialize)]
struct AreaProps {
    #[serde(rename = "ID")]
    id: Option<i64>,
}

/// Decode the licence areas to WGS-84 geometry by area id.
pub fn decode_areas(text: &str) -> Result<HashMap<i64, Geometry<f64>>, SourceError> {
    let file: AreaFile = serde_json::from_str(text)
        .map_err(|e| SourceError::Decode(format!("licence areas: {e}")))?;
    let mut out = HashMap::new();
    for f in file.features {
        let (Some(id), Some(geometry)) = (f.properties.id, f.geometry) else {
            continue;
        };
        let polygons: Vec<Vec<Vec<[f64; 2]>>> = match geometry {
            geojson::GeoJsonGeometry::Polygon { coordinates } => vec![coordinates],
            geojson::GeoJsonGeometry::MultiPolygon { coordinates } => coordinates,
        };
        let transformed: Vec<Polygon<f64>> = polygons
            .into_iter()
            .filter_map(|rings| {
                let mut rings = rings.into_iter().map(|ring| {
                    LineString(
                        ring.iter()
                            .map(|[e, n]| {
                                let (x, y) = bng_to_wgs84(*e, *n);
                                Coord { x, y }
                            })
                            .collect(),
                    )
                });
                let exterior = rings.next()?;
                Some(Polygon::new(exterior, rings.collect()))
            })
            .collect();
        if !transformed.is_empty() {
            out.insert(id, Geometry::MultiPolygon(MultiPolygon(transformed)));
        }
    }
    Ok(out)
}

fn decode(
    feed: Feed,
    boundaries: &HashMap<i64, Geometry<f64>>,
    source_id: &SourceId,
    now: DateTime<Utc>,
) -> Vec<Observation> {
    let mut out = Vec::new();
    for period in feed.data {
        // Dated by the period it describes, never ahead of the clock.
        let from = period
            .from
            .as_deref()
            .and_then(parse_period)
            .unwrap_or(now)
            .min(now);
        let to = period.to.as_deref().and_then(parse_period);
        for region in period.regions {
            let Some(id) = region.regionid else { continue };
            let Some(&(_, _, name)) = REGIONS.iter().find(|(r, _, _)| *r == id) else {
                // England, Scotland, Wales, GB: aggregates without an area.
                continue;
            };
            let intensity = region.intensity.as_ref();
            let Some(value) = intensity.and_then(|i| i.actual.or(i.forecast)) else {
                continue;
            };

            let mut attrs = serde_json::Map::new();
            let mut put = |k: &str, v: serde_json::Value| {
                if !v.is_null() {
                    attrs.insert(k.to_string(), v);
                }
            };
            put("region_id", serde_json::json!(id));
            put("region", serde_json::json!(name));
            put(
                "dno",
                serde_json::json!(
                    region
                        .dnoregion
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                ),
            );
            put("intensity_gco2_kwh", serde_json::json!(value));
            put(
                "intensity_forecast_gco2_kwh",
                serde_json::json!(intensity.and_then(|i| i.forecast)),
            );
            put(
                "intensity_actual_gco2_kwh",
                serde_json::json!(intensity.and_then(|i| i.actual)),
            );
            put(
                "index",
                serde_json::json!(intensity.and_then(|i| i.index.as_deref())),
            );
            let mut mix = serde_json::Map::new();
            for m in &region.generationmix {
                if let (Some(fuel), Some(pct)) = (m.fuel.as_deref(), m.perc) {
                    mix.insert(fuel.to_string(), serde_json::json!(pct));
                }
            }
            if !mix.is_empty() {
                put("generation_mix_pct", serde_json::Value::Object(mix));
            }
            put(
                "period_from",
                serde_json::json!(from.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            );
            put(
                "period_to",
                serde_json::json!(to.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))),
            );

            let label = match intensity.and_then(|i| i.index.as_deref()) {
                Some(index) => format!("{name}: {value:.0} g/kWh ({index})"),
                None => format!("{name}: {value:.0} g/kWh"),
            };

            let mut obs = Observation::new(
                source_id.clone(),
                EntityId::new(EntityKind::Measure, format!("dno-{id}")),
                from,
                Quality::Live,
            )
            .with_label(label)
            .with_attrs(serde_json::Value::Object(attrs));
            if let Some(area) = boundaries.get(&id) {
                if let Some((lon, lat)) = geojson::centroid(area) {
                    obs = obs.with_position(Position {
                        lon,
                        lat,
                        alt_m: None,
                        datum: AltitudeDatum::AboveGround,
                    });
                }
                obs = obs.with_geom(area.clone());
            } else {
                // No boundary yet: the reading still exists, at the region's
                // rough centre, so the layer is never empty for want of a
                // 3 MB file.
                let (lon, lat) = FALLBACK_CENTRES[(id - 1) as usize];
                obs = obs.with_position(Position {
                    lon,
                    lat,
                    alt_m: None,
                    datum: AltitudeDatum::AboveGround,
                });
            }
            out.push(obs);
        }
    }
    out
}

/// A point inside each region, by API region id, for a poll that has no
/// boundary. Approximate by design.
const FALLBACK_CENTRES: [(f64, f64); 14] = [
    (-4.2, 57.5),
    (-3.9, 55.7),
    (-2.6, 53.7),
    (-1.6, 54.9),
    (-1.2, 53.8),
    (-3.3, 53.2),
    (-3.6, 51.7),
    (-2.0, 52.5),
    (-1.0, 52.9),
    (0.6, 52.4),
    (-3.7, 50.8),
    (-1.3, 51.2),
    (-0.1, 51.5),
    (0.5, 51.2),
];

/// The API writes `2026-09-16T11:00Z`: no seconds, and a bare `Z`.
fn parse_period(s: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%MZ")
        .ok()
        .map(|t| t.and_utc())
        .or_else(|| s.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> SourceId {
        SourceId::new("carbon-intensity")
    }

    const FEED: &str = r#"{"data":[{"from":"2026-09-16T11:00Z","to":"2026-09-16T11:30Z","regions":[
      {"regionid":7,"dnoregion":"WPD South Wales","shortname":"South Wales","intensity":{"forecast":329,"index":"very high"},"generationmix":[{"perc":0,"fuel":"biomass"},{"perc":83.4,"fuel":"gas"},{"fuel":"solar","perc":12.4},{"fuel":"wind","perc":4.2}]},
      {"regionid":13,"dnoregion":"UKPN London","shortname":"London","intensity":{"forecast":109,"index":"moderate"},"generationmix":[{"perc":40,"fuel":"gas"},{"perc":60,"fuel":"imports"}]},
      {"regionid":18,"dnoregion":"GB","shortname":"GB","intensity":{"forecast":112,"index":"moderate"},"generationmix":[]}
    ]}]}"#;

    /// Two licence areas in grid coordinates: a square around London and
    /// one around Cardiff, with a hole in the second.
    const AREAS: &str = r#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:EPSG::27700"}},"features":[
      {"type":"Feature","properties":{"ID":12,"Name":"_C","DNO":"UKPN","Area":"London"},"geometry":{"type":"MultiPolygon","coordinates":[[[[520000,175000],[540000,175000],[540000,190000],[520000,190000],[520000,175000]]]]}},
      {"type":"Feature","properties":{"ID":21,"Name":"_K","DNO":"NGED","Area":"South Wales"},"geometry":{"type":"MultiPolygon","coordinates":[[[[300000,170000],[340000,170000],[340000,200000],[300000,200000],[300000,170000]],[[315000,180000],[325000,180000],[325000,190000],[315000,190000],[315000,180000]]]]}}
    ]}"#;

    fn now() -> DateTime<Utc> {
        "2026-09-16T11:10:00Z".parse().unwrap()
    }

    #[test]
    fn licence_areas_come_out_in_degrees_not_metres() {
        let areas = decode_areas(AREAS).unwrap();
        assert_eq!(areas.len(), 2);
        let Geometry::MultiPolygon(london) = &areas[&12] else {
            panic!("multipolygon")
        };
        let ring = &london.0[0].exterior().0;
        for c in ring {
            assert!(
                (-0.5..=0.2).contains(&c.x) && (51.3..=51.7).contains(&c.y),
                "London corner at {c:?}"
            );
        }
        let Geometry::MultiPolygon(wales) = &areas[&21] else {
            panic!("multipolygon")
        };
        assert_eq!(wales.0[0].interiors().len(), 1, "the hole survives");
    }

    #[test]
    fn regions_are_measures_dated_by_their_period_with_the_mix_and_the_boundary() {
        let feed: Feed = serde_json::from_str(FEED).unwrap();
        let areas = decode_areas(AREAS).unwrap();
        let by_region: HashMap<i64, Geometry<f64>> = REGIONS
            .iter()
            .filter_map(|(r, a, _)| areas.get(a).map(|g| (*r, g.clone())))
            .collect();
        let obs = decode(feed, &by_region, &source(), now());
        assert_eq!(
            obs.len(),
            2,
            "GB is an aggregate with no area and is not emitted"
        );
        let wales = obs.iter().find(|o| o.entity.key == "dno-7").unwrap();
        assert_eq!(wales.entity.kind, EntityKind::Measure);
        assert_eq!(
            wales.observed_at,
            "2026-09-16T11:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(wales.attrs["intensity_gco2_kwh"], serde_json::json!(329.0));
        assert_eq!(
            wales.attrs["generation_mix_pct"]["gas"],
            serde_json::json!(83.4)
        );
        assert_eq!(wales.attrs["index"], serde_json::json!("very high"));
        assert_eq!(
            wales.label.as_deref(),
            Some("South Wales: 329 g/kWh (very high)")
        );
        assert!(matches!(wales.geom, Some(Geometry::MultiPolygon(_))));
        let p = wales.position.unwrap();
        assert!(
            (-3.3..=-3.0).contains(&p.lon) && (51.4..=51.7).contains(&p.lat),
            "centroid {p:?}"
        );
    }

    #[test]
    fn without_a_boundary_the_reading_still_lands_at_a_point_in_the_region() {
        let feed: Feed = serde_json::from_str(FEED).unwrap();
        let obs = decode(feed, &HashMap::new(), &source(), now());
        let london = obs.iter().find(|o| o.entity.key == "dno-13").unwrap();
        assert!(london.geom.is_none());
        let p = london.position.unwrap();
        assert!((p.lon - -0.1).abs() < 0.01 && (p.lat - 51.5).abs() < 0.01);
    }

    #[test]
    fn a_period_ahead_of_the_clock_is_dated_now_not_in_the_future() {
        let feed: Feed =
            serde_json::from_str(&FEED.replace("2026-09-16T11:00Z", "2026-09-16T12:00Z")).unwrap();
        let obs = decode(feed, &HashMap::new(), &source(), now());
        assert_eq!(obs[0].observed_at, now());
    }

    #[test]
    fn the_region_table_covers_the_fourteen_api_regions_once_each() {
        let mut api: Vec<i64> = REGIONS.iter().map(|(r, _, _)| *r).collect();
        api.sort_unstable();
        assert_eq!(api, (1..=14).collect::<Vec<_>>());
        let mut areas: Vec<i64> = REGIONS.iter().map(|(_, a, _)| *a).collect();
        areas.sort_unstable();
        areas.dedup();
        assert_eq!(areas.len(), 14, "every region maps to its own licence area");
    }

    #[test]
    fn the_apis_minute_precision_stamp_parses() {
        assert_eq!(
            parse_period("2026-09-16T11:00Z"),
            Some("2026-09-16T11:00:00Z".parse().unwrap())
        );
        assert_eq!(
            parse_period("2026-09-16T11:00:00Z"),
            Some("2026-09-16T11:00:00Z".parse().unwrap())
        );
    }
}
