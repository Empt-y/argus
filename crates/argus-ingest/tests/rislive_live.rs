//! Hold the RIS Live socket for a minute and a half and see what it says.
//!
//! Asserts that most collectors reported and that the rates are the
//! order of magnitude measured when this was written (4,600 messages a
//! second across the network). Network-gated: ARGUS_NETWORK_TESTS=1.

use argus_core::entity::EntityKind;
use argus_core::source::Source;

#[tokio::test]
async fn ninety_seconds_of_the_stream_yields_a_rate_for_most_collectors() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to open the live socket");
        return;
    }
    let source = argus_ingest::sources::RisLive::new();
    let first = source.poll(&argus_core::PollCtx::default()).await.expect("the first poll starts the reader");
    assert!(first.is_empty(), "nothing has been heard yet");
    tokio::time::sleep(std::time::Duration::from_secs(90)).await;
    let obs = source.poll(&argus_core::PollCtx::default()).await.expect("a drain after ninety seconds");
    assert!(obs.len() >= 15, "{} collectors reported; there are 23", obs.len());
    let total: f64 = obs.iter().map(|o| o.attrs["updates_per_min"].as_f64().unwrap()).sum();
    assert!(total > 20_000.0, "{total} updates a minute across the network; 4,600 a second was measured");
    for o in &obs {
        assert_eq!(o.entity.kind, EntityKind::Measure);
        assert!(o.attrs["peers_heard"].as_i64().unwrap() > 0);
        assert!(o.position.is_some());
    }
}
