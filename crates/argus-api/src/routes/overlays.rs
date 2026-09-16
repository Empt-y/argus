//! `/v1/overlays` — satellite imagery and other raster products a client
//! can drape over the globe, from NASA's Global Imagery Browse Services.
//!
//! GIBS serves the daily Earth-observation products as ordinary XYZ tiles
//! in Web Mercator, keyless and with generous terms, which is the whole of
//! Phase 7's imagery in one endpoint: true-colour from MODIS and VIIRS,
//! the night-time lights, sea surface temperature, aerosols. They are
//! rasters, not entities — nothing is ingested and nothing is stored — so
//! they are a catalogue the clients read and draw themselves, with the
//! date resolved here.
//!
//! The date is the point. A daily product is complete the day after, and
//! today's is a strip of swaths with gaps, so "latest" is yesterday; and
//! the DVR's `at` maps onto a date, so a client rewound to last Tuesday
//! gets last Tuesday's imagery under last Tuesday's contacts. GIBS keeps
//! years of it.

use crate::error::ApiResult;
use crate::params::parse_instant;
use axum::extract::Query;
use axum::Json;
use chrono::{Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

const GIBS: &str = "https://gibs.earthdata.nasa.gov/wmts/epsg3857/best";

/// One product as GIBS names it.
struct Product {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    layer: &'static str,
    /// GIBS's tile matrix set, which also fixes the finest zoom.
    matrix: &'static str,
    max_zoom: u8,
    format: &'static str,
    /// How a client should blend it by default: imagery replaces the
    /// ground, a thematic product sits over it half-transparent.
    opacity: f64,
    /// The first day the product exists, so a rewind past it gets nothing
    /// rather than a 404 per tile.
    since: (i32, u32, u32),
}

const PRODUCTS: &[Product] = &[
    Product {
        id: "modis-terra-true-colour",
        name: "Daylight imagery, MODIS Terra",
        description: "True-colour reflectance from the Terra morning pass, 250 m. Cloud is cloud.",
        layer: "MODIS_Terra_CorrectedReflectance_TrueColor",
        matrix: "GoogleMapsCompatible_Level9",
        max_zoom: 9,
        format: "jpg",
        opacity: 1.0,
        since: (2000, 2, 24),
    },
    Product {
        id: "viirs-true-colour",
        name: "Daylight imagery, VIIRS",
        description: "True-colour reflectance from the Suomi NPP afternoon pass, 375 m.",
        layer: "VIIRS_SNPP_CorrectedReflectance_TrueColor",
        matrix: "GoogleMapsCompatible_Level9",
        max_zoom: 9,
        format: "jpg",
        opacity: 1.0,
        since: (2015, 11, 24),
    },
    Product {
        id: "night-lights",
        name: "Night lights",
        description: "VIIRS day/night band radiance: city lights, gas flares, fishing fleets, aurora, moonlit cloud.",
        layer: "VIIRS_SNPP_DayNightBand_At_Sensor_Radiance",
        matrix: "GoogleMapsCompatible_Level8",
        max_zoom: 8,
        format: "png",
        opacity: 1.0,
        since: (2016, 11, 30),
    },
    Product {
        id: "sea-surface-temperature",
        name: "Sea surface temperature",
        description: "GHRSST MUR analysis, 1 km, blended from every sensor that saw the sea that day.",
        layer: "GHRSST_L4_MUR_Sea_Surface_Temperature",
        matrix: "GoogleMapsCompatible_Level7",
        max_zoom: 7,
        format: "png",
        opacity: 0.7,
        since: (2002, 6, 1),
    },
    Product {
        id: "aerosol",
        name: "Aerosol optical depth",
        description: "MODIS Terra aerosol, 3 km: smoke, dust and haze as the column's opacity.",
        layer: "MODIS_Terra_Aerosol_Optical_Depth_3km",
        matrix: "GoogleMapsCompatible_Level6",
        max_zoom: 6,
        format: "png",
        opacity: 0.7,
        since: (2000, 2, 24),
    },
];

#[derive(Debug, Deserialize)]
pub struct OverlaysQuery {
    /// The DVR instant; the overlays are for that day. Absent is the
    /// newest complete day.
    pub at: Option<String>,
}

#[derive(Serialize)]
pub struct OverlayView {
    pub id: String,
    pub name: String,
    pub description: String,
    /// XYZ template with `{z}`, `{x}`, `{y}`; the date is already in it.
    pub tiles: String,
    pub date: NaiveDate,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub tile_size: u32,
    pub opacity: f64,
    pub attribution: serde_json::Value,
}

#[derive(Serialize)]
pub struct OverlaysResponse {
    /// The day the templates are for.
    pub date: NaiveDate,
    pub overlays: Vec<OverlayView>,
}

/// The day a product is asked for: the instant's date, but never later
/// than yesterday, because today's product is still being assembled.
pub fn day_for(at: Option<chrono::DateTime<Utc>>) -> NaiveDate {
    let latest = (Utc::now() - Duration::days(1)).date_naive();
    at.map(|t| t.date_naive()).unwrap_or(latest).min(latest)
}

pub fn overlays_for(day: NaiveDate) -> Vec<OverlayView> {
    PRODUCTS
        .iter()
        .filter(|p| {
            let (y, m, d) = p.since;
            NaiveDate::from_ymd_opt(y, m, d).is_some_and(|since| day >= since)
        })
        .map(|p| OverlayView {
            id: p.id.into(),
            name: p.name.into(),
            description: p.description.into(),
            tiles: format!("{GIBS}/{}/default/{}/{}/{{z}}/{{y}}/{{x}}.{}", p.layer, day.format("%Y-%m-%d"), p.matrix, p.format),
            date: day,
            min_zoom: 0,
            max_zoom: p.max_zoom,
            tile_size: 256,
            opacity: p.opacity,
            attribution: serde_json::json!({
                "provider": "NASA Global Imagery Browse Services (GIBS), EOSDIS",
                "url": "https://www.earthdata.nasa.gov/engage/open-data-services-software/earthdata-developer-portal/gibs-api",
                "license": "Public domain (NASA)",
                "notice": "We acknowledge the use of imagery provided by services from NASA's Global Imagery Browse Services (GIBS), part of NASA's Earth Observing System Data and Information System (EOSDIS)",
            }),
        })
        .collect()
}

pub async fn overlays(Query(query): Query<OverlaysQuery>) -> ApiResult<Json<OverlaysResponse>> {
    let at = query.at.as_deref().map(parse_instant).transpose()?;
    let day = day_for(at);
    Ok(Json(OverlaysResponse {
        date: day,
        overlays: overlays_for(day),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_day_is_never_later_than_yesterday_and_follows_the_dvr() {
        let yesterday = (Utc::now() - Duration::days(1)).date_naive();
        assert_eq!(day_for(None), yesterday);
        assert_eq!(day_for(Some(Utc::now())), yesterday, "today is still being assembled");
        let tuesday: chrono::DateTime<Utc> = "2026-09-08T14:00:00Z".parse().unwrap();
        assert_eq!(day_for(Some(tuesday)), NaiveDate::from_ymd_opt(2026, 9, 8).unwrap());
    }

    #[test]
    fn templates_name_the_day_and_products_that_did_not_exist_yet_are_left_out() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let all = overlays_for(day);
        assert_eq!(all.len(), PRODUCTS.len());
        let night = all.iter().find(|o| o.id == "night-lights").unwrap();
        assert_eq!(night.tiles, "https://gibs.earthdata.nasa.gov/wmts/epsg3857/best/VIIRS_SNPP_DayNightBand_At_Sensor_Radiance/default/2026-09-15/GoogleMapsCompatible_Level8/{z}/{y}/{x}.png");
        assert_eq!(night.max_zoom, 8);
        let early = overlays_for(NaiveDate::from_ymd_opt(2010, 1, 1).unwrap());
        assert!(early.iter().all(|o| o.id != "night-lights" && o.id != "viirs-true-colour"));
        assert!(early.iter().any(|o| o.id == "modis-terra-true-colour"));
    }
}
