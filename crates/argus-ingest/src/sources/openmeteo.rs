//! Open-Meteo: gridded model output, sampled where the areas of interest
//! are — air quality and pollen from CAMS, and sea state from the wave
//! models.
//!
//! Open-Meteo serves model grids through a point API: ask for a
//! latitude and longitude and it answers for the model cell that contains
//! them, telling you the cell's own centre. It takes a list of points in
//! one request — 156 in a quarter of a second when this was written — and
//! charges per point, so a gridded layer here is a lattice of points laid
//! over each area of interest, refreshed together, each one a `Measure`
//! at the cell the model answered for. Nothing is interpolated: what is
//! drawn is the model's value at the model's cell, with the lattice
//! spacing carried so a reader knows how sparsely the area was sampled.
//!
//! ## What the readings are
//!
//! They are model output, not measurements — CAMS Europe for air quality
//! and pollen (about 10 km), and the wave models behind the marine API
//! (about 9 km), each a forecast run a few hours old evaluated at the
//! current hour. [`Quality::Modeled`] says so, and the card repeats it.
//! Pollen is only produced for Europe; elsewhere those fields are absent
//! and are not written. The marine model has nothing to say over land and
//! answers `null` for every field, and those points are dropped rather
//! than drawn as a sea with no waves.
//!
//! ## Budget
//!
//! The free tier allows 10,000 calls a day, with a call counted per point
//! and scaled up past ten variables. The lattice is sized so that no area
//! costs more than [`MAX_POINTS`] points a poll, the air quality layer
//! polls hourly (the model's own step) and the sea state every three
//! hours (the wave models update no faster). With two areas configured
//! that is about 6,700 calls a day. The `quota` on each descriptor says
//! the same thing in numbers.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Quota, Source,
    SourceDescriptor, SourceError, SourceId,
};
use argus_core::BoundingBox;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The most points one area is sampled at. Spacing is chosen from
/// [`SPACINGS`] as the finest that keeps an area under this.
pub const MAX_POINTS: usize = 80;

/// Candidate lattice spacings, degrees, finest first. The air quality
/// model is 0.1° and the wave models about 0.083°, so nothing finer than
/// 0.1° would sample a new cell.
const SPACINGS: [f64; 8] = [0.1, 0.2, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0];

/// One variable to ask for, and how to store and label it.
#[derive(Debug, Clone, Copy)]
struct Variable {
    /// The API's name.
    api: &'static str,
    /// The attribute key, with its unit suffix in the project's convention.
    key: &'static str,
}

const AIR_VARIABLES: &[Variable] = &[
    Variable { api: "european_aqi", key: "european_aqi" },
    Variable { api: "pm10", key: "pm10_ugm3" },
    Variable { api: "pm2_5", key: "pm2_5_ugm3" },
    Variable { api: "nitrogen_dioxide", key: "no2_ugm3" },
    Variable { api: "ozone", key: "ozone_ugm3" },
    Variable { api: "sulphur_dioxide", key: "so2_ugm3" },
    Variable { api: "carbon_monoxide", key: "co_ugm3" },
    Variable { api: "ammonia", key: "nh3_ugm3" },
    Variable { api: "dust", key: "dust_ugm3" },
    Variable { api: "uv_index", key: "uv_index" },
    Variable { api: "grass_pollen", key: "grass_pollen_grains_m3" },
    Variable { api: "birch_pollen", key: "birch_pollen_grains_m3" },
    Variable { api: "alder_pollen", key: "alder_pollen_grains_m3" },
    Variable { api: "mugwort_pollen", key: "mugwort_pollen_grains_m3" },
    Variable { api: "olive_pollen", key: "olive_pollen_grains_m3" },
    Variable { api: "ragweed_pollen", key: "ragweed_pollen_grains_m3" },
];

const SEA_VARIABLES: &[Variable] = &[
    Variable { api: "wave_height", key: "wave_height_m" },
    Variable { api: "wave_direction", key: "wave_dir_deg" },
    Variable { api: "wave_period", key: "wave_period_s" },
    Variable { api: "wind_wave_height", key: "wind_wave_height_m" },
    Variable { api: "swell_wave_height", key: "swell_wave_height_m" },
    Variable { api: "sea_surface_temperature", key: "sea_temp_c" },
    Variable { api: "ocean_current_velocity", key: "current_speed_kmh" },
];

