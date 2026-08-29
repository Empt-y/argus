//! End-to-end tests against a real PostgreSQL + PostGIS + TimescaleDB.
//!
//! These are the tests that matter for the DVR: the unit suite can only prove
//! the mappings are consistent, not that a batch of observations survives a
//! round trip through the hypertable, the rollup and back out as a historical
//! snapshot.
//!
//! Run with:
//!   ARGUS_TEST_DATABASE_URL=postgres://argus@localhost/argus_test cargo test -p argus-store
//!
//! Skipped (not failed) when that variable is unset, so `cargo test --workspace`
//! stays green on a machine with no database.

use argus_core::entity::{
    AltitudeDatum, EntityId, Kinematics, Observation, Position, Quality,
};
use argus_core::geo::BoundingBox;
use argus_core::source::SourceId;
use argus_store::{EntityFilter, Store};
use chrono::{Duration, Utc};

async fn store() -> Option<Store> {
    let url = std::env::var("ARGUS_TEST_DATABASE_URL").ok()?;
    let store = Store::connect(&url, 4).await.expect("connect to test database");
    store.migrate().await.expect("migrations apply");
    // Start from a known state; these tests assert on counts.
    sqlx::query("TRUNCATE observations, entities, sources CASCADE")
        .execute(store.pool())
        .await
        .expect("truncate");
    sqlx::query(
        "INSERT INTO sources (source_id, layer_id, display_name, entity_kind, cost_class)
         VALUES ('test-adsb', 'flights', 'Test ADS-B', 'aircraft', 'free')
         ON CONFLICT (source_id) DO NOTHING",
    )
    .execute(store.pool())
    .await
    .expect("seed source");
    Some(store)
}

fn observation(key: &str, lon: f64, lat: f64, secs_ago: i64) -> Observation {
    Observation::new(
        SourceId::new("test-adsb"),
        EntityId::aircraft(key),
        Utc::now() - Duration::seconds(secs_ago),
        Quality::Live,
    )
    .with_position(Position {
        lon,
        lat,
        alt_m: Some(10_000.0),
        // Deliberately barometric: the round trip must preserve the datum, not
        // quietly normalise it to something geometric.
        datum: AltitudeDatum::Barometric,
    })
    .with_kinematics(Kinematics {
        course_deg: Some(270.0),
        heading_deg: Some(268.0),
        ground_speed_mps: Some(230.0),
        vertical_rate_mps: Some(-2.5),
    })
    .with_label(format!("TEST{key}"))
}

