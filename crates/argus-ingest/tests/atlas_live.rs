//! Page every connected and disconnected RIPE Atlas probe for real.
//!
//! Thirty-five pages at one a second. Asserts on the network's shape —
//! count, anchors, both states, positions — never on status codes.
//! Network-gated: ARGUS_NETWORK_TESTS=1.

use argus_core::entity::{EntityKind, Quality};
use argus_core::source::{Source, SourceError};

#[tokio::test]
async fn the_whole_probe_network_arrives_with_anchors_and_both_connection_states() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(60)).expect("an http client");
    let obs = match argus_ingest::sources::AtlasProbes::new(http).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err @ (SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_))) => {
            eprintln!("SKIPPING ripe-atlas: unavailable ({err})");
            return;
        }
        Err(err) => panic!("ripe-atlas failed: {err}"),
    };
    // 17,238 when written, 14 without a position.
    assert!(obs.len() > 12_000, "{} probes placed", obs.len());
    let connected = obs.iter().filter(|o| o.quality == Quality::Live).count();
    let disconnected = obs.iter().filter(|o| o.quality == Quality::Stale).count();
    assert!(connected > 10_000 && disconnected > 500, "{connected} connected, {disconnected} disconnected; 15,059 and 2,179 when written");
    let anchors = obs.iter().filter(|o| o.attrs["anchor"] == true).count();
    assert!(anchors > 700, "{anchors} anchors; there were 1,067");
    for o in obs.iter().take(2000) {
        assert_eq!(o.entity.kind, EntityKind::Station);
        assert!(o.position.is_some_and(|p| p.is_plausible()));
        assert!(!o.attrs.to_string().contains("address_v4"), "addresses are not stored");
    }
    let keys: std::collections::HashSet<_> = obs.iter().map(|o| &o.entity.key).collect();
    assert_eq!(keys.len(), obs.len(), "probe ids are unique");
}
