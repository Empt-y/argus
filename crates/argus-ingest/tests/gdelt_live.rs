//! Read GDELT's newest published file for real.
//!
//! Walks forward from an hour ago and stops at the first listed file that
//! is not yet published. Asserts on the file's shape — a thousand placed
//! events, every code worded, every precision — never on status codes.
//! Network-gated: ARGUS_NETWORK_TESTS=1.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

#[tokio::test]
async fn an_hour_of_news_events_arrives_placed_and_worded() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(60)).expect("an http client");
    let obs = match argus_ingest::sources::Gdelt::new(http).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err @ (SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_))) => {
            eprintln!("SKIPPING gdelt: unavailable ({err})");
            return;
        }
        Err(err) => panic!("gdelt failed: {err}"),
    };
    // About 1,300 placed events per file; an hour back is up to five files,
    // fewer when the newest are not yet published.
    assert!(obs.len() > 800, "{} events placed", obs.len());
    let precisions: std::collections::HashSet<_> = obs.iter().map(|o| o.attrs["place_precision"].as_str().unwrap().to_string()).collect();
    assert!(precisions.contains("city") && precisions.contains("country"), "{precisions:?}");
    let bare = obs.iter().filter(|o| o.attrs["event"] == "event").count();
    assert_eq!(bare, 0, "{bare} events with a code the CAMEO table does not know");
    for o in obs.iter().take(500) {
        assert_eq!(o.entity.kind, EntityKind::Event);
        assert!(o.position.is_some_and(|p| p.is_plausible()));
        assert!(o.attrs["source_url"].as_str().is_some_and(|u| u.starts_with("http")));
        assert!((chrono::Utc::now() - o.observed_at) < chrono::Duration::hours(3), "{} is dated {}", o.entity.key, o.observed_at);
    }
    let keys: std::collections::HashSet<_> = obs.iter().map(|o| &o.entity.key).collect();
    assert_eq!(keys.len(), obs.len(), "event ids are unique");
}