macro_rules! require_db {
    () => {
        match store().await {
            Some(s) => s,
            None => {
                eprintln!("skipping: ARGUS_TEST_DATABASE_URL not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn observations_round_trip_through_the_hypertable() {
    let store = require_db!();
    let obs = vec![
        observation("abc123", -97.74, 30.27, 30),
        observation("def456", -97.70, 30.30, 20),
    ];
    let written = store.write_observations(&obs).await.expect("write");
    assert_eq!(written.inserted, 2);
    assert_eq!(written.deduped, 0);

    let found = store
        .entities_in_bbox(BoundingBox::new(-98.0, 30.0, -97.0, 31.0), &EntityFilter::default(), 100)
        .await
        .expect("query");
    assert_eq!(found.len(), 2);

    let one = found.iter().find(|r| r.entity_key == "abc123").expect("abc123");
    let pos = one.position().expect("position survived");
    assert!((pos.lon - -97.74).abs() < 1e-9);
    assert!((pos.lat - 30.27).abs() < 1e-9);
    // The datum must come back exactly as stored.
    assert_eq!(pos.datum, AltitudeDatum::Barometric);
    assert_eq!(one.quality(), Quality::Live);
    assert_eq!(one.label.as_deref(), Some("TESTabc123"));
    // The layer is resolved through the sources table, not copied from the id.
    assert_eq!(one.layer_id, "flights");
}

#[tokio::test]
async fn a_late_arriving_stale_fix_cannot_overwrite_a_newer_one() {
    // The bug this prevents: two feeds describe the same aircraft at different
    // lags, the slower one lands second, and the contact visibly jumps
    // backwards to where it was a minute ago.
    let store = require_db!();
    store
        .write_observations(&[observation("jump01", -97.70, 30.30, 10)])
        .await
        .expect("recent write");
    store
        .write_observations(&[observation("jump01", -97.90, 30.10, 600)])
        .await
        .expect("stale write");

    let found = store
        .entities_in_bbox(BoundingBox::new(-98.0, 30.0, -97.0, 31.0), &EntityFilter::default(), 100)
        .await
        .expect("query");
    let row = found.iter().find(|r| r.entity_key == "jump01").expect("found");
    let pos = row.position().expect("position");
    assert!(
        (pos.lon - -97.70).abs() < 1e-9,
        "stale fix overwrote the newer one: lon={}",
        pos.lon
    );
}

#[tokio::test]
async fn out_of_order_samples_within_one_batch_still_settle_on_the_newest() {
    let store = require_db!();
    store
        .write_observations(&[
            observation("batch1", -97.90, 30.10, 600),
            observation("batch1", -97.70, 30.30, 10),
            observation("batch1", -97.80, 30.20, 300),
        ])
        .await
        .expect("write");

    let found = store
        .entities_in_bbox(BoundingBox::new(-98.0, 30.0, -97.0, 31.0), &EntityFilter::default(), 100)
        .await
        .expect("query");
    let row = found.iter().find(|r| r.entity_key == "batch1").expect("found");
    assert!((row.position().unwrap().lon - -97.70).abs() < 1e-9);

    // All three samples are in the history even though one wins the live row.
    // History lives in the rollup, whose refresh runs on a timer, so
    // materialise it explicitly rather than waiting for the policy to fire.
    sqlx::query("CALL refresh_continuous_aggregate('tracks_1m', NULL, NULL)")
        .execute(store.pool())
        .await
        .expect("refresh rollup");
    let track = store
        .track(
            &EntityId::aircraft("batch1"),
            Utc::now() - Duration::hours(1),
            Utc::now(),
        )
        .await
        .expect("track");
    assert!(track.len() >= 2, "expected history, got {}", track.len());
}

#[tokio::test]
async fn implausible_positions_are_rejected_before_they_reach_the_store() {
    let store = require_db!();
    let mut null_island = observation("null01", 0.0, 0.0, 5);
    null_island.attrs = serde_json::Value::Null;
    let written = store
        .write_observations(&[null_island])
        .await
        .expect("write");
    // No position, no geometry, no attrs — nothing worth storing.
    assert_eq!(written.inserted, 0);
    assert_eq!(written.skipped, 1);
}

#[tokio::test]
async fn antimeridian_queries_return_both_sides() {
    let store = require_db!();
    store
        .write_observations(&[
            observation("fiji01", 179.5, -17.0, 10),
            observation("fiji02", -179.5, -17.0, 10),
            observation("other1", 0.0, -17.0, 10),
        ])
        .await
        .expect("write");

    let found = store
        .entities_in_bbox(BoundingBox::new(170.0, -20.0, -170.0, -10.0), &EntityFilter::default(), 100)
        .await
        .expect("query");
    let keys: Vec<&str> = found.iter().map(|r| r.entity_key.as_str()).collect();
    assert!(keys.contains(&"fiji01"), "missing east side: {keys:?}");
    assert!(keys.contains(&"fiji02"), "missing west side: {keys:?}");
    assert!(!keys.contains(&"other1"), "box leaked: {keys:?}");
}

#[tokio::test]
async fn the_dvr_answers_where_things_were_at_a_past_instant() {
    let store = require_db!();
    let then = Utc::now() - Duration::minutes(30);

    // Two positions for one aircraft, half an hour apart.
    let mut old = observation("dvr001", -97.90, 30.10, 0);
    old.observed_at = then;
    let mut recent = observation("dvr001", -97.60, 30.40, 0);
    recent.observed_at = Utc::now() - Duration::minutes(2);
    store
        .write_observations(&[old, recent])
        .await
        .expect("write");

    // The rollup is what the DVR reads, and its refresh policy runs on a timer,
    // so materialise the window explicitly rather than waiting a minute.
    sqlx::query("CALL refresh_continuous_aggregate('tracks_1m', NULL, NULL)")
        .execute(store.pool())
        .await
        .expect("refresh rollup");

    let bbox = BoundingBox::new(-98.0, 30.0, -97.0, 31.0);
    let past = store
        .entities_at(bbox, then + Duration::minutes(1), &EntityFilter::default(), 100)
        .await
        .expect("historical query");
    let row = past
        .iter()
        .find(|r| r.entity_key == "dvr001")
        .expect("present in the past snapshot");
    assert!(
        (row.position().unwrap().lon - -97.90).abs() < 1e-6,
        "DVR returned the wrong position for the past instant: {:?}",
        row.position()
    );

    // And the live view still shows the newer one.
    let now = store.entities_in_bbox(bbox, &EntityFilter::default(), 100).await.expect("live");
    let live = now.iter().find(|r| r.entity_key == "dvr001").expect("live row");
    assert!((live.position().unwrap().lon - -97.60).abs() < 1e-9);
}

#[tokio::test]
async fn a_stale_sample_is_not_dragged_forward_forever() {
    // An aircraft that landed hours ago must not keep appearing in every later
    // snapshot; the historical query floors how far back it will reach.
    let store = require_db!();
    let mut landed = observation("gone01", -97.75, 30.25, 0);
    landed.observed_at = Utc::now() - Duration::hours(3);
    store.write_observations(&[landed]).await.expect("write");
    sqlx::query("CALL refresh_continuous_aggregate('tracks_1m', NULL, NULL)")
        .execute(store.pool())
        .await
        .expect("refresh rollup");

    let found = store
        .entities_at(
            BoundingBox::new(-98.0, 30.0, -97.0, 31.0),
            Utc::now(),
            &EntityFilter::default(),
            100,
        )
        .await
        .expect("query");
    assert!(
        !found.iter().any(|r| r.entity_key == "gone01"),
        "a three-hour-old sample leaked into the present snapshot"
    );
}

#[tokio::test]
async fn re_polling_an_immutable_event_does_not_rewrite_it() {
    // Earthquakes keep the same observed_at forever, but the feed must still be
    // re-polled because USGS revises magnitudes for hours. Without the dedupe
    // index one 5-minute feed wrote ~74,000 identical rows a day.
    let store = require_db!();
    let quake = observation("quake1", -122.0, 38.0, 900);

    let first = store.write_observations(std::slice::from_ref(&quake)).await.expect("first");
    assert_eq!(first.inserted, 1);
    assert_eq!(first.deduped, 0);

    let second = store.write_observations(std::slice::from_ref(&quake)).await.expect("second");
    assert_eq!(second.inserted, 0, "duplicate was rewritten");
    assert_eq!(second.deduped, 1);
    // A fully deduped poll is a healthy poll, not a rejected one.
    assert_eq!(second.accepted(), 1);
    assert_eq!(second.skipped, 0);

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM observations WHERE entity_key = 'quake1'",
    )
    .fetch_one(store.pool())
    .await
    .expect("count");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn a_moving_entity_is_never_deduped_away() {
    // The dedupe key includes observed_at, which advances with every
    // transponder return — so an aircraft's track must survive intact.
    let store = require_db!();
    let obs: Vec<_> = (0..5)
        .map(|i| observation("mover", -97.0 - (i as f64) * 0.01, 30.0, 300 - i * 60))
        .collect();
    let written = store.write_observations(&obs).await.expect("write");
    assert_eq!(written.inserted, 5);
    assert_eq!(written.deduped, 0);
}

#[tokio::test]
async fn polygons_survive_the_round_trip_as_geometry() {
    // Regression: `Observation::geom` existed from the start but the schema had
    // nowhere to put it, so any observation whose shape was its whole meaning —
    // a weather alert area, a fire perimeter, a forecast cone — was accepted by
    // is_meaningful() and then written with the geometry silently discarded.
    let store = require_db!();

    let square = geo_types::Polygon::new(
        geo_types::LineString(vec![
            geo_types::Coord { x: -97.0, y: 30.0 },
            geo_types::Coord { x: -96.0, y: 30.0 },
            geo_types::Coord { x: -96.0, y: 31.0 },
            geo_types::Coord { x: -97.0, y: 31.0 },
            geo_types::Coord { x: -97.0, y: 30.0 },
        ]),
        vec![],
    );
    let obs = Observation::new(
        SourceId::new("test-adsb"),
        EntityId::new(argus_core::EntityKind::Event, "alert-1"),
        Utc::now() - Duration::seconds(30),
        Quality::Live,
    )
    .with_geom(geo_types::Geometry::Polygon(square))
    .with_position(Position::surface(-96.5, 30.5))
    .with_label("Severe Thunderstorm Warning");

    assert_eq!(store.write_observations(&[obs]).await.expect("write").inserted, 1);

    let found = store
        .entities_in_bbox(BoundingBox::new(-98.0, 29.0, -95.0, 32.0), &EntityFilter::default(), 100)
        .await
        .expect("query");
    let row = found
        .iter()
        .find(|r| r.entity_key == "alert-1")
        .expect("alert present");

    let geom = row.geom.as_ref().expect("geometry survived the round trip");
    assert_eq!(geom["type"], serde_json::json!("Polygon"));
    let ring = geom["coordinates"][0].as_array().expect("outer ring");
    assert_eq!(ring.len(), 5, "ring was not preserved intact");
    // And the label anchor is still there alongside it — the two are distinct.
    assert!(row.lon.is_some() && row.lat.is_some());
}

#[tokio::test]
async fn a_shape_is_found_by_a_box_that_misses_its_label_anchor() {
    // A viewport clipping the corner of a large alert area must still find it.
    // Matching only on the point would hide any polygon whose anchor happens to
    // sit outside the current view — which for a big warning is most views.
    let store = require_db!();
    let strip = geo_types::Polygon::new(
        geo_types::LineString(vec![
            geo_types::Coord { x: -100.0, y: 30.0 },
            geo_types::Coord { x: -90.0, y: 30.0 },
            geo_types::Coord { x: -90.0, y: 31.0 },
            geo_types::Coord { x: -100.0, y: 31.0 },
            geo_types::Coord { x: -100.0, y: 30.0 },
        ]),
        vec![],
    );
    let obs = Observation::new(
        SourceId::new("test-adsb"),
        EntityId::new(argus_core::EntityKind::Event, "wide-alert"),
        Utc::now() - Duration::seconds(30),
        Quality::Live,
    )
    .with_geom(geo_types::Geometry::Polygon(strip))
    // Anchor at the middle, far from the box we will query.
    .with_position(Position::surface(-95.0, 30.5));
    store.write_observations(&[obs]).await.expect("write");

    // A box over the western end only: contains the polygon, not the anchor.
    let found = store
        .entities_in_bbox(BoundingBox::new(-99.5, 30.2, -99.0, 30.8), &EntityFilter::default(), 100)
        .await
        .expect("query");
    assert!(
        found.iter().any(|r| r.entity_key == "wide-alert"),
        "polygon was missed because the query only matched its anchor point"
    );
}
