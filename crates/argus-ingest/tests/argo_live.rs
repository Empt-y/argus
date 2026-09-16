//! Poll the Coriolis ERDDAP for real and assert on what comes back.
//!
//! The unit tests hold the decoder to a captured envelope. What they cannot
//! see: the dataset being renamed or its columns changing (a loud error, by
//! design), the server slowing past the patient client's limit, or the
//! window quietly returning a fraction of the array.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::{EntityKind, Quality};
use argus_core::source::{Source, SourceError};

/// 4,315 floats had reported within thirty days when this was written.
const FLOATS_SEEN: usize = 4_315;

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

#[tokio::test]
async fn the_array_arrives_whole_and_dated_by_its_surfacings() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    // The daemon gives this source the 120-second patient client; a
    // thirty-day query took twenty seconds. The same here, so the test runs
    // under the driver's real patience and not a longer one.
    let http =
        argus_ingest::HttpClient::new(std::time::Duration::from_secs(120)).expect("an http client");
    let source = argus_ingest::sources::ArgoFloats::new(http);

    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("argo-floats", err),
    };

    let floor = FLOATS_SEEN / 2;
    assert!(
        observations.len() >= floor,
        "{} floats reporting, expected at least {floor}",
        observations.len()
    );

    let now = chrono::Utc::now();
    let mut live = 0usize;
    let mut keys = std::collections::HashSet::new();
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Station);
        assert!(
            keys.insert(o.entity.key.clone()),
            "{} appeared twice",
            o.entity.key
        );
        let p = o.position.expect("a float without a position was emitted");
        assert!(
            (-180.0..=180.0).contains(&p.lon) && (-90.0..=90.0).contains(&p.lat),
            "{} is at {:.3},{:.3}",
            o.entity.key,
            p.lon,
            p.lat
        );
        // Dated by the poll, so never in the past or future by more than
        // the poll took.
        assert!((now - o.observed_at).num_seconds().abs() < 300);
        let surfaced = o.attrs["surfaced_at"]
            .as_str()
            .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok())
            .expect("every float carries its surfacing time");
        let age = now - surfaced;
        assert!(
            age <= chrono::Duration::days(31) && age >= chrono::Duration::hours(-1),
            "{} surfaced {} hours ago, outside the thirty-day window",
            o.entity.key,
            age.num_hours()
        );
        // The quality must agree with the age: a week-old position marked
        // live is the lie this layer exists to avoid.
        let within = age <= EntityKind::Station.live_horizon().unwrap();
        assert_eq!(
            o.quality == Quality::Live,
            within,
            "{} is {} hours old and {:?}",
            o.entity.key,
            age.num_hours(),
            o.quality
        );
        if within {
            live += 1;
        }
    }

    // 357 of 4,315 had surfaced within a day. Zero means the stamps or the
    // horizon have gone wrong; more than half means the window has.
    assert!(
        live > 50 && live * 2 < observations.len(),
        "{live} of {} floats are within the day",
        observations.len()
    );

    eprintln!(
        "argo: {} floats, {live} surfaced within 24h",
        observations.len()
    );
}
