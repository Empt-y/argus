//! Poll PeeringDB and root-servers.org for real.
//!
//! Three PeeringDB pulls ten seconds apart and one script. Asserts on the
//! shape of the register, never on status codes. Network-gated:
//! ARGUS_NETWORK_TESTS=1.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => eprintln!("SKIPPING {what}: unavailable ({err})"),
        other => panic!("{what} failed: {other}"),
    }
}

fn gated() -> bool {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feeds");
        return false;
    }
    true
}

#[tokio::test]
async fn thousands_of_facilities_arrive_placed_with_their_tenants_counted() {
    if !gated() {
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(120)).expect("an http client");
    let shared = std::sync::Arc::new(argus_ingest::sources::peeringdb::Shared::new(http));
    let obs = match argus_ingest::sources::PeeringDbFacilities::new(shared).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("peeringdb-fac", err),
    };
    // 5,263 of 5,874 had coordinates when written.
    assert!(obs.len() > 4_000, "{} facilities placed", obs.len());
    let ashburn = obs.iter().find(|o| o.entity.key == "pdb:fac:1").expect("Equinix Ashburn, facility 1");
    assert_eq!(ashburn.attrs["operator"], "Equinix, Inc.");
    assert!(ashburn.attrs["networks"].as_u64().unwrap() > 100);
    for o in obs.iter().take(500) {
        assert_eq!(o.entity.kind, EntityKind::Feature);
        assert!(o.position.is_some_and(|p| p.is_plausible()));
    }
}

#[tokio::test]
async fn hundreds_of_exchanges_are_placed_through_their_facilities() {
    if !gated() {
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(120)).expect("an http client");
    let shared = std::sync::Arc::new(argus_ingest::sources::peeringdb::Shared::new(http));
    let obs = match argus_ingest::sources::PeeringDbExchanges::new(shared).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("peeringdb-ix", err),
    };
    // 915 of 1,324 when written.
    assert!(obs.len() > 700, "{} exchanges placed", obs.len());
    let multi = obs.iter().filter(|o| matches!(o.geom, Some(geo_types::Geometry::MultiPoint(_)))).count();
    let single = obs.iter().filter(|o| matches!(o.geom, Some(geo_types::Geometry::Point(_)))).count();
    assert!(multi > 200 && single > 200, "{multi} in several buildings, {single} in one");
    let linx = obs.iter().find(|o| o.attrs["name"].as_str().is_some_and(|n| n.starts_with("LINX LON1"))).expect("LINX LON1");
    assert!(linx.attrs["networks"].as_u64().unwrap() > 500, "{}", linx.attrs["networks"]);
}

#[tokio::test]
async fn every_root_server_letter_has_sites_and_they_number_over_a_thousand() {
    if !gated() {
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let obs = match argus_ingest::sources::RootServers::new(http).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("root-servers", err),
    };
    // 1,573 rows, 1,467 sites once co-located rows merge, when written.
    assert!(obs.len() > 1_000, "{} root server sites", obs.len());
    let letters: std::collections::HashSet<_> = obs.iter().map(|o| o.attrs["letter"].as_str().unwrap().to_string()).collect();
    assert_eq!(letters.len(), 13, "{letters:?}");
    let keys: std::collections::HashSet<_> = obs.iter().map(|o| &o.entity.key).collect();
    assert_eq!(keys.len(), obs.len(), "keys are unique");
    let instances: u64 = obs.iter().map(|o| o.attrs["instances"].as_u64().unwrap()).sum();
    assert!(instances > obs.len() as u64, "{instances} instances across {} sites", obs.len());
}
