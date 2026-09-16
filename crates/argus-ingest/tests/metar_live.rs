//! Poll the Aviation Weather Center's bulk METAR cache for real and assert
//! on what comes back.
//!
//! The unit tests hold the decoder to a captured header and seven captured
//! rows. What they cannot see is the file moving underneath them: a column
//! added (which the header check turns into a loud error), the TAF or station
//! files moving (which turns into a layer with no forecasts and no names, and
//! only a count notices), or the whole thing quietly becoming the thinned
//! query-API answer — 62 stations for the UK instead of 110.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::EntityKind;
use argus_core::source::{Source, SourceError};

/// 5,128 aerodromes were reporting when this was written, 110 of them in the
/// UK box. Half is well under the day-to-day swing and well over what the
/// thinned API, a truncated file or a shifted column leaves behind.
const STATIONS_SEEN: usize = 5_128;
const UK_STATIONS_SEEN: usize = 110;

/// 2,699 of the 2,971 TAF aerodromes also had a METAR row.
const TAFS_SEEN: usize = 2_699;

fn skip_or_panic(what: &str, err: SourceError) {
    match err {
        SourceError::Transport(_) | SourceError::RateLimited { .. } | SourceError::Forbidden(_) => {
            eprintln!("SKIPPING {what}: unavailable ({err})");
        }
        other => panic!("{what} failed: {other}"),
    }
}

#[tokio::test]
async fn the_network_arrives_whole_with_forecasts_and_names() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feed");
        return;
    }
    // The daemon's shared client allows 30 seconds. All three files arrived
    // in under a second each, so this tests the patience the driver gets in
    // production rather than a longer one that would pass where it fails.
    let http =
        argus_ingest::HttpClient::new(std::time::Duration::from_secs(30)).expect("an http client");
    let source = argus_ingest::sources::Metars::new(http);

    let observations = match source.poll(&argus_core::PollCtx::default()).await {
        Ok(o) => o,
        Err(err) => return skip_or_panic("awc-metar", err),
    };

    let floor = STATIONS_SEEN / 2;
    assert!(
        observations.len() >= floor,
        "{} aerodromes reporting, expected at least {floor} — a short answer is what the \
         thinned query API, a truncated file or a shifted column looks like",
        observations.len()
    );

    let now = chrono::Utc::now();
    let mut uk = 0usize;
    let mut named = 0usize;
    let mut with_taf = 0usize;
    let mut fresh = 0usize;
    let mut oldest = chrono::Duration::zero();
    for o in &observations {
        assert_eq!(o.entity.kind, EntityKind::Station);
        let p = o
            .position
            .expect("a station without a position was emitted");
        assert!(
            (-180.0..=180.0).contains(&p.lon) && (-90.0..=90.0).contains(&p.lat),
            "{} is at {:.3},{:.3}",
            o.entity.key,
            p.lon,
            p.lat
        );
        // The unplaced sentinel must never reach the store. It is inside the
        // valid range, so the range check above would not catch it.
        assert!(
            !(p.lat == -99.99 || (p.lat == 0.0 && p.lon == 0.0)),
            "{} is at the unplaced sentinel or on Null Island",
            o.entity.key
        );
        if (49.0..=61.0).contains(&p.lat) && (-11.0..=2.0).contains(&p.lon) {
            uk += 1;
        }
        let attrs = o.attrs.as_object().expect("attrs are an object");
        assert!(
            attrs
                .get("raw")
                .and_then(|v| v.as_str())
                .is_some_and(|r| r.len() > 10),
            "{} has no raw report",
            o.entity.key
        );
        // The literal string `null` reached 368 rows of the live file. It
        // must not reach the store as a category.
        assert_ne!(
            attrs.get("flight_category").and_then(|v| v.as_str()),
            Some("null"),
            "{} has the word null as a flight category",
            o.entity.key
        );
        if let Some(v) = attrs.get("visibility_mi") {
            assert!(
                v.is_number(),
                "{} visibility is {v}, not a number",
                o.entity.key
            );
        }
        for key in [
            "maxT_c",
            "minT_c",
            "maxT24hr_c",
            "minT24hr_c",
            "vert_vis_ft",
        ] {
            assert!(
                attrs.get(key).is_none(),
                "{} carries the broken column {key}",
                o.entity.key
            );
        }
        // The stamp is UTC. Read as local on a BST machine, every aerodrome
        // would be an hour in the future.
        let age = now - o.observed_at;
        assert!(
            age >= chrono::Duration::minutes(-5),
            "{} is stamped {} minutes in the future",
            o.entity.key,
            -age.num_minutes()
        );
        oldest = oldest.max(age);
        if age <= chrono::Duration::hours(2) {
            fresh += 1;
        }
        if attrs.get("name").is_some() {
            named += 1;
        }
        if let Some(taf) = attrs.get("taf") {
            with_taf += 1;
            let taf = taf.as_str().expect("a TAF is text");
            assert!(
                taf.starts_with("TAF"),
                "{}'s forecast does not read like one: {taf}",
                o.entity.key
            );
            let valid_to = attrs
                .get("taf_valid_to")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok())
                .expect("an attached TAF carries its validity");
            assert!(
                valid_to >= now,
                "{} has an expired TAF attached",
                o.entity.key
            );
        }
    }

    // The bulk cache is the *latest* report per aerodrome and the oldest in
    // the captured file was 90 minutes old. If most of it is older than two
    // hours the file has stopped being rebuilt.
    assert!(
        fresh * 2 >= observations.len(),
        "only {fresh} of {} aerodromes reported in the last two hours; oldest {} minutes",
        observations.len(),
        oldest.num_minutes()
    );

    // The query API thins the UK box to 62. The cache has 110. If this drops
    // to the thinned number the layer has stopped being the network.
    assert!(
        uk >= UK_STATIONS_SEEN * 2 / 3,
        "only {uk} aerodromes in the UK box, expected around {UK_STATIONS_SEEN}"
    );

    // Every reporting aerodrome joined to the station table. If the table
    // moves or its `icaoId` key is renamed, this collapses to zero.
    assert!(
        named * 4 >= observations.len() * 3,
        "only {named} of {} aerodromes have a name; the station table join has broken",
        observations.len()
    );

    // Half the aerodromes carry a forecast. If the TAF file moves, or its
    // schema changes so that no `<TAF>` decodes, this collapses to zero
    // without any other symptom.
    assert!(
        with_taf >= TAFS_SEEN / 2,
        "only {with_taf} aerodromes have a TAF attached, expected around {TAFS_SEEN}"
    );

    eprintln!(
        "metar: {} aerodromes, {uk} in the UK, {named} named, {with_taf} with a TAF, {fresh} within 2h, oldest {}m",
        observations.len(),
        oldest.num_minutes()
    );
}
