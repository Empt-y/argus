//! Poll all nine storm overflow feeds for real and assert on what comes back.
//!
//! The unit tests prove the decoder handles the shapes that were in the feeds
//! on the day it was written. They cannot prove that paging actually collects
//! every outfall, because the thing that breaks paging — a server that ignores
//! `resultOffset`, or a page cap lowered from 2,000 to 500 — produces a
//! perfectly valid, perfectly decodable, silently *short* answer. A truncated
//! feed looks exactly like a county with no sewers.
//!
//! So the assertion here is a record count, never a status code. That is the
//! discipline the source research settled on after three separate services
//! returned HTTP 200 with unusable bodies, and it is the only check that
//! catches a feed that has quietly become half a feed.
//!
//! Network-gated: set ARGUS_NETWORK_TESTS=1 to run.

use argus_core::entity::{EntityKind, Quality};
use argus_core::source::{Source, SourceError};

/// The count each company served when this was written, on 2026-09-08.
///
/// The floor is set well below it — two thirds — because these are real asset
/// registers that gain and lose outfalls as monitors are installed and
/// decommissioned, and a test that fails when Yorkshire commissions forty new
/// EDMs is a test that gets deleted. Two thirds still catches every failure
/// this test exists for: a lost page, a lowered cap, a region silently dropped.
const EXPECTED: &[(&str, usize)] = &[
    ("storm-overflows-angl", 1439),
    ("storm-overflows-nwl", 1575),
    ("storm-overflows-swsc", 2073),
    ("storm-overflows-stw", 2412),
    ("storm-overflows-sww", 1344),
    ("storm-overflows-tw", 573),
    ("storm-overflows-uu", 2252),
    ("storm-overflows-wsx", 1427),
    ("storm-overflows-yw", 2179),
];

#[tokio::test]
async fn every_company_serves_a_whole_feed_and_it_decodes() {
    if std::env::var("ARGUS_NETWORK_TESTS").is_err() {
        eprintln!("SKIPPING: set ARGUS_NETWORK_TESTS=1 to poll the live feeds");
        return;
    }

    let http = argus_ingest::HttpClient::new(std::time::Duration::from_secs(90))
        .expect("an http client");
    let ctx = argus_core::PollCtx::default();
    let mut unavailable = Vec::new();
    let mut national_stations = 0usize;
    let mut national_discharging = 0usize;
    let mut national_stale = 0usize;

    for source in argus_ingest::sources::StormOverflows::all(http.clone()) {
        let id = source.descriptor().id.to_string();
        let (_, floor) = EXPECTED
            .iter()
            .find(|(name, _)| *name == id)
            .unwrap_or_else(|| panic!("{id} is not in the expected table"));
        let floor = floor * 2 / 3;

        let observations = match source.poll(&ctx).await {
            Ok(observations) => observations,
            // An outage at one water company says nothing about this decoder.
            // A decode failure is a different matter and is left to fail: it
            // means the schema moved, which breaks the real driver too.
            Err(
                err @ (SourceError::Transport(_)
                | SourceError::RateLimited { .. }
                | SourceError::Forbidden(_)),
            ) => {
                eprintln!("SKIPPING {id}: unavailable ({err})");
                unavailable.push(id);
                continue;
            }
            Err(err) => panic!("{id} failed to decode: {err}"),
        };

        let stations: Vec<_> = observations
            .iter()
            .filter(|o| o.entity.kind == EntityKind::Station)
            .collect();
        let events: Vec<_> = observations
            .iter()
            .filter(|o| o.entity.kind == EntityKind::Event)
            .collect();
        let discharging = stations
            .iter()
            .filter(|o| o.attrs["state"] == serde_json::json!("discharging"))
            .count();
        let stale = stations.iter().filter(|o| o.quality == Quality::Stale).count();

        assert!(
            stations.len() >= floor,
            "{id} served {} outfalls, expected at least {floor} — a short feed is \
             what a lost page looks like",
            stations.len()
        );

        // Every outfall must be placeable and in the British Isles. A
        // reprojection failure — Scottish Water's layer is natively EPSG:27700
        // — would put every one of its assets in the Gulf of Guinea, which is a
        // decodable, plausible-looking, completely wrong answer.
        for o in &stations {
            let p = o.position.expect("an outfall without a position was emitted");
            assert!(
                (-8.7..=2.0).contains(&p.lon) && (49.8..=61.0).contains(&p.lat),
                "{id} placed {} at {:.4},{:.4}, which is not in the British Isles",
                o.entity.key,
                p.lon,
                p.lat
            );
        }

        // Discharge events are a strict subset of the outfalls: one per outfall
        // at most, and only where the outfall itself was reported.
        assert!(events.len() <= stations.len());
        assert!(
            events.len() >= discharging,
            "{id} reported {discharging} outfalls discharging but only {} events",
            events.len()
        );

        national_stations += stations.len();
        national_discharging += discharging;
        national_stale += stale;
        eprintln!(
            "{id}: {} outfalls, {discharging} discharging, {stale} stale, {} spills in the last 48h",
            stations.len(),
            events.len()
        );
    }

    assert!(
        unavailable.len() < EXPECTED.len(),
        "every company was unavailable; this says nothing about the decoder"
    );
    // Every outfall is dated by the poll, so none of them can fall out of the
    // station horizon — that is the whole reason the company's own stamp is not
    // used as the clock. What the stamp does drive is the stale mark, and if
    // that ever swallowed most of the country it would mean the rule had
    // inverted rather than that the sewers had. Around one in eight is normal:
    // Northumbrian's per-record stamps plus the nationally offline monitors.
    assert!(
        national_stale * 5 < national_stations * 2,
        "{national_stale} of {national_stations} outfalls marked stale; the rule has inverted"
    );
    eprintln!(
        "\n{national_stations} outfalls across {} companies, {national_discharging} discharging now, \
         {national_stale} stale",
        EXPECTED.len() - unavailable.len()
    );
}
