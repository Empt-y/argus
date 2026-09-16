//! Sample Open-Meteo's air quality and marine grids for real, over the
//! home area.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run. Costs about 200 of
//! the day's 10,000 calls.

use argus_core::entity::{EntityKind, Quality};
use argus_core::source::{Source, SourceError};
use argus_core::BoundingBox;

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

fn ctx(bbox: BoundingBox) -> argus_core::PollCtx {
    argus_core::PollCtx {
        bbox: Some(bbox),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_home_area_answers_on_a_quarter_degree_lattice_with_pollen() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let source = argus_ingest::sources::OpenMeteo::air_quality(http);
    let home = BoundingBox::new(-2.5, 51.0, 0.5, 52.5);
    let observations = match source.poll(&ctx(home)).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("open-meteo-air", err),
    };
    assert_eq!(observations.len(), 72, "12 by 6 cells at 0.25°");
    let now = chrono::Utc::now();
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Measure);
        assert_eq!(o.quality, Quality::Modeled);
        assert!(o.observed_at <= now && now - o.observed_at < chrono::Duration::hours(2), "{} dated {}", o.entity.key, o.observed_at);
        let p = o.position.expect("a position");
        // The model answers for its own cell, within a cell of what was asked.
        assert!(home.expanded(0.15).contains(p.lon, p.lat), "{} at {:.2},{:.2}", o.entity.key, p.lon, p.lat);
        let aqi = o.attrs["european_aqi"].as_f64().expect("an AQI");
        assert!((0.0..=500.0).contains(&aqi));
        // Europe has pollen; the field is present even when it is zero.
        assert!(o.attrs.get("grass_pollen_grains_m3").is_some(), "{} has no pollen: {}", o.entity.key, o.attrs);
        assert_eq!(o.attrs["lattice_spacing_deg"], 0.25);
    }
}

#[tokio::test]
async fn the_wave_model_answers_for_the_sea_and_not_the_land() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let source = argus_ingest::sources::OpenMeteo::sea_state(http);
    // The Channel, the south coast and Hampshire up to Basingstoke: sea,
    // coast and land. 10 by 8 at 0.2°.
    let bbox = BoundingBox::new(-2.0, 49.5, 0.0, 51.1);
    let observations = match source.poll(&ctx(bbox)).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("open-meteo-marine", err),
    };
    // The model answers a coastal land point with the nearest sea cell,
    // and an inland one with nothing; so fewer than asked, and never two
    // readings for one cell.
    assert!(observations.len() > 40 && observations.len() < 80, "{} sea cells of 80 asked", observations.len());
    let keys: std::collections::HashSet<_> = observations.iter().map(|o| &o.entity.key).collect();
    assert_eq!(keys.len(), observations.len(), "one reading per model cell");
    for o in &observations {
        assert_eq!(o.quality, Quality::Modeled);
        let h = o.attrs["wave_height_m"].as_f64().expect("a wave height");
        assert!((0.0..=30.0).contains(&h));
        let p = o.position.expect("a position");
        assert!(p.lat < 51.0, "{} at {:.2}N is well inland", o.entity.key, p.lat);
    }
}

#[tokio::test]
async fn river_discharge_is_sampled_at_a_thousand_gauged_cells_and_the_thames_flows() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let source = argus_ingest::sources::RiverDischarge::new(http);
    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("glofas", err),
    };
    // 1,078 tenth-degree cells held a river gauge when written.
    assert!(observations.len() > 700, "{} cells", observations.len());
    let thames: Vec<_> = observations.iter().filter(|o| o.attrs["rivers"].as_array().is_some_and(|r| r.iter().any(|x| x == "River Thames"))).collect();
    assert!(thames.len() > 10, "{} Thames cells", thames.len());
    // Somewhere on the Thames the model has a river, not a ditch.
    assert!(thames.iter().any(|o| o.attrs["discharge_m3s"].as_f64().unwrap() > 5.0), "no Thames cell over 5 m³/s: {:?}", thames.iter().map(|o| o.attrs["discharge_m3s"].as_f64()).collect::<Vec<_>>());
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Measure);
        assert_eq!(o.quality, Quality::Modeled);
        assert!(o.attrs["discharge_7d_m3s"].as_array().is_some_and(|a| a.len() >= 2));
    }
}
