//! A stop must not wait for a crawl.
//!
//! Every poll used to run to completion before the runtime would return,
//! which for a driver that paces itself with sleeps between tiles meant a
//! `pkill argusd` returned long before the daemon exited — and a grid crawl
//! kept a stopped daemon alive for an hour beside its replacement. This
//! pins the fix: a source mid-poll is abandoned the moment the token is
//! cancelled, and `run` returns within a second of it.
//!
//! Needs a database, like the store's own tests:
//!   ARGUS_TEST_DATABASE_URL=postgres://argus@localhost/argus_test cargo test -p argus-ingest

use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Source,
    SourceDescriptor, SourceId,
};
use argus_core::{EntityKind, Quality};
use argus_ingest::runtime::{CredentialResolver, Runtime};
use argus_ingest::scheduler::SchedulerConfig;
use argus_store::Store;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A source whose every poll takes a minute, like a tile crawl.
struct SlowCrawl {
    descriptor: SourceDescriptor,
    finished: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Source for SlowCrawl {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<argus_core::Observation>, argus_core::SourceError> {
        tokio::time::sleep(Duration::from_secs(60)).await;
        self.finished.store(true, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn cancelling_mid_poll_stops_the_runtime_without_waiting_for_the_poll() {
    let Some(url) = std::env::var("ARGUS_TEST_DATABASE_URL").ok() else {
        eprintln!("skipping: ARGUS_TEST_DATABASE_URL not set");
        return;
    };
    let store = Store::connect(&url, 2).await.expect("connect to test database");
    store.migrate().await.expect("migrations apply");

    let finished = Arc::new(AtomicBool::new(false));
    let mut runtime = Runtime::new(store, SchedulerConfig::default());
    runtime.register(Arc::new(SlowCrawl {
        descriptor: SourceDescriptor {
            id: SourceId::new("test-slow-crawl"),
            layer_id: LayerId::new("test-slow-crawl"),
            display_name: "Slow crawl".into(),
            kind: EntityKind::Feature,
            cadence: Cadence::every(3600),
            coverage: Coverage::Global,
            auth: AuthRequirement::None,
            cost: CostClass::Free,
            attribution: Attribution {
                provider: "Test".into(),
                url: String::new(),
                license: "None".into(),
                notice: None,
            },
            base_quality: Quality::Live,
            quota: None,
        },
        finished: finished.clone(),
    }));
    let cancel = runtime.cancel_token();
    let credentials = CredentialResolver::new(Default::default());

    let run = tokio::spawn(async move { runtime.run(&credentials, u64::MAX, 0.9).await });
    // A lone source starts its first poll straight away; this is long enough
    // for it to be a minute's sleep in, and short enough to matter.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let stopped_at = Instant::now();
    cancel.cancel();

    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the runtime returned promptly after cancellation")
        .expect("the runtime task did not panic")
        .expect("the runtime stopped cleanly");
    assert!(stopped_at.elapsed() < Duration::from_secs(2), "took {:?}", stopped_at.elapsed());
    assert!(!finished.load(Ordering::SeqCst), "the poll was abandoned, not waited for");
}
