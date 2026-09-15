//! Poll NDBC for real and assert on what comes back.
//!
//! The unit tests hold the decoder to the 22-field contract against captured
//! rows. What they cannot see is the file changing shape underneath them: a
//! new column, a station table that stops joining, or a download that
//! truncates to a valid-looking prefix. All three produce a decodable answer
//! with fewer stations in it, and only a count notices.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

/// 876 stations were reporting when this was written. Half is well under the
/// day-to-day swing and well over what a truncated file or a column shift
/// leaves behind.
const STATIONS_SEEN: usize = 876;

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

#[tokio::test]
async fn the_network_arrives_whole_and_named() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    // The daemon's shared client allows 30 seconds; the whole file arrived in
    // under half a second, so this deliberately tests the same patience the
    // driver gets in production.
    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(30))
        .expect("an http client");
    let source = argus_ingest::sources::NdbcBuoys::new(http);

    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("ndbc-buoys", err),
    };

    let floor = STATIONS_SEEN / 2;
    assert!(
        observations.len() >= floor,
        "{} stations reporting, expected at least {floor} — a short answer is what a \
         truncated file or a shifted column looks like",
        observations.len()
    );

    let now = chrono::Utc::now();
    let mut named = 0usize;
    let mut fresh = 0usize;
    let mut oldest = chrono::Duration::zero();
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Station);
        let p = o.position.expect("a station without a position was emitted");
        assert!(
            (-180.0..=180.0).contains(&p.lon) && (-90.0..=90.0).contains(&p.lat),
            "{} is at {:.3},{:.3}",
            o.entity.key,
            p.lon,
            p.lat
        );
        assert!(
            o.attrs.as_object().is_some_and(|a| a.len() > 1),
            "{} was emitted with no measurement; it should have been skipped",
            o.entity.key
        );
        // The stamp is UTC. Read as local on a BST machine, every station
        // would be an hour in the future, and this is what would catch it.
        let age = now - o.observed_at;
        assert!(
            age >= chrono::Duration::minutes(-5),
            "{} is stamped {} minutes in the future",
            o.entity.key,
            -age.num_minutes()
        );
        oldest = oldest.max(age);
        if age <= chrono::Duration::hours(3) {
            fresh += 1;
        }
        if o.attrs.get("name").is_some() {
            named += 1;
        }
        // 341 of the table's notes are HTML fragments. A card shows text.
        for key in ["name", "note", "station_type"] {
            if let Some(v) = o.attrs.get(key).and_then(|v| v.as_str()) {
                assert!(
                    !v.contains('<') || !v.contains('>'),
                    "{} has markup in its {key}: {v}",
                    o.entity.key
                );
            }
        }
    }

    // The file is the *latest* observation per station and the oldest seen
    // was two and a half hours old. If most of it is older than three hours
    // the file has stopped being rebuilt, and that is a stale layer
    // presented as live.
    assert!(
        fresh * 2 >= observations.len(),
        "only {fresh} of {} stations reported in the last three hours; oldest {} minutes",
        observations.len(),
        oldest.num_minutes()
    );

    // Every one of the 876 ids joined to the station table once case was
    // folded, and 148 table rows have no name. If the join breaks — the table
    // moves, its columns shift, the case rule changes — this halves.
    assert!(
        named * 4 >= observations.len() * 3,
        "only {named} of {} stations have a name; the station table join has broken",
        observations.len()
    );

    eprintln!(
        "ndbc: {} stations, {named} named, {fresh} within 3h, oldest {}m",
        observations.len(),
        oldest.num_minutes()
    );
}
