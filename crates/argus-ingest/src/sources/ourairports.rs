//! Every airport, airfield, heliport and seaplane base in the world, with
//! its runways, from OurAirports.
//!
//! OurAirports is a public-domain gazetteer maintained by volunteers and
//! published as CSV on GitHub: 86,083 airports (42,730 small airfields,
//! 23,214 heliports, 13,524 closed, 4,106 medium and 1,174 large airports,
//! 1,273 seaplane bases, 62 balloonports) in a 12.7 MB file, and 48,000
//! runways in a 4 MB one. 10,508 carry an ICAO code, which is the join to
//! the `metars` layer.
//!
//! An [`EntityKind::Feature`]: an airfield does not move, and a closed
//! one is still a fact about a field. Everything is kept, closed ones
//! marked. Weekly: the files change by a few rows a day, and the store
//! writes a feature only when something about it changed. The two files
//! are read on the raw GitHub host rather than the project's own domain,
//! which answered nothing to a plain GET when this was written.

use crate::http::HttpClient;
use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

const AIRPORTS_URL: &str = "https://raw.githubusercontent.com/davidmegginson/ourairports-data/main/airports.csv";
const RUNWAYS_URL: &str = "https://raw.githubusercontent.com/davidmegginson/ourairports-data/main/runways.csv";

const CADENCE_SECS: u64 = 7 * 24 * 3600;

pub struct Airports {
    descriptor: SourceDescriptor,
    http: HttpClient,
}

impl Airports {
    pub fn new(http: HttpClient) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("ourairports"),
                layer_id: LayerId::new("airports"),
                display_name: "Airports and airfields (OurAirports)".into(),
                kind: EntityKind::Feature,
                cadence: Cadence::every(CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::None,
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "OurAirports".into(),
                    url: "https://ourairports.com/data/".into(),
                    license: "Public domain".into(),
                    notice: Some("Airport data from OurAirports (public domain)".into()),
                },
                base_quality: Quality::Live,
                quota: None,
            },
            http,
        }
    }
}

#[async_trait::async_trait]
impl Source for Airports {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let airports = self.http.get_bytes(AIRPORTS_URL).await?;
        // Runways are the second file; without it an airport is still an
        // airport.
        let runways = match self.http.get_bytes(RUNWAYS_URL).await {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(source = %self.descriptor.id, %err, "runways file failed; airports written without runways");
                Vec::new()
            }
        };
        let now = Utc::now();
        let decoded = decode(&airports, &runways, &self.descriptor.id, now)?;
        tracing::info!(source = %self.descriptor.id, airports = decoded.observations.len(), runways = decoded.runways, unplaced = decoded.unplaced, "airports read");
        Ok(decoded.observations)
    }
}

// --- wire format -----------------------------------------------------------

#[derive(Debug)]
pub struct Decoded {
    pub observations: Vec<Observation>,
    pub runways: usize,
    pub unplaced: usize,
}

/// Runways by airport id, each as the attributes the card shows.
fn runways_by_airport(bytes: &[u8]) -> (HashMap<String, Vec<serde_json::Value>>, usize) {
    let mut out: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    let mut count = 0;
    if bytes.is_empty() {
        return (out, 0);
    }
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(bytes);
    let Ok(headers) = reader.headers().cloned() else { return (out, 0) };
    let col = |name: &str| headers.iter().position(|h| h == name);
    let (Some(c_ref), Some(c_le), Some(c_he)) = (col("airport_ref"), col("le_ident"), col("he_ident")) else { return (out, 0) };
    let c_len = col("length_ft");
    let c_wid = col("width_ft");
    let c_surf = col("surface");
    let c_lit = col("lighted");
    let c_closed = col("closed");
    let c_hdg = col("le_heading_degT");
    for record in reader.records().flatten() {
        let field = |c: Option<usize>| c.and_then(|i| record.get(i)).map(str::trim).filter(|s| !s.is_empty());
        let Some(airport) = field(Some(c_ref)) else { continue };
        count += 1;
        let ident = match (field(Some(c_le)), field(Some(c_he))) {
            (Some(a), Some(b)) => format!("{a}/{b}"),
            (Some(a), None) | (None, Some(a)) => a.to_string(),
            (None, None) => continue,
        };
        let mut r = serde_json::Map::new();
        r.insert("ident".into(), serde_json::json!(ident));
        if let Some(l) = field(c_len).and_then(|s| s.parse::<f64>().ok()) {
            r.insert("length_ft".into(), serde_json::json!(l));
        }
        if let Some(w) = field(c_wid).and_then(|s| s.parse::<f64>().ok()) {
            r.insert("width_ft".into(), serde_json::json!(w));
        }
        if let Some(s) = field(c_surf) {
            r.insert("surface".into(), serde_json::json!(s));
        }
        if let Some(h) = field(c_hdg).and_then(|s| s.parse::<f64>().ok()) {
            r.insert("heading_deg".into(), serde_json::json!(h));
        }
        if field(c_lit) == Some("1") {
            r.insert("lighted".into(), serde_json::json!(true));
        }
        if field(c_closed) == Some("1") {
            r.insert("closed".into(), serde_json::json!(true));
        }
        out.entry(airport.to_string()).or_default().push(serde_json::Value::Object(r));
    }
    (out, count)
}

