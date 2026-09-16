//! Poll CNEOS and OurAirports for real.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

fn gated() -> bool {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return false;
    }
    true
}

#[tokio::test]
async fn decades_of_fireballs_arrive_placed_and_chelyabinsk_is_among_them() {
    if !gated() {
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let obs = match argus_ingest::sources::Fireballs::new(http).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("cneos-fireballs", err),
    };
    // 887 records, about 800 with a position, when written.
    assert!(obs.len() > 500, "{} fireballs", obs.len());
    let chelyabinsk = obs.iter().find(|o| o.entity.key == "cneos:20130215T032026").expect("Chelyabinsk");
    assert_eq!(chelyabinsk.attrs["impact_energy_kt"], 441.0);
    let p = chelyabinsk.position.unwrap();
    assert!((p.lat - 54.8).abs() < 0.1 && (p.lon - 61.1).abs() < 0.1, "{p:?}");
    for o in &obs {
        assert_eq!(o.entity.kind, EntityKind::Event);
    }
}

#[tokio::test]
async fn every_airfield_in_the_world_arrives_with_heathrow_and_its_runways() {
    if !gated() {
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(60)).expect("an http client");
    let obs = match argus_ingest::sources::Airports::new(http).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("ourairports", err),
    };
    // 86,083 when written.
    assert!(obs.len() > 60_000, "{} airports", obs.len());
    let lhr = obs.iter().find(|o| o.entity.key == "airport:EGLL").expect("Heathrow");
    assert_eq!(lhr.attrs["iata"], "LHR");
    assert_eq!(lhr.attrs["runway_count"], 2);
    let with_icao = obs.iter().filter(|o| o.attrs.get("icao").is_some()).count();
    assert!(with_icao > 8_000, "{with_icao} with an ICAO code; there were 10,508");
    let with_runways = obs.iter().filter(|o| o.attrs.get("runways").is_some()).count();
    assert!(with_runways > 30_000, "{with_runways} with runways; the runways file has 48,000 rows");
    let keys: std::collections::HashSet<_> = obs.iter().map(|o| &o.entity.key).collect();
    assert_eq!(keys.len(), obs.len(), "idents are unique");
}

#[tokio::test]
async fn thousands_of_hf_paths_touch_the_british_isles_every_ten_minutes() {
    if !gated() {
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let ctx = argus_core::PollCtx {
        bbox: Some(argus_core::BoundingBox::new(-11.0, 49.5, 2.0, 61.0)),
        ..Default::default()
    };
    let obs = match argus_ingest::sources::WsprPaths::new(http).poll(&ctx).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("wspr", err),
    };
    // 6,770 in a September evening; the band is quieter in the small hours.
    assert!(obs.len() > 500, "{} paths", obs.len());
    let now = chrono::Utc::now();
    let mut bands = std::collections::BTreeSet::new();
    for o in &obs {
        assert_eq!(o.entity.kind, EntityKind::Measure);
        assert!(matches!(o.geom, Some(geo_types::Geometry::LineString(_))));
        assert!(now - o.observed_at < chrono::Duration::minutes(20), "{} last heard {}", o.entity.key, o.observed_at);
        bands.insert(o.attrs["band"].as_str().unwrap().to_string());
    }
    assert!(bands.len() >= 4, "bands open: {bands:?}");
    let far = obs.iter().filter(|o| o.attrs["distance_km"].as_f64().unwrap() > 5_000.0).count();
    assert!(far > 0, "no path over 5,000 km; the great circles have nothing to prove");
}
