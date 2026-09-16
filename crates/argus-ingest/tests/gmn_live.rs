//! Fetch the Global Meteor Network's daily files for real.
//!
//! The unit tests hold the decoder to a captured header and two rows. What
//! they cannot see: the directory moving, the aliases being renamed, or a
//! header change that the by-name lookup turns into a loud error.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

/// 4,031 trajectories in one complete day when this was written. The first
/// poll also backfills the week, so this is a floor on the day alone.
const DAY_SEEN: usize = 4_031;

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

#[tokio::test]
async fn a_week_of_meteors_arrives_with_lines_and_heights() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    let http =
        argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let source = argus_ingest::sources::GmnMeteors::new(http);

    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("gmn-meteors", err),
    };
    // A week's backfill plus yesterday plus today: well over one day's worth.
    let floor = DAY_SEEN;
    assert!(
        observations.len() >= floor,
        "{} meteors, expected at least {floor} from a week of files",
        observations.len()
    );

    let now = chrono::Utc::now();
    let horizon = EntityKind::Event.live_horizon().unwrap();
    let mut with_line = 0usize;
    let mut showers = std::collections::HashSet::new();
    let mut oldest = chrono::Duration::zero();
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Event);
        let p = o
            .position
            .expect("a meteor without a beginning was emitted");
        let alt_km = p.alt_m.expect("a meteor without a height") / 1000.0;
        assert!(
            (40.0..=200.0).contains(&alt_km),
            "{} began at {alt_km} km",
            o.entity.key
        );
        let age = now - o.observed_at;
        assert!(
            age >= chrono::Duration::hours(-1),
            "{} is in the future",
            o.entity.key
        );
        oldest = oldest.max(age);
        if o.geom.is_some() {
            with_line += 1;
        }
        if let Some(s) = o.attrs.get("shower").and_then(|v| v.as_str()) {
            showers.insert(s.to_string());
        }
    }
    assert!(
        with_line * 100 >= observations.len() * 99,
        "only {with_line} of {} have a line",
        observations.len()
    );
    assert!(showers.len() >= 5, "only {} showers named", showers.len());
    // The backfill reaches back to the horizon and no further.
    assert!(
        oldest <= horizon + chrono::Duration::days(1),
        "oldest meteor is {} days old",
        oldest.num_days()
    );
    assert!(
        oldest >= chrono::Duration::days(3),
        "oldest meteor is only {} hours old; the backfill did not happen",
        oldest.num_hours()
    );

    eprintln!(
        "gmn: {} meteors, {with_line} with lines, {} showers, oldest {} days",
        observations.len(),
        showers.len(),
        oldest.num_days()
    );
}
