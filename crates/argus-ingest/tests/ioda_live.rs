//! Poll IODA for real: a day of outage events placed on their outlines.
//!
//! Downloads both topologies (57 MB) and asks RIPEstat for every AS-wide
//! event's country, so it runs a few minutes. Asserts on the shape of the
//! answer, never on status codes. Network-gated: ARGUS_NETWORK_TESTS=1.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};
use std::sync::Arc;

#[tokio::test]
async fn a_day_of_outages_arrives_with_every_scope_drawn_on_an_outline() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(120)).expect("an http client");
    let ripestat = Arc::new(argus_ingest::ripestat::RipeStat::new(http.clone()));
    let source = argus_ingest::sources::Ioda::new(http, ripestat);
    let obs = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err @ (SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_))) => {
            eprintln!("SKIPPING ioda: unavailable ({err})");
            return;
        }
        Err(err) => panic!("ioda failed: {err}"),
    };
    // 2,000 events in 24 hours when written, the page limit; a quiet day is
    // still hundreds.
    assert!(obs.len() > 200, "{} outage events placed", obs.len());
    let scopes = |s: &str| obs.iter().filter(|o| o.attrs["scope"] == s).count();
    for scope in ["country", "region", "network", "network in region"] {
        assert!(scopes(scope) > 0, "no {scope} events among {}", obs.len());
    }
    for o in &obs {
        assert_eq!(o.entity.kind, EntityKind::Event);
        assert!(o.geom.is_some(), "{} has no outline", o.entity.key);
        assert!(o.observed_at < chrono::Utc::now(), "{} starts in the future", o.entity.key);
    }
    let as_wide = obs.iter().filter(|o| o.attrs["placed_by"] == "registered country").count();
    assert!(as_wide > 0, "AS-wide events are drawn on their registration country");
    let bgp = obs.iter().filter(|o| o.attrs["datasource"] == "bgp").count();
    assert!(bgp > 50, "{bgp} BGP-visibility events; there were 1,330");
}
