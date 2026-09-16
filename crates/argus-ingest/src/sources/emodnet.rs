//! Offshore infrastructure in European waters: oil and gas platforms and
//! wind farms, from EMODnet Human Activities.
//!
//! EMODnet is the EU's marine data network; its Human Activities theme
//! collates national registers into one WFS. Two layers are read here:
//! `platforms`, 1,617 points from Norway's 722 to Croatia's 19, with
//! operator, function and status; and `windfarmspoly`, 600 wind farm
//! outlines with capacity, turbine count and a status from `Planned` to
//! `Dismantled`. These are the first [`EntityKind::Feature`] layers: things
//! that do not move and do not expire, so the store never retires them and
//! the observation is the register as read on the day.
//!
//! ## What the whole layers showed
//!
//! - The WFS returns GeoJSON with `srsName=EPSG:4326` in longitude,
//!   latitude order — checked, because the storm overflows taught that
//!   `f=json` from an ArcGIS server does not.
//! - `platformid` is missing on 19 platforms and shared by three pairs, and
//!   21 wind farms have no name at all, so the key is the WFS feature id
//!   (`platforms.21`), which is unique within a release of the dataset. A
//!   republish that renumbers would leave the old ids in the store, since
//!   features never expire. That is the honest limit of a registry with no
//!   stable identifiers.
//! - 318 platforms have no category and 374 give `valid_from` as a bare
//!   year. Both are carried as given.
//! - Polled daily. A register of steel in the sea does not change by the
//!   hour, and asking for four megabytes more often than that is rude.

use crate::geojson::{self, GeoJsonGeometry};
use crate::http::HttpClient;
use argus_core::BoundingBox;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

const WFS_URL: &str = "https://ows.emodnet-humanactivities.eu/wfs?service=WFS&version=2.0.0&request=GetFeature&outputFormat=application/json&srsName=EPSG:4326&typeNames=emodnet:";

const CADENCE_SECS: u64 = 24 * 60 * 60;

/// Which of the two layers a source reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asset {
    Platforms,
    WindFarms,
}

impl Asset {
    fn type_name(self) -> &'static str {
        match self {
            Self::Platforms => "platforms",
            Self::WindFarms => "windfarmspoly",
        }
    }
}

pub struct Emodnet {
    descriptor: SourceDescriptor,
    http: HttpClient,
    asset: Asset,
}

impl Emodnet {
    pub fn platforms(http: HttpClient) -> Self {
        Self::new(
            http,
            Asset::Platforms,
            "emodnet-platforms",
            "offshore-platforms",
            "Offshore platforms (EMODnet)",
        )
    }

    pub fn wind_farms(http: HttpClient) -> Self {
        Self::new(
            http,
            Asset::WindFarms,
            "emodnet-wind-farms",
            "wind-farms",
            "Offshore wind farms (EMODnet)",
        )
    }

    fn new(http: HttpClient, asset: Asset, id: &str, layer: &str, name: &str) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new(id),
                layer_id: LayerId::new(layer),
                display_name: name.into(),
                kind: EntityKind::Feature,
                cadence: Cadence::every(CADENCE_SECS),
                // European seas, from the Barents to the Black Sea — and the
                // Canaries, which are Spain and have wind farms at 29°N.
                coverage: Coverage::Fixed {
                    bbox: BoundingBox::new(-32.0, 26.0, 45.0, 82.0),
                },
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "EMODnet Human Activities".into(),
                    url: "https://emodnet.ec.europa.eu/en/human-activities".into(),
                    license: "CC BY 4.0".into(),
                    notice: Some(
                        "Data from EMODnet Human Activities, funded by the European Union".into(),
                    ),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
            asset,
        }
    }
}

