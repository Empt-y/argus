//! Poll GRIP for real: the last few hours of routing events, placed.
//!
//! Asks RIPEstat about a few hundred prefixes at four a second, so it
//! runs a minute or two. Network-gated: ARGUS_NETWORK_TESTS=1.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};
use std::sync::Arc;

#[tokio::test]
async fn hours_of_routing_events_arrive_placed_with_their_suspicion_and_their_networks() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(120)).expect("an http client");
    let ripestat = Arc::new(argus_ingest::ripestat::RipeStat::new(http.clone()));
    let obs = match argus_ingest::sources::Grip::new(http, ripestat).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err @ (SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_))) => {
            eprintln!("SKIPPING grip: unavailable ({err})");
            return;
        }
        Err(err) => panic!("grip failed: {err}"),
    };
    // 300 fetched, about 70% above the suspicion floor, nearly all placed.
    assert!(obs.len() > 100, "{} routing events placed", obs.len());
    for o in &obs {
        assert_eq!(o.entity.kind, EntityKind::Event);
        assert!(o.position.is_some_and(|p| p.is_plausible()), "{} is not placed", o.entity.key);
        assert!(o.attrs["suspicion"].as_i64().unwrap() >= 20);
        assert!(o.attrs["prefixes"].as_array().is_some_and(|p| !p.is_empty()));
    }
    let types: std::collections::HashSet<_> = obs.iter().map(|o| o.attrs["event_type"].as_str().unwrap().to_string()).collect();
    assert!(types.len() >= 2, "one event type only: {types:?}");
    let named = obs.iter().filter(|o| o.attrs["newcomers"][0].get("name").is_some()).count();
    assert!(named * 2 > obs.len(), "{named} of {} events name a network", obs.len());
}