pub fn decode(airports: &[u8], runways: &[u8], source_id: &SourceId, now: DateTime<Utc>) -> Result<Decoded, SourceError> {
    let (runways, runway_count) = runways_by_airport(runways);
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(airports);
    let headers = reader.headers().map_err(|e| SourceError::Decode(e.to_string()))?.clone();
    let col = |name: &str| headers.iter().position(|h| h == name);
    let (Some(c_id), Some(c_ident), Some(c_type), Some(c_name), Some(c_lat), Some(c_lon)) =
        (col("id"), col("ident"), col("type"), col("name"), col("latitude_deg"), col("longitude_deg"))
    else {
        return Err(SourceError::Decode(format!("airports.csv columns changed: {}", headers.iter().collect::<Vec<_>>().join(","))));
    };
    let c_elev = col("elevation_ft");
    let c_country = col("iso_country");
    let c_region = col("iso_region");
    let c_town = col("municipality");
    let c_sched = col("scheduled_service");
    let c_icao = col("icao_code");
    let c_iata = col("iata_code");
    let c_gps = col("gps_code");
    let c_local = col("local_code");
    let c_home = col("home_link");
    let c_wiki = col("wikipedia_link");

    let mut observations = Vec::with_capacity(90_000);
    let mut unplaced = 0;
    for record in reader.records() {
        let r = record.map_err(|e| SourceError::Decode(e.to_string()))?;
        let field = |c: Option<usize>| c.and_then(|i| r.get(i)).map(str::trim).filter(|s| !s.is_empty());
        let (Some(id), Some(ident), Some(name)) = (field(Some(c_id)), field(Some(c_ident)), field(Some(c_name))) else { continue };
        let (Some(lat), Some(lon)) = (field(Some(c_lat)).and_then(|s| s.parse::<f64>().ok()), field(Some(c_lon)).and_then(|s| s.parse::<f64>().ok())) else {
            unplaced += 1;
            continue;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || (lat == 0.0 && lon == 0.0) {
            unplaced += 1;
            continue;
        }
        let kind = field(Some(c_type)).unwrap_or("");
        let elevation_ft = field(c_elev).and_then(|s| s.parse::<f64>().ok());
        let icao = field(c_icao);
        let iata = field(c_iata);

        let mut attrs = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                attrs.insert(k.to_string(), v);
            }
        };
        put("ident", serde_json::json!(ident));
        put("name", serde_json::json!(name));
        put("type", serde_json::json!(kind));
        put("closed", serde_json::json!(if kind == "closed" { Some(true) } else { None }));
        put("icao", serde_json::json!(icao));
        put("iata", serde_json::json!(iata));
        put("gps_code", serde_json::json!(field(c_gps).filter(|g| Some(*g) != icao)));
        put("local_code", serde_json::json!(field(c_local)));
        put("elevation_ft", serde_json::json!(elevation_ft));
        put("country", serde_json::json!(field(c_country)));
        put("region", serde_json::json!(field(c_region)));
        put("municipality", serde_json::json!(field(c_town)));
        put("scheduled_service", serde_json::json!(field(c_sched).map(|s| s == "yes")));
        put("url", serde_json::json!(field(c_home).filter(|u| u.starts_with("http"))));
        put("wikipedia", serde_json::json!(field(c_wiki).filter(|u| u.starts_with("http"))));
        if let Some(rw) = runways.get(id) {
            put("runway_count", serde_json::json!(rw.len()));
            put("runways", serde_json::json!(rw));
        }

        let label = match (iata, icao) {
            (Some(a), _) => format!("{name} ({a})"),
            (None, Some(i)) => format!("{name} ({i})"),
            (None, None) => name.to_string(),
        };
        observations.push(
            Observation::new(source_id.clone(), EntityId::new(EntityKind::Feature, format!("airport:{ident}")), now, Quality::Live)
                .with_position(Position { lon, lat, alt_m: elevation_ft.map(|f| f * 0.3048), datum: AltitudeDatum::Geoid })
                .with_label(label)
                .with_attrs(serde_json::Value::Object(attrs)),
        );
    }
    if observations.is_empty() && unplaced == 0 {
        return Err(SourceError::Decode("airports.csv had a header and no rows".into()));
    }
    Ok(Decoded { observations, runways: runway_count, unplaced })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AIRPORTS: &str = "\"id\",\"ident\",\"type\",\"name\",\"latitude_deg\",\"longitude_deg\",\"elevation_ft\",\"continent\",\"iso_country\",\"iso_region\",\"municipality\",\"scheduled_service\",\"icao_code\",\"iata_code\",\"gps_code\",\"local_code\",\"home_link\",\"wikipedia_link\",\"keywords\"
