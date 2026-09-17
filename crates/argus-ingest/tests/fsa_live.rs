//! Poll the FSA's food hygiene register for real: all 363 files.
//!
//! Slow — a quarter-second between files plus the downloads, four or five
//! minutes — and it asserts on the register's shape, not on status codes: a
//! council that quietly published an empty file would show here as a count.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

fn gated() -> bool {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return false;
    }
    true
}

#[tokio::test]
async fn the_whole_register_arrives_placed_with_both_schemes_and_every_rating() {
    if !gated() {
        return;
    }
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(60)).expect("an http client");
    let obs = match argus_ingest::sources::FoodHygiene::new(http).poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err @ (SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_))) => {
            eprintln!("SKIPPING fsa-hygiene: unavailable ({err})");
            return;
        }
        Err(err) => panic!("fsa-hygiene failed: {err}"),
    };
    // 455,840 placed of 613,379 when written; a quarter have no geocode.
    assert!(obs.len() > 400_000, "{} establishments placed", obs.len());
    let keys: std::collections::HashSet<_> = obs.iter().map(|o| &o.entity.key).collect();
    assert!(keys.len() >= obs.len() - 5, "{} distinct keys for {} rows; one FHRSID was duplicated when written", keys.len(), obs.len());
    let scottish = obs.iter().filter(|o| o.attrs["scheme"] == "FHIS").count();
    assert!(scottish > 40_000, "{scottish} FHIS establishments; Scotland had 58,984 with 33,497 placed");
    let rated_5 = obs.iter().filter(|o| o.attrs["rating"] == 5).count();
    assert!(rated_5 > 200_000, "{rated_5} rated 5; there were 281,364");
    let awaiting = obs.iter().filter(|o| o.attrs["rating"] == "awaiting_inspection").count();
    assert!(awaiting > 10_000, "{awaiting} awaiting inspection; there were 27,483 across three spellings");
    let scored = obs.iter().filter(|o| o.attrs.get("hygiene_points").is_some()).count();
    assert!(scored > 300_000, "{scored} with inspection scores; there were 369,413");
    for o in obs.iter().take(1000) {
        assert_eq!(o.entity.kind, EntityKind::Feature);
        let p = o.position.expect("placed");
        assert!((-9.0..2.5).contains(&p.lon) && (49.0..61.5).contains(&p.lat), "{p:?} is not in the UK");
        assert!(!o.attrs.to_string().contains("&lt;"), "escaped HTML reached the store: {}", o.attrs);
    }
}