/// Which of Open-Meteo's grids a source samples.
#[derive(Debug, Clone, Copy)]
enum Model {
    AirQuality,
    Marine,
}

impl Model {
    fn endpoint(self) -> &'static str {
        match self {
            Self::AirQuality => "https://air-quality-api.open-meteo.com/v1/air-quality",
            Self::Marine => "https://marine-api.open-meteo.com/v1/marine",
        }
    }

    fn variables(self) -> &'static [Variable] {
        match self {
            Self::AirQuality => AIR_VARIABLES,
            Self::Marine => SEA_VARIABLES,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::AirQuality => "air",
            Self::Marine => "sea",
        }
    }
}

pub struct OpenMeteo {
    descriptor: SourceDescriptor,
    http: HttpClient,
    model: Model,
}

impl OpenMeteo {
    /// Air quality and pollen: the European AQI, particulates, gases, UV
    /// and six pollens, hourly.
    pub fn air_quality(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("open-meteo-air"),
                layer_id: LayerId::new("air-quality"),
                display_name: "Air quality and pollen (Open-Meteo, CAMS)".into(),
                kind: EntityKind::Measure,
                cadence: Cadence::every(3600),
                coverage: Coverage::Bounded,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: attribution(),
                base_quality: Quality::Modeled,
                quota: Some(Quota {
                    limit: 10_000,
                    window: std::time::Duration::from_secs(86_400),
                    // Sixteen variables is 1.6 calls a point; two areas of
                    // eighty points.
                    cost_per_poll: 260,
                }),
            },
            http,
            model: Model::AirQuality,
        }
    }

    /// Sea state: waves, swell, surface temperature and current, every
    /// three hours.
    pub fn sea_state(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("open-meteo-marine"),
                layer_id: LayerId::new("sea-state"),
                display_name: "Sea state (Open-Meteo marine)".into(),
                kind: EntityKind::Measure,
                cadence: Cadence::every(3 * 3600),
                coverage: Coverage::Bounded,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: attribution(),
                base_quality: Quality::Modeled,
                quota: Some(Quota {
                    limit: 10_000,
                    window: std::time::Duration::from_secs(86_400),
                    cost_per_poll: 160,
                }),
            },
            http,
            model: Model::Marine,
        }
    }

    fn url(&self, points: &[(f64, f64)]) -> String {
        let lats: Vec<String> = points.iter().map(|(lat, _)| format!("{lat:.3}")).collect();
        let lons: Vec<String> = points.iter().map(|(_, lon)| format!("{lon:.3}")).collect();
        let vars: Vec<&str> = self.model.variables().iter().map(|v| v.api).collect();
        format!(
            "{}?latitude={}&longitude={}&current={}&timezone=UTC",
            self.model.endpoint(),
            lats.join(","),
            lons.join(","),
            vars.join(",")
        )
    }
}

fn attribution() -> Attribution {
    Attribution {
        provider: "Open-Meteo".into(),
        url: "https://open-meteo.com/".into(),
        license: "CC BY 4.0, non-commercial".into(),
        notice: Some("Weather data by Open-Meteo.com; air quality from CAMS (Copernicus)".into()),
    }
}

#[async_trait::async_trait]
impl Source for OpenMeteo {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let bbox = ctx.bbox.unwrap_or(BoundingBox::GLOBAL);
        let (spacing, points) = lattice(&bbox);
        if points.is_empty() {
            return Ok(Vec::new());
        }
        let url = self.url(&points);
        let bytes = self.http.get_bytes(&url).await?;
        // One point is answered as an object, several as an array.
        let cells: Vec<Cell> = match serde_json::from_slice::<Vec<Cell>>(&bytes) {
            Ok(cells) => cells,
            Err(e) => match serde_json::from_slice::<Cell>(&bytes) {
                Ok(cell) => vec![cell],
                Err(_) => return Err(SourceError::Decode(e.to_string())),
            },
        };
        let now = Utc::now();
        let decoded = decode(&cells, self.model, spacing, &self.descriptor.id, now);
        tracing::info!(
            source = %self.descriptor.id,
            asked = points.len(),
            answered = cells.len(),
            written = decoded.observations.len(),
            empty = decoded.empty,
            spacing,
            "open-meteo lattice read"
        );
        Ok(decoded.observations)
    }
}