2434,\"EGLL\",\"large_airport\",\"London Heathrow Airport\",51.4706,-0.461941,83,\"EU\",\"GB\",\"GB-ENG\",\"London\",\"yes\",\"EGLL\",\"LHR\",\"EGLL\",,\"http://www.heathrowairport.com/\",\"https://en.wikipedia.org/wiki/Heathrow_Airport\",\"LON, Londres\"
6523,\"00A\",\"heliport\",\"Total RF Heliport\",40.070985,-74.933689,11,\"NA\",\"US\",\"US-PA\",\"Bensalem\",\"no\",,,\"K00A\",\"00A\",,,
9999,\"XXXX\",\"closed\",\"Old Field\",,,,\"EU\",\"GB\",\"GB-ENG\",,\"no\",,,,,,,
";
    const RUNWAYS: &str = "\"id\",\"airport_ref\",\"airport_ident\",\"length_ft\",\"width_ft\",\"surface\",\"lighted\",\"closed\",\"le_ident\",\"le_latitude_deg\",\"le_longitude_deg\",\"le_elevation_ft\",\"le_heading_degT\",\"le_displaced_threshold_ft\",\"he_ident\",\"he_latitude_deg\",\"he_longitude_deg\",\"he_elevation_ft\",\"he_heading_degT\",\"he_displaced_threshold_ft\"
1,2434,\"EGLL\",12799,164,\"ASP\",1,0,\"09L\",51.4775,-0.4893,79,89.6,1007,\"27R\",51.4777,-0.4340,78,269.6,
2,2434,\"EGLL\",12008,164,\"ASP\",1,0,\"09R\",51.4648,-0.4826,75,89.6,1007,\"27L\",51.4649,-0.4290,77,269.6,
3,6523,\"00A\",80,80,\"ASPH-G\",1,0,\"H1\",,,,,,,,,,,
";

    #[test]
    fn an_airport_is_a_feature_with_its_runways_and_a_closed_one_without_a_position_is_not_placed() {
        let d = decode(AIRPORTS.as_bytes(), RUNWAYS.as_bytes(), &SourceId::new("ourairports"), Utc::now()).unwrap();
        assert_eq!(d.runways, 3);
        assert_eq!(d.unplaced, 1);
        assert_eq!(d.observations.len(), 2);
        let lhr = &d.observations[0];
        assert_eq!(lhr.entity.key, "airport:EGLL");
        assert_eq!(lhr.entity.kind, EntityKind::Feature);
        assert_eq!(lhr.label.as_deref(), Some("London Heathrow Airport (LHR)"));
        assert_eq!(lhr.attrs["runway_count"], 2);
        assert_eq!(lhr.attrs["runways"][0]["ident"], "09L/27R");
        assert_eq!(lhr.attrs["runways"][0]["length_ft"], 12799.0);
        assert_eq!(lhr.attrs["scheduled_service"], true);
        assert!(lhr.attrs.get("gps_code").is_none(), "a GPS code equal to the ICAO code is noise");
        assert!((lhr.position.unwrap().alt_m.unwrap() - 25.3).abs() < 0.1);
        let heli = &d.observations[1];
        assert_eq!(heli.attrs["type"], "heliport");
        assert_eq!(heli.attrs["gps_code"], "K00A");
        assert_eq!(heli.attrs["runways"][0]["ident"], "H1");
    }
}
