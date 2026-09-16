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
