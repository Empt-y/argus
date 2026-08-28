//! Live failover against real providers.
//!
//! The unit tests prove the chain logic with mocks. This proves the thing that
//! actually matters: when a real provider is unreachable, real aircraft still
//! arrive from the next one, decoded by the same code path.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::geo::BoundingBox;
use argus_core::source::{PollCtx, Source};
use argus_ingest::sources::ReadsbProvider;
use argus_ingest::{HttpClient, ProviderChain};
use std::sync::Arc;

/// Heathrow's approach corridor: reliably busy at any hour, so an empty result
/// means a real failure rather than a quiet sky.
fn busy_airspace() -> BoundingBox {
    BoundingBox::new(-1.0, 51.0, 0.3, 51.9)
}

fn ctx() -> PollCtx {
    PollCtx {
        bbox: Some(busy_airspace()),
        ..Default::default()
    }
}

fn skip() -> bool {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("skipping: set ARGUS_NETWORK_TESTS=1 to run");
        return true;
    }
    false
}

#[tokio::test]
async fn a_dead_primary_is_carried_by_the_next_provider() {
    if skip() {
        return;
    }
    let http = HttpClient::new(std::time::Duration::from_secs(20)).unwrap();

    // A provider pointed at a host that does not resolve. Every other part of
    // the pipeline — decode, normalise, entity keys — is identical to the
    // healthy one, so this isolates the failover itself.
    let broken = ReadsbProvider::adsb_lol(http.clone());
    let mut broken_desc = broken.descriptor().clone();
    broken_desc.id = argus_core::SourceId::new("broken-primary");
    let broken = BrokenProvider {
        descriptor: broken_desc,
    };

    let chain = ProviderChain::new(
        "flights-failover-test",
        vec![
            Arc::new(broken),
            Arc::new(ReadsbProvider::adsb_fi(http.clone())),
        ],
    );

    let obs = chain.poll(&ctx()).await.expect("chain served despite dead primary");
    assert!(
        !obs.is_empty(),
        "fallback returned no aircraft over busy airspace"
    );

    let status = chain.status().await;
    println!(
        "serving via {} (rank {}), skipped: {:?}",
        status.serving, status.serving_rank, status.skipped
    );
    assert!(status.is_degraded(), "chain did not report itself degraded");
    assert_eq!(status.serving.as_str(), "adsb-fi");

    // And the data is real, not a shape that merely parsed.
    let with_position = obs.iter().filter(|o| o.position.is_some()).count();
    assert!(
        with_position > 5,
        "only {with_position} aircraft had positions"
    );
    assert!(
        obs.iter().all(|o| o.entity.kind == argus_core::EntityKind::Aircraft)
    );
}

#[tokio::test]
async fn both_real_providers_independently_see_the_same_sky() {
    if skip() {
        return;
    }
    let http = HttpClient::new(std::time::Duration::from_secs(20)).unwrap();
    let lol = ReadsbProvider::adsb_lol(http.clone());
    let fi = ReadsbProvider::adsb_fi(http.clone());

    let a = lol.poll(&ctx()).await.expect("adsb.lol");
    let b = fi.poll(&ctx()).await.expect("adsb.fi");
    println!("adsb.lol: {} aircraft, adsb.fi: {} aircraft", a.len(), b.len());

    assert!(!a.is_empty() && !b.is_empty(), "a provider returned nothing");

    // They are independent receiver networks, so they will not agree exactly.
    // But over the same busy airspace they must substantially overlap, or they
    // are not interchangeable and the chain's premise is wrong.
    let shared = a
        .iter()
        .filter(|x| b.iter().any(|y| y.entity == x.entity))
        .count();
    let smaller = a.len().min(b.len());
    println!("shared: {shared} of {smaller}");
    assert!(
        shared * 2 >= smaller,
        "only {shared} of {smaller} aircraft shared — providers are not interchangeable"
    );
}

/// A provider that always fails to connect, standing in for a dead upstream.
struct BrokenProvider {
    descriptor: argus_core::SourceDescriptor,
}

#[async_trait::async_trait]
impl Source for BrokenProvider {
    fn descriptor(&self) -> &argus_core::SourceDescriptor {
        &self.descriptor
    }

    async fn poll(
        &self,
        _ctx: &PollCtx,
    ) -> Result<Vec<argus_core::Observation>, argus_core::SourceError> {
        Err(argus_core::SourceError::Transport(
            "simulated outage: host unreachable".into(),
        ))
    }
}