#[async_trait::async_trait]
impl Source for Emodnet {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let url = format!("{WFS_URL}{}", self.asset.type_name());
        let collection: Collection = self.http.get_json(&url).await?;
        Ok(decode(
            collection,
            self.asset,
            &self.descriptor.id,
            Utc::now(),
        ))
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Collection {
    #[serde(default)]
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    id: Option<String>,
    geometry: Option<serde_json::Value>,
    #[serde(default)]
    properties: serde_json::Map<String, serde_json::Value>,
}

/// A property as text, if it is there and not blank.
fn text(props: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    match props.get(key)? {
        serde_json::Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn number(props: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<f64> {
    props.get(key)?.as_f64()
}

/// A GeoJSON Point's coordinates.
fn point(geometry: &serde_json::Value) -> Option<(f64, f64)> {
    if geometry.get("type")?.as_str()? != "Point" {
        return None;
    }
    let c = geometry.get("coordinates")?.as_array()?;
    Some((c.first()?.as_f64()?, c.get(1)?.as_f64()?))
}

fn decode(
    collection: Collection,
    asset: Asset,
    source_id: &SourceId,
    now: DateTime<Utc>,
) -> Vec<Observation> {
    let mut out = Vec::with_capacity(collection.features.len());
    for f in collection.features {
        let Some(key) = f.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(geometry) = f.geometry.as_ref() else {
            continue;
        };
        let props = &f.properties;

        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("country", serde_json::json!(text(props, "country")));

        let (label, position, geom) = match asset {
            Asset::Platforms => {
                let Some((lon, lat)) = point(geometry) else {
                    continue;
                };
                if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
                    continue;
                }
                put("platform_id", serde_json::json!(text(props, "platformid")));
                put("status", serde_json::json!(text(props, "current_status")));
                put("category", serde_json::json!(text(props, "category")));
                put("function", serde_json::json!(text(props, "function")));
                put("operator", serde_json::json!(text(props, "operator")));
                put(
                    "production",
                    serde_json::json!(text(props, "primary_production")),
                );
                put("blocks", serde_json::json!(text(props, "location_blocks")));
                put("valid_from", serde_json::json!(text(props, "valid_from")));
                put("valid_to", serde_json::json!(text(props, "valid_to")));
                put(
                    "water_depth_m",
                    serde_json::json!(number(props, "water_depth")),
                );
                put(
                    "coast_distance_m",
                    serde_json::json!(number(props, "coast_dist")),
                );
                put("remarks", serde_json::json!(text(props, "remarks")));
                let label = text(props, "name").unwrap_or_else(|| key.to_string());
                (label, (lon, lat), None)
            }
            Asset::WindFarms => {
                let Ok(shape) = serde_json::from_value::<GeoJsonGeometry>(geometry.clone()) else {
                    continue;
                };
                let Some(area) = geojson::convert(&shape) else {
                    continue;
                };
                let Some(centre) = geojson::centroid(&area) else {
                    continue;
                };
                put("status", serde_json::json!(text(props, "status")));
                put("power_mw", serde_json::json!(number(props, "power_mw")));
                put(
                    "turbines",
                    serde_json::json!(number(props, "n_turbines").map(|n| n as i64)),
                );
                put("foundation", serde_json::json!(text(props, "type_inst")));
                put("year", serde_json::json!(text(props, "year")));
                put("updated_year", serde_json::json!(text(props, "updateyear")));
                put("area_km2", serde_json::json!(number(props, "area_sqkm")));
                put(
                    "coast_distance_m",
                    serde_json::json!(number(props, "dist_coast")),
                );
                put("notes", serde_json::json!(text(props, "notes")));
                let label = match (text(props, "name"), text(props, "status")) {
                    (Some(n), Some(s)) if s != "Production" => format!("{n} ({s})"),
                    (Some(n), _) => n,
                    (None, _) => format!("Wind farm {key}"),
                };
                (label, centre, Some(area))
            }
        };

        let mut obs = Observation::new(
            source_id.clone(),
            EntityId::new(EntityKind::Feature, key),
            now,
            Quality::Live,
        )
        .with_position(Position {
            lon: position.0,
            lat: position.1,
            alt_m: None,
            datum: AltitudeDatum::Geoid,
        })
        .with_label(label)
        .with_attrs(serde_json::Value::Object(attrs));
        if let Some(g) = geom {
            obs = obs.with_geom(g);
        }
        out.push(obs);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-09-16T12:00:00Z".parse().unwrap()
    }

    const PLATFORMS: &str = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","id":"platforms.21","geometry":{"type":"Point","coordinates":[4.74882821,55.71545852]},"properties":{"country":"Denmark","platformid":"DK35","current_status":"Operational","name":"Tyra WC","category":"Fixed steel","function":"Above water production","operator":"Mærsk","location_blocks":"5504/11","primary_production":"Natural Gas","weight_sub":1499.0,"valid_from":"19840101","valid_to":null,"water_depth":41.0,"coast_dist":210002.399704,"remarks":"Wellhead"}},
      {"type":"Feature","id":"platforms.22","geometry":{"type":"Point","coordinates":[1.5,57.0]},"properties":{"country":"United Kingdom","platformid":null,"current_status":"Decommissioned","name":"","category":null,"valid_from":"1975","water_depth":90.0,"coast_dist":150000.0}},
      {"type":"Feature","id":"platforms.23","geometry":null,"properties":{"country":"Norway","name":"Ghost"}}
    ]}"#;

    const FARMS: &str = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","id":"windfarmspoly.103","geometry":{"type":"MultiPolygon","coordinates":[[[[-2.66,47.21],[-2.635,47.178],[-2.58,47.163],[-2.57,47.183],[-2.66,47.21]]]]},"properties":{"country":"France","name":"Saint-Nazaire","n_turbines":80,"power_mw":480.0,"status":"Production","type_inst":"Grounded","updateyear":"2026","year":"2022","dist_coast":11306.4766933,"area_sqkm":78.1086361513,"notes":null}},
      {"type":"Feature","id":"windfarmspoly.44","geometry":{"type":"MultiPolygon","coordinates":[[[[2.0,51.5],[2.1,51.5],[2.1,51.6],[2.0,51.6],[2.0,51.5]]]]},"properties":{"country":"Belgium","name":null,"status":"Planned","power_mw":700.0}}
    ]}"#;

    #[test]
    fn platforms_are_features_keyed_by_the_wfs_id_and_unplaced_ones_are_skipped() {
        let c: Collection = serde_json::from_str(PLATFORMS).unwrap();
        let obs = decode(
            c,
            Asset::Platforms,
            &SourceId::new("emodnet-platforms"),
            now(),
        );
        assert_eq!(obs.len(), 2, "the platform with no geometry is not a place");
        let tyra = &obs[0];
        assert_eq!(tyra.entity.kind, EntityKind::Feature);
        assert_eq!(tyra.entity.key, "platforms.21");
        assert_eq!(tyra.label.as_deref(), Some("Tyra WC"));
        assert_eq!(tyra.observed_at, now());
        let p = tyra.position.unwrap();
        assert_eq!(
            (p.lon, p.lat),
            (4.74882821, 55.71545852),
            "longitude first, as the WFS sends it"
        );
        assert_eq!(tyra.attrs["operator"], serde_json::json!("Mærsk"));
        assert_eq!(tyra.attrs["water_depth_m"], serde_json::json!(41.0));
        assert_eq!(tyra.attrs["status"], serde_json::json!("Operational"));
        // Blank name, null category, bare-year valid_from: carried as given
        // or absent, never as empty strings.
        let anon = &obs[1];
        assert_eq!(anon.label.as_deref(), Some("platforms.22"));
        assert!(anon.attrs.get("category").is_none());
        assert!(anon.attrs.get("platform_id").is_none());
        assert_eq!(anon.attrs["valid_from"], serde_json::json!("1975"));
    }

    #[test]
    fn wind_farms_keep_their_outline_and_say_when_they_are_not_yet_built() {
        let c: Collection = serde_json::from_str(FARMS).unwrap();
        let obs = decode(
            c,
            Asset::WindFarms,
            &SourceId::new("emodnet-wind-farms"),
            now(),
        );
        assert_eq!(obs.len(), 2);
        let sn = &obs[0];
        assert_eq!(sn.label.as_deref(), Some("Saint-Nazaire"));
        assert!(matches!(
            sn.geom,
            Some(geo_types::Geometry::MultiPolygon(_))
        ));
        let p = sn.position.unwrap();
        assert!((-2.7..=-2.5).contains(&p.lon) && (47.1..=47.3).contains(&p.lat));
        assert_eq!(sn.attrs["power_mw"], serde_json::json!(480.0));
        assert_eq!(sn.attrs["turbines"], serde_json::json!(80));
        assert_eq!(sn.attrs["foundation"], serde_json::json!("Grounded"));
        let planned = &obs[1];
        assert_eq!(planned.label.as_deref(), Some("Wind farm windfarmspoly.44"));
        assert_eq!(planned.attrs["status"], serde_json::json!("Planned"));
    }
}