/// The sample points for an area: cell centres on the finest spacing that
/// keeps the count under [`MAX_POINTS`], and their spacing.
pub fn lattice(bbox: &BoundingBox) -> (f64, Vec<(f64, f64)>) {
    let width = (bbox.east - bbox.west).max(0.0);
    let height = (bbox.north - bbox.south).max(0.0);
    let spacing = SPACINGS
        .iter()
        .copied()
        .find(|s| {
            let cols = (width / s).ceil().max(1.0) as usize;
            let rows = (height / s).ceil().max(1.0) as usize;
            cols * rows <= MAX_POINTS
        })
        .unwrap_or(SPACINGS[SPACINGS.len() - 1]);
    let cols = (width / spacing).ceil().max(1.0) as usize;
    let rows = (height / spacing).ceil().max(1.0) as usize;
    let mut points = Vec::with_capacity(cols * rows);
    for r in 0..rows {
        let lat = bbox.south + spacing * (r as f64 + 0.5);
        if lat > 90.0 {
            break;
        }
        for c in 0..cols {
            let lon = bbox.west + spacing * (c as f64 + 0.5);
            if lon > bbox.east || lat > bbox.north {
                continue;
            }
            points.push((lat, lon));
        }
    }
    // An area too big for the coarsest spacing is cut at the limit rather
    // than sampled past the budget.
    points.truncate(MAX_POINTS);
    (spacing, points)
}

// --- wire format -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Cell {
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    elevation: Option<f64>,
    #[serde(default)]
    current_units: BTreeMap<String, String>,
    #[serde(default)]
    current: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug)]
pub struct Decoded {
    pub observations: Vec<Observation>,
    /// Cells the model had nothing for — sea points asked of the air model
    /// still answer; land points asked of the wave model do not.
    pub empty: usize,
}

fn decode(cells: &[Cell], model: Model, spacing: f64, source_id: &SourceId, now: DateTime<Utc>) -> Decoded {
    let mut observations = Vec::with_capacity(cells.len());
    let mut empty = 0;
    let mut seen = std::collections::HashSet::new();
    for cell in cells {
        let (lat, lon) = (cell.latitude, cell.longitude);
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            continue;
        }
        // The wave model answers a point on the coast with the nearest sea
        // cell, up to a fifth of a degree away, so two lattice points can
        // come back as the same cell. One reading per cell.
        let key = format!("{}:{lat:.3}:{lon:.3}", model.prefix());
        if !seen.insert(key.clone()) {
            continue;
        }
        // Dated by the model hour the reading is for, never ahead of the
        // clock, so a poll that sees the same hour again writes nothing.
        let at = cell
            .current
            .get("time")
            .and_then(|v| v.as_str())
            .and_then(parse_hour)
            .unwrap_or(now)
            .min(now);

        let mut attrs = serde_json::Map::new();
        let mut any = false;
        for v in model.variables() {
            if let Some(n) = cell.current.get(v.api).and_then(|x| x.as_f64()) {
                attrs.insert(v.key.to_string(), serde_json::json!(n));
                any = true;
            }
        }
        if !any {
            empty += 1;
            continue;
        }
        attrs.insert("model_time".into(), serde_json::json!(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
        attrs.insert("lattice_spacing_deg".into(), serde_json::json!(spacing));
        if let Some(e) = cell.elevation
            && matches!(model, Model::AirQuality)
        {
            attrs.insert("model_elevation_m".into(), serde_json::json!(e));
        }
        if let Some(u) = cell.current_units.get("european_aqi")
            && u != "EAQI"
        {
            attrs.insert("aqi_scale".into(), serde_json::json!(u));
        }

        let label = match model {
            Model::AirQuality => match attrs.get("european_aqi").and_then(|v| v.as_f64()) {
                Some(aqi) => format!("AQI {aqi:.0} ({})", aqi_band(aqi)),
                None => "Air quality".to_string(),
            },
            Model::Marine => match attrs.get("wave_height_m").and_then(|v| v.as_f64()) {
                Some(h) => format!("Waves {h:.1} m"),
                None => "Sea state".to_string(),
            },
        };

        observations.push(
            Observation::new(
                source_id.clone(),
                EntityId::new(EntityKind::Measure, key),
                at,
                Quality::Modeled,
            )
            .with_position(Position {
                lon,
                lat,
                alt_m: None,
                datum: AltitudeDatum::AboveGround,
            })
            .with_label(label)
            .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    Decoded { observations, empty }
}

/// The European Air Quality Index bands, as CAMS defines them.
pub fn aqi_band(aqi: f64) -> &'static str {
    match aqi as i64 {
        i64::MIN..=20 => "good",
        21..=40 => "fair",
        41..=60 => "moderate",
        61..=80 => "poor",
        81..=100 => "very poor",
        _ => "extremely poor",
    }
}

/// `2026-09-16T15:00`: no seconds, no zone, and the request asked for UTC.
fn parse_hour(s: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
        .ok()
        .map(|t| t.and_utc())
        .or_else(|| s.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lattice_is_the_finest_spacing_under_the_point_budget() {
        // The home area: 3° by 1.5°. 0.2° would be 15 × 8 = 120 points,
        // 0.25° is 12 × 6 = 72.
        let (spacing, points) = lattice(&BoundingBox::new(-2.5, 51.0, 0.5, 52.5));
        assert_eq!(spacing, 0.25);
        assert_eq!(points.len(), 72);
        assert!((points[0].0 - 51.125).abs() < 1e-9 && (points[0].1 + 2.375).abs() < 1e-9, "cell centres, not corners");
        // The British Isles: 13° by 11.5°. 1.5° gives 9 × 8 = 72.
        let (spacing, points) = lattice(&BoundingBox::new(-11.0, 49.5, 2.0, 61.0));
        assert_eq!(spacing, 1.5);
        assert_eq!(points.len(), 72);
        // The whole world is cut at the budget rather than asked for.
        let (_, points) = lattice(&BoundingBox::GLOBAL);
        assert_eq!(points.len(), MAX_POINTS);
        // A tiny box is one point.
        let (spacing, points) = lattice(&BoundingBox::new(-0.55, 51.42, -0.35, 51.52));
        assert_eq!(spacing, 0.1);
        assert_eq!(points.len(), 2);
    }

    const AIR: &str = r#"[{"latitude":51.5,"longitude":-0.10000038,"elevation":12.0,"utc_offset_seconds":0,"timezone":"GMT","current_units":{"time":"iso8601","interval":"seconds","european_aqi":"EAQI","pm10":"μg/m³","pm2_5":"μg/m³","grass_pollen":"grains/m³"},"current":{"time":"2026-09-16T15:00","interval":3600,"european_aqi":19,"pm10":7.1,"pm2_5":3.2,"nitrogen_dioxide":7.1,"ozone":58.0,"sulphur_dioxide":0.7,"carbon_monoxide":199.0,"uv_index":1.15,"alder_pollen":0.0,"birch_pollen":0.0,"grass_pollen":0.1,"mugwort_pollen":0.0,"olive_pollen":0.0,"ragweed_pollen":0.0,"dust":0.0,"ammonia":2.3}},
    {"latitude":40.7,"longitude":-74.0,"elevation":10.0,"location_id":1,"current_units":{"time":"iso8601"},"current":{"time":"2026-09-16T15:00","interval":3600,"european_aqi":33,"pm10":12.0,"pm2_5":8.0,"nitrogen_dioxide":9.0,"ozone":60.0,"sulphur_dioxide":1.0,"carbon_monoxide":210.0,"uv_index":3.0,"alder_pollen":null,"birch_pollen":null,"grass_pollen":null,"mugwort_pollen":null,"olive_pollen":null,"ragweed_pollen":null,"dust":0.0,"ammonia":1.0}}]"#;

    #[test]
    fn an_air_cell_is_a_modelled_measure_dated_by_its_hour() {
        let cells: Vec<Cell> = serde_json::from_str(AIR).unwrap();
        let now: DateTime<Utc> = "2026-09-16T15:20:00Z".parse().unwrap();
        let d = decode(&cells, Model::AirQuality, 0.25, &SourceId::new("open-meteo-air"), now);
        assert_eq!(d.observations.len(), 2);
        let london = &d.observations[0];
        assert_eq!(london.entity.key, "air:51.500:-0.100");
        assert_eq!(london.quality, Quality::Modeled);
        assert_eq!(london.observed_at, "2026-09-16T15:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(london.attrs["pm2_5_ugm3"], 3.2);
        assert_eq!(london.attrs["grass_pollen_grains_m3"], 0.1);
        assert_eq!(london.attrs["lattice_spacing_deg"], 0.25);
        assert_eq!(london.label.as_deref(), Some("AQI 19 (good)"));
        // Outside Europe there is no pollen, and no pollen key.
        let nyc = &d.observations[1];
        assert!(nyc.attrs.get("grass_pollen_grains_m3").is_none());
        assert_eq!(nyc.label.as_deref(), Some("AQI 33 (fair)"));
    }

    const SEA: &str = r#"[{"latitude":52.041664,"longitude":-1.0416565,"elevation":82.0,"current_units":{"time":"iso8601"},"current":{"time":"2026-09-16T15:45","interval":900,"wave_height":null,"wave_direction":null,"wave_period":null,"wind_wave_height":null,"swell_wave_height":null,"sea_surface_temperature":null,"ocean_current_velocity":null}},
    {"latitude":51.541664,"longitude":1.4583435,"elevation":0.0,"location_id":1,"current_units":{"time":"iso8601"},"current":{"time":"2026-09-16T15:45","interval":900,"wave_height":0.52,"wave_direction":293,"wave_period":3.05,"wind_wave_height":0.4,"swell_wave_height":0.3,"sea_surface_temperature":19.5,"ocean_current_velocity":1.2}},
    {"latitude":51.541664,"longitude":1.4583435,"elevation":0.0,"location_id":2,"current_units":{"time":"iso8601"},"current":{"time":"2026-09-16T15:45","interval":900,"wave_height":0.52,"wave_direction":293,"wave_period":3.05,"wind_wave_height":0.4,"swell_wave_height":0.3,"sea_surface_temperature":19.5,"ocean_current_velocity":1.2}}]"#;

    #[test]
    fn the_wave_model_has_nothing_to_say_over_land_and_answers_the_coast_with_one_sea_cell() {
        let cells: Vec<Cell> = serde_json::from_str(SEA).unwrap();
        let d = decode(&cells, Model::Marine, 0.25, &SourceId::new("open-meteo-marine"), Utc::now());
        assert_eq!(d.empty, 1, "inland is dropped");
        assert_eq!(d.observations.len(), 1, "two coastal points snapped to one cell are one reading");
        let sea = &d.observations[0];
        assert_eq!(sea.entity.key, "sea:51.542:1.458");
        assert_eq!(sea.attrs["wave_height_m"], 0.52);
        assert_eq!(sea.attrs["sea_temp_c"], 19.5);
        assert_eq!(sea.label.as_deref(), Some("Waves 0.5 m"));
    }

    #[test]
    fn the_request_names_every_point_and_variable() {
        let http = HttpClient::new(std::time::Duration::from_secs(5)).unwrap();
        let s = OpenMeteo::air_quality(http);
        let url = s.url(&[(51.125, -2.375), (51.125, -2.125)]);
        assert!(url.starts_with("https://air-quality-api.open-meteo.com/v1/air-quality?latitude=51.125,51.125&longitude=-2.375,-2.125&current=european_aqi,"));
        assert!(url.ends_with("&timezone=UTC"));
    }
}
