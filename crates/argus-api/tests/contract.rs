//! Contract tests for every endpoint, against a real database.
//!
//! Run with:
//!   ARGUS_TEST_DATABASE_URL=postgres://argus@localhost/argus_test cargo test -p argus-api
//!
//! Skipped (not failed) when that variable is unset, so `cargo test --workspace`
//! stays green on a machine with no database.
//!
//! These go through the assembled `Router` rather than calling handlers
//! directly. The auth layer, the path patterns and the status codes are as much
//! of the contract as the JSON is, and a test that bypasses the router proves
//! none of them.

use argus_api::{ApiConfig, ApiState, AuthMode};
use argus_core::entity::{EntityId, Kinematics, Observation, Position, Quality};
use argus_core::source::SourceId;
use argus_store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use serde_json::Value;
use tower::ServiceExt;

/// Serialises the tests in this binary.
///
/// Every test here truncates the shared database and seeds its own fixture, so
/// two running at once corrupt each other's expectations. That was previously
/// handled by documenting `--test-threads=1`, which works right up until
/// somebody runs a bare `cargo test` and gets six confusing failures that have
/// nothing to do with their change. A lock makes the requirement structural
/// instead of a thing to remember.
static DATABASE: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));


const LOOPBACK: std::net::SocketAddr = std::net::SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    50_000,
);
const REMOTE: std::net::SocketAddr = std::net::SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 50)),
    50_000,
);

async fn state(auth: AuthMode) -> Option<(ApiState, tokio::sync::MutexGuard<'static, ()>)> {
    // Held for the life of the test: the truncate below is destructive to any
    // other test using the same database.
    let guard = DATABASE.lock().await;
    let url = std::env::var("ARGUS_TEST_DATABASE_URL").ok()?;
    let store = Store::connect(&url, 4).await.expect("connect to test database");
    store.migrate().await.expect("migrations apply");
    sqlx::query("TRUNCATE observations, entities, sources, devices, geofences, alerts CASCADE")
        .execute(store.pool())
        .await
        .expect("truncate");
    sqlx::query(
        "INSERT INTO sources (source_id, layer_id, display_name, entity_kind,
                              cost_class, state, last_success, observations)
         VALUES ('test-adsb', 'flights', 'Test ADS-B', 'aircraft', 'free',
                 'live', now(), 3)",
    )
    .execute(store.pool())
    .await
    .expect("seed source");

    seed_aircraft(&store).await;

    Some((
        ApiState::new(
            store,
            ApiConfig {
                auth,
                public_url: "http://argus.test:8787".into(),
                // Configured, because a style with no ground under it is a
                // different document — the basemap has to be in the fixture for
                // the draw order and the offline style to mean anything.
                basemap: Some(argus_api::Basemap {
                    tiles_url: "https://tiles.test/{z}/{x}/{y}.png".into(),
                    attribution: Some("© Test".into()),
                    paint: argus_api::BasemapPaint::default(),
                }),
                basemaps: std::collections::BTreeMap::from([(
                    "night".to_string(),
                    argus_api::Basemap {
                        tiles_url: "https://tiles.test/night/{z}/{x}/{y}.png".into(),
                        attribution: Some("© Test".into()),
                        paint: argus_api::BasemapPaint {
                            brightness_max: Some(0.3),
                            saturation: Some(-0.7),
                            ..Default::default()
                        },
                    },
                )]),
                ..ApiConfig::default()
            },
        ),
        guard,
    ))
}

/// Three aircraft over the same patch of Texas, one of them a minute old.
async fn seed_aircraft(store: &Store) {
    let now = Utc::now();
    let observations: Vec<Observation> = [
        ("a1b2c3", -97.74, 30.27, 0),
        ("d4e5f6", -97.70, 30.30, 0),
        ("999999", -96.60, 30.20, 60),
    ]
    .iter()
    .map(|(key, lon, lat, age)| {
        Observation::new(
            SourceId::new("test-adsb"),
            EntityId::aircraft(key),
            now - Duration::seconds(*age),
            Quality::Live,
        )
        .with_position(Position {
            lon: *lon,
            lat: *lat,
            alt_m: Some(10_000.0),
            datum: argus_core::entity::AltitudeDatum::Barometric,
        })
        .with_kinematics(Kinematics {
            course_deg: Some(271.0),
            heading_deg: None,
            ground_speed_mps: Some(230.0),
            vertical_rate_mps: None,
        })
        .with_label(format!("FLT{key}"))
    })
    .collect();
    store
        .write_observations(&observations)
        .await
        .expect("seed observations");
}

async fn get(state: &ApiState, uri: &str, peer: std::net::SocketAddr) -> (StatusCode, Vec<u8>) {
    send(
        state,
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
        peer,
    )
    .await
}

async fn send(
    state: &ApiState,
    mut request: Request<Body>,
    peer: std::net::SocketAddr,
) -> (StatusCode, Vec<u8>) {
    // The router expects connect info, which a oneshot request has no socket to
    // supply; injecting it is how the loopback exemption gets exercised at all.
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = argus_api::router(state.clone())
        .oneshot(request)
        .await
        .expect("router responds");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body")
        .to_vec();
    (status, body)
}

async fn post_json(
    state: &ApiState,
    uri: &str,
    body: &Value,
    peer: std::net::SocketAddr,
) -> (StatusCode, Vec<u8>) {
    send(
        state,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap(),
        peer,
    )
    .await
}

/// Like [`get`], but keeps the response headers.
///
/// Cache-Control is part of this contract rather than a detail: it is what
/// stops a client caching a live style, and a test that only reads the body
/// cannot see it.
async fn get_full(
    state: &ApiState,
    uri: &str,
    peer: std::net::SocketAddr,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = argus_api::router(state.clone())
        .oneshot(request)
        .await
        .expect("router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body")
        .to_vec();
    (status, headers, body)
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("response should be JSON")
}

#[tokio::test]
async fn health_answers_without_a_token_from_anywhere() {
    let Some((state, _guard)) = state(AuthMode::Required).await else {
        return;
    };
    // Even under Required: telling a wrong address apart from a down server has
    // to be possible before a device is paired.
    let (status, body) = get(&state, "/v1/health", REMOTE).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["status"], "ok");
    assert_eq!(json(&body)["loopback_exempt"], false);
}

#[tokio::test]
async fn a_remote_caller_without_a_token_is_refused() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, body) = get(&state, "/v1/entities", REMOTE).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json(&body)["error"]["code"], "unauthorized");

    // ... and the same request from this machine is allowed through.
    let (status, _) = get(&state, "/v1/entities", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
}

/// A reverse proxy on loopback must not hand its callers the loopback
/// exemption.
///
/// Found by running it: with `tailscale serve` in front of the daemon,
/// `GET /v1/layers` answered 200 with no token from another machine on the
/// tailnet, because serve terminates on 127.0.0.1 and the peer address said
/// loopback. The exemption's justification — that anything reaching loopback
/// could already read the config file — stops being true the moment something
/// forwards other people's connections through it.
#[tokio::test]
async fn a_proxied_request_does_not_inherit_the_loopback_exemption() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    // Same socket, same policy: the only difference is the header a proxy adds.
    let (status, _) = get(&state, "/v1/layers", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK, "a local caller keeps the exemption");

    for header in ["x-forwarded-for", "x-forwarded-proto", "x-forwarded-host", "forwarded"] {
        let (status, _) = send(
            &state,
            Request::builder()
                .uri("/v1/layers")
                .header(header, "example")
                .body(Body::empty())
                .unwrap(),
            LOOPBACK,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a request carrying {header} did not originate on loopback"
        );
    }

    // And a real token still works through the proxy, or the fix would have
    // closed the hole by breaking the feature.
    let issued = state
        .store
        .create_device("phone", &["read".to_string()])
        .await
        .expect("device");
    let (status, _) = send(
        &state,
        Request::builder()
            .uri("/v1/layers")
            .header("x-forwarded-proto", "https")
            .header("authorization", format!("Bearer {}", issued.token))
            .body(Body::empty())
            .unwrap(),
        LOOPBACK,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn loopback_is_not_exempt_when_the_policy_says_required() {
    let Some((state, _guard)) = state(AuthMode::Required).await else {
        return;
    };
    let (status, _) = get(&state, "/v1/entities", LOOPBACK).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_paired_device_token_is_accepted_and_a_revoked_one_is_not() {
    let Some((state, _guard)) = state(AuthMode::Required).await else {
        return;
    };
    let code = state.pairing.issue();
    let request = Request::builder()
        .method("POST")
        .uri("/v1/pair")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "code": code, "name": "test phone" }).to_string(),
        ))
        .unwrap();
    let (status, body) = send(&state, request, REMOTE).await;
    assert_eq!(status, StatusCode::OK);
    let paired = json(&body);
    let token = paired["token"].as_str().expect("a token").to_string();
    assert_eq!(token.len(), 64);

    let with_token = |uri: &str| {
        Request::builder()
            .uri(uri.to_string())
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    let (status, _) = send(&state, with_token("/v1/entities"), REMOTE).await;
    assert_eq!(status, StatusCode::OK);

    // The same code must not pair a second device: a photograph of the console
    // is otherwise a permanent key.
    let replay = Request::builder()
        .method("POST")
        .uri("/v1/pair")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "code": code, "name": "attacker" }).to_string(),
        ))
        .unwrap();
    let (status, _) = send(&state, replay, REMOTE).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Revoke, and the token stops working immediately.
    let device_id = paired["device_id"].as_str().unwrap();
    let revoke = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/devices/{device_id}"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(&state, revoke, REMOTE).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(&state, with_token("/v1/entities"), REMOTE).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_token_in_the_query_string_works_for_clients_that_cannot_set_headers() {
    let Some((state, _guard)) = state(AuthMode::Required).await else {
        return;
    };
    let issued = state
        .store
        .create_device("maplibre", &["read".to_string()])
        .await
        .expect("device");
    let (status, _) = get(
        &state,
        &format!("/v1/tiles/flights/6/14/26?token={}", issued.token),
        REMOTE,
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "got {status}"
    );
}

#[tokio::test]
async fn a_read_only_device_cannot_mint_a_pairing_code() {
    let Some((state, _guard)) = state(AuthMode::Required).await else {
        return;
    };
    let issued = state
        .store
        .create_device("wall display", &["read".to_string()])
        .await
        .expect("device");
    let request = Request::builder()
        .method("POST")
        .uri("/v1/pair/code")
        .header("authorization", format!("Bearer {}", issued.token))
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(&state, request, REMOTE).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn entities_answers_a_viewport_and_reports_whether_it_truncated() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, body) = get(
        &state,
        "/v1/entities?bbox=-98,30,-97,31&layers=flights",
        LOOPBACK,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["live"], true);
    assert_eq!(body["count"], 2, "the third aircraft is outside the box");
    assert_eq!(body["truncated"], false);
    assert_eq!(body["entities"][0]["layer_id"], "flights");

    // A limit that bites must say so rather than quietly presenting a partial
    // view as an empty sky.
    let (_, body) = get(
        &state,
        "/v1/entities?bbox=-98,30,-97,31&layers=flights&limit=1",
        LOOPBACK,
    )
    .await;
    assert_eq!(json(&body)["truncated"], true);
}

/// "Live" has to mean live.
///
/// This is the bug the Android client found by being looked at: an aircraft
/// last seen three days ago was drawn on the map exactly like one in the sky,
/// because the live query had no freshness horizon at all — 1,213 of 1,528
/// contacts on a real map were more than a day old. Asking for the entity by
/// key still answers, because a client that names something specific is owed a
/// truthful answer about it rather than a 404.
#[tokio::test]
async fn a_contact_nobody_has_seen_for_days_is_not_in_the_live_view() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    seed_aircraft(&state.store).await;
    // Same box, same layer; the only thing separating this one is its age.
    let stale = Observation::new(
        SourceId::new("test-adsb"),
        EntityId::aircraft("stale1"),
        Utc::now() - Duration::days(3),
        Quality::Live,
    )
    .with_position(Position {
        lon: -97.72,
        lat: 30.28,
        alt_m: Some(0.0),
        datum: argus_core::entity::AltitudeDatum::Barometric,
    })
    .with_label("PARKED");
    state
        .store
        .write_observations(&[stale])
        .await
        .expect("a three-day-old fix is plausible, just not current");

    let (status, body) = get(
        &state,
        "/v1/entities?bbox=-98,30,-97,31&layers=flights",
        LOOPBACK,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = json(&body);
    let keys: Vec<&str> = listed["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["entity_key"].as_str().unwrap())
        .collect();
    assert!(keys.contains(&"a1b2c3"), "the fresh contacts are still there");
    assert!(
        !keys.contains(&"stale1"),
        "a three-day-old aircraft is not a current contact: {keys:?}"
    );

    // The layer rail counts under the same horizon, or it advertises 1,528
    // aircraft over a map drawing 212.
    let (_, layers) = get(&state, "/v1/layers", LOOPBACK).await;
    let flights = json(&layers)["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == "flights")
        .cloned()
        .expect("flights layer");
    assert_eq!(flights["live_entities"], 3);

    // Named directly, it still answers — with its real age, so the caller can
    // see for itself that this is a stale contact rather than a missing one.
    let (status, detail) = get(&state, "/v1/entities/aircraft/stale1", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&detail)["label"], "PARKED");
}

/// Kinds expire on their own schedule.
///
/// A flat horizon cannot be right for both: an earthquake from last Tuesday is
/// still a true statement about where the ground moved, and hiding it after
/// fifteen minutes would empty the hazard layers.
#[tokio::test]
async fn an_event_outlives_an_aircraft_by_a_long_way() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let day_old = Utc::now() - Duration::hours(30);
    let quake = Observation::new(
        SourceId::new("test-adsb"),
        EntityId::new(argus_core::EntityKind::Event, "quake1"),
        day_old,
        Quality::Live,
    )
    .with_position(Position {
        lon: -97.72,
        lat: 30.28,
        alt_m: None,
        datum: argus_core::entity::AltitudeDatum::Wgs84Ellipsoid,
    })
    .with_label("M4.2");
    let aircraft = Observation::new(
        SourceId::new("test-adsb"),
        EntityId::aircraft("olda1"),
        day_old,
        Quality::Live,
    )
    .with_position(Position {
        lon: -97.72,
        lat: 30.29,
        alt_m: Some(0.0),
        datum: argus_core::entity::AltitudeDatum::Barometric,
    });
    state
        .store
        .write_observations(&[quake, aircraft])
        .await
        .expect("seed");

    let (_, body) = get(&state, "/v1/entities?bbox=-98,30,-97,31", LOOPBACK).await;
    let listed = json(&body);
    let keys: Vec<&str> = listed["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["entity_key"].as_str().unwrap())
        .collect();
    assert!(keys.contains(&"quake1"), "a day-old quake is still news: {keys:?}");
    assert!(!keys.contains(&"olda1"), "a day-old aircraft is not: {keys:?}");
}

/// A geofence round-trips, and its rule is checked before it is stored.
///
/// The validation is the point. A fence whose rule will not parse is skipped by
/// the engine every thirty seconds for the rest of its life while looking
/// perfectly armed in the list — so the typo has to be a 400 at the moment
/// somebody makes it, not a silence discovered later.
#[tokio::test]
async fn a_geofence_is_created_validated_and_deleted() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let body = serde_json::json!({
        "name": "Heathrow final",
        "geometry": {
            "type": "Polygon",
            "coordinates": [[[-0.42, 51.44], [-0.28, 51.44], [-0.28, 51.50],
                             [-0.42, 51.50], [-0.42, 51.44]]]
        },
        "rule": { "trigger": "enters", "kinds": ["aircraft"], "max_alt_m": 1500 }
    });
    let (status, created) = post_json(&state, "/v1/geofences", &body, LOOPBACK).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&created));
    let created = json(&created);
    let id = created["geofence_id"].as_i64().expect("an id");
    assert_eq!(created["geometry"]["type"], "Polygon");
    assert_eq!(created["enabled"], true);

    let (_, listed) = get(&state, "/v1/geofences", LOOPBACK).await;
    assert_eq!(json(&listed)["geofences"][0]["name"], "Heathrow final");

    // A rule with a typo in a predicate name is refused rather than stored as
    // something that matches everything.
    let typo = serde_json::json!({
        "name": "typo",
        "geometry": created["geometry"],
        "rule": { "max_altitude_m": 1500 }
    });
    let (status, err) = post_json(&state, "/v1/geofences", &typo, LOOPBACK).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        String::from_utf8_lossy(&err).contains("max_altitude_m"),
        "the error should name the field: {}",
        String::from_utf8_lossy(&err)
    );

    // A self-intersecting ring is the dangerous one: ST_Contains against an
    // invalid polygon is undefined rather than merely false, so this fence
    // would be unpredictable rather than simply silent.
    let bowtie = serde_json::json!({
        "name": "bowtie",
        "geometry": {
            "type": "Polygon",
            "coordinates": [[[0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0], [0.0, 0.0]]]
        },
        "rule": {}
    });
    let (status, err) = post_json(&state, "/v1/geofences", &bowtie, LOOPBACK).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        String::from_utf8_lossy(&err).contains("Self-intersection"),
        "PostGIS names the exact vertex; pass that on: {}",
        String::from_utf8_lossy(&err)
    );

    // A degenerate ring is caught by the same validity check rather than by a
    // separate area test — see `validate_geofence_shape`.
    let degenerate = serde_json::json!({
        "name": "degenerate",
        "geometry": {
            "type": "Polygon",
            "coordinates": [[[0.0, 0.0], [0.0, 0.0], [0.0, 0.0], [0.0, 0.0]]]
        },
        "rule": {}
    });
    let (status, _) = post_json(&state, "/v1/geofences", &degenerate, LOOPBACK).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A line is not an area.
    let line = serde_json::json!({
        "name": "line",
        "geometry": { "type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0]] },
        "rule": {}
    });
    let (status, _) = post_json(&state, "/v1/geofences", &line, LOOPBACK).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // And a document PostGIS cannot parse at all, in its own words.
    let nonsense = serde_json::json!({
        "name": "nonsense",
        "geometry": { "type": "Rhombus", "coordinates": [] },
        "rule": {}
    });
    let (status, _) = post_json(&state, "/v1/geofences", &nonsense, LOOPBACK).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri(format!("/v1/geofences/{id}"))
            .body(Body::empty())
            .unwrap(),
        LOOPBACK,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, listed) = get(&state, "/v1/geofences", LOOPBACK).await;
    assert!(json(&listed)["geofences"].as_array().unwrap().is_empty());
}

/// Delivery is tracked per device, which is what makes replay possible.
///
/// The same query answers "what is new" and "what did I miss", so a phone that
/// spent ten minutes in a tunnel needs no special path — and a tablet that was
/// watching the whole time is not told twice.
#[tokio::test]
async fn an_alert_is_pending_for_each_device_until_that_device_sees_it() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let alert = state
        .store
        .insert_alert(&argus_store::NewAlert {
            geofence_id: None,
            entity: EntityId::aircraft("a1b2c3"),
            fired_at: Utc::now(),
            severity: "notice".into(),
            message: "FLTa1b2c3 entered Heathrow final".into(),
            lon: Some(-0.35),
            lat: Some(51.47),
            attrs: serde_json::json!({ "geofence": "Heathrow final" }),
        })
        .await
        .expect("insert alert");

    let phone = "device-phone";
    let tablet = "device-tablet";
    assert_eq!(state.store.alerts_undelivered_to(phone, 10).await.unwrap().len(), 1);
    assert_eq!(state.store.alerts_undelivered_to(tablet, 10).await.unwrap().len(), 1);

    state.store.mark_alert_delivered(alert.alert_id, phone).await.unwrap();
    assert!(state.store.alerts_undelivered_to(phone, 10).await.unwrap().is_empty());
    assert_eq!(
        state.store.alerts_undelivered_to(tablet, 10).await.unwrap().len(),
        1,
        "one device seeing an alert must not consume it for another"
    );

    // Marking twice does not grow the array without bound: a client that
    // reconnects mid-replay gets the same alert again and re-marks it.
    state.store.mark_alert_delivered(alert.alert_id, phone).await.unwrap();
    let rows = state.store.alerts(None, false, 10).await.unwrap();
    assert_eq!(rows[0].delivered_to.as_array().unwrap().len(), 1);

    // Acknowledging retires it for everyone, including devices that never saw
    // it — an alert somebody has dealt with should not surface on the tablet an
    // hour later as though it were news.
    let (status, _) = send(
        &state,
        Request::builder()
            .method("POST")
            .uri(format!("/v1/alerts/{}/ack", alert.alert_id))
            .body(Body::empty())
            .unwrap(),
        LOOPBACK,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(state.store.alerts_undelivered_to(tablet, 10).await.unwrap().is_empty());

    let (_, listed) = get(&state, "/v1/alerts?limit=10", LOOPBACK).await;
    assert_eq!(json(&listed)["alerts"][0]["message"], "FLTa1b2c3 entered Heathrow final");
}

/// A read-only device can watch but cannot arm.
#[tokio::test]
async fn a_read_only_device_cannot_create_a_geofence() {
    let Some((state, _guard)) = state(AuthMode::Required).await else {
        return;
    };
    let issued = state
        .store
        .create_device("wall display", &["read".to_string()])
        .await
        .expect("device");
    let body = serde_json::json!({
        "name": "nope",
        "geometry": {
            "type": "Polygon",
            "coordinates": [[[-0.42, 51.44], [-0.28, 51.44], [-0.28, 51.50],
                             [-0.42, 51.50], [-0.42, 51.44]]]
        }
    });
    let (status, _) = send(
        &state,
        Request::builder()
            .method("POST")
            .uri("/v1/geofences")
            .header("authorization", format!("Bearer {}", issued.token))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
        REMOTE,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_kind_filter_and_a_layer_filter_both_apply() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (_, body) = get(&state, "/v1/entities?kinds=aircraft", LOOPBACK).await;
    assert_eq!(json(&body)["count"], 3);
    let (_, body) = get(&state, "/v1/entities?kinds=vessel", LOOPBACK).await;
    assert_eq!(json(&body)["count"], 0);
    let (_, body) = get(&state, "/v1/entities?layers=nothing-here", LOOPBACK).await;
    assert_eq!(json(&body)["count"], 0);
}

#[tokio::test]
async fn a_malformed_viewport_is_a_client_error_naming_the_problem() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    for (uri, needle) in [
        ("/v1/entities?bbox=1,2,3", "four values"),
        ("/v1/entities?bbox=-2,52,0.5,51", "latitudes"),
        ("/v1/entities?kinds=submarine", "submarine"),
        ("/v1/entities?at=yesterday", "RFC 3339"),
    ] {
        let (status, body) = get(&state, uri, LOOPBACK).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        let message = json(&body)["error"]["message"].as_str().unwrap().to_string();
        assert!(message.contains(needle), "{uri} said: {message}");
    }
}

#[tokio::test]
async fn an_entity_detail_and_its_track_resolve_by_natural_key() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, body) = get(&state, "/v1/entities/aircraft/a1b2c3", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["entity_key"], "a1b2c3");
    assert_eq!(body["quality"], "live");
    assert!(body["attrs"].is_object());
    // The presented card rides alongside the raw row, never instead of it:
    // a client that only knows the row keeps working, and one that knows
    // the card gets words rather than `wind_speed_kt 16`.
    assert!(body["card"]["title"].is_string(), "{body:#}");
    assert!(body["card"]["sections"].is_array(), "{body:#}");

    let (status, body) = get(&state, "/v1/entities/aircraft/a1b2c3/track", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    // The rollup that backs a track is materialised on a timer, so a track
    // taken seconds after the write is legitimately empty. What is asserted
    // here is the contract, not the content.
    assert!(json(&body)["points"].is_array());

    let (status, _) = get(&state, "/v1/entities/aircraft/nope", LOOPBACK).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = get(&state, "/v1/entities/submarine/nope", LOOPBACK).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_backwards_track_window_is_refused() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, _) = get(
        &state,
        "/v1/entities/aircraft/a1b2c3/track\
         ?from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z",
        LOOPBACK,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sources_and_layers_report_health_honestly() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, body) = get(&state, "/v1/sources", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["sources"][0]["source_id"], "test-adsb");

    let (status, body) = get(&state, "/v1/layers", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    let layer = &json(&body)["layers"][0];
    assert_eq!(layer["id"], "flights");
    assert_eq!(layer["state"], "live");
    assert_eq!(layer["live_entities"], 3);
    // The style hints are what let a client draw a layer it has never heard of.
    assert_eq!(layer["style"]["geometry"], "point");
    assert_eq!(layer["style"]["rotates_with_course"], true);
    assert!(layer["style"]["color"].as_str().unwrap().starts_with('#'));
}

/// A failover chain is one feed, not several.
///
/// Found by looking at the Android sources sheet: "Aircraft (adsb.lol)"
/// appeared twice — once as the chain, once as the provider serving it — with
/// 1,398 observations against 317,763. Nothing recorded that one was inside the
/// other, so `/v1/layers` also summed both and reported a layer total larger
/// than the work actually done.
#[tokio::test]
async fn a_chain_and_its_providers_are_distinguishable_and_counted_once() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    // A chain over the flights layer, with the fixture's `test-adsb` as one of
    // its providers and a second that has never answered.
    sqlx::query(
        "INSERT INTO sources (source_id, layer_id, display_name, entity_kind,
                              cost_class, state, last_success, observations, member_of)
         VALUES ('flights', 'flights', 'Aircraft (chain)', 'aircraft', 'free',
                 'live', now(), 100, NULL),
                ('backup-adsb', 'flights', 'Aircraft (backup)', 'aircraft', 'free',
                 'unknown', NULL, 0, 'flights')",
    )
    .execute(state.store.pool())
    .await
    .expect("seed a chain");
    sqlx::query("UPDATE sources SET member_of = 'flights' WHERE source_id = 'test-adsb'")
        .execute(state.store.pool())
        .await
        .expect("make the fixture source a member");

    let (status, body) = get(&state, "/v1/sources", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    let listed = json(&body);
    let rows = listed["sources"].as_array().unwrap();

    // Ordered so a client can render the hierarchy by walking the list: the
    // chain first, then the providers inside it.
    assert_eq!(rows[0]["source_id"], "flights");
    assert!(rows[0]["member_of"].is_null(), "a chain belongs to nothing");
    for member in &rows[1..] {
        assert_eq!(
            member["member_of"], "flights",
            "every remaining row is inside the chain: {member}"
        );
    }

    // The layer total counts the chain's work once, not the chain plus each
    // member's contribution to it.
    let (_, body) = get(&state, "/v1/layers", LOOPBACK).await;
    let listed = json(&body);
    let layer = listed["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == "flights")
        .expect("flights layer");
    assert_eq!(
        layer["observations"], 100,
        "3 from the fixture member must not be added to the chain's 100"
    );
    // And the layer is named after the thing that represents it, not after
    // whichever member happens to rank best right now.
    assert_eq!(layer["display_name"], "Aircraft (chain)");
}

/// A source that is nobody's member still reports on its own.
#[tokio::test]
async fn a_standalone_source_is_not_treated_as_a_chain_member() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (_, body) = get(&state, "/v1/sources", LOOPBACK).await;
    let listed = json(&body);
    let row = &listed["sources"][0];
    assert_eq!(row["source_id"], "test-adsb");
    assert!(row["member_of"].is_null());

    let (_, body) = get(&state, "/v1/layers", LOOPBACK).await;
    let listed = json(&body);
    assert_eq!(listed["layers"][0]["observations"], 3);
}

#[tokio::test]
async fn a_tile_over_the_seeded_aircraft_contains_them() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let coord = tile_for(-97.74, 30.27, 8);
    let (status, body) = get(
        &state,
        &format!("/v1/tiles/flights/{}/{}/{}", coord.0, coord.1, coord.2),
        LOOPBACK,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    use geozero::mvt::Message;
    let tile = geozero::mvt::Tile::decode(body.as_slice()).expect("valid MVT");
    assert_eq!(tile.layers.len(), 1);
    assert_eq!(tile.layers[0].name, "flights");
    assert!(!tile.layers[0].features.is_empty());
    assert!(tile.layers[0].keys.iter().any(|k| k == "quality"));
}

#[tokio::test]
async fn an_empty_tile_is_a_204_rather_than_a_404() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    // Mid-Pacific at z=8: nothing seeded there. A 404 would make MapLibre
    // retry the same empty tile forever.
    let coord = tile_for(-150.0, 0.0, 8);
    let (status, _) = get(
        &state,
        &format!("/v1/tiles/flights/{}/{}/{}", coord.0, coord.1, coord.2),
        LOOPBACK,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn an_out_of_range_tile_is_a_client_error() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, _) = get(&state, "/v1/tiles/flights/1/9/0", LOOPBACK).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&state, "/v1/tiles/flights/8/1/not-a-row", LOOPBACK).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_style_document_points_at_the_public_url() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, body) = get(&state, "/v1/style.json", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    let style = json(&body);
    assert_eq!(style["version"], 8);
    let tiles = style["sources"]["flights"]["tiles"][0].as_str().unwrap();
    // A phone cannot use the bind address; the style must carry the address a
    // client would actually type.
    assert_eq!(
        tiles,
        "http://argus.test:8787/v1/tiles/flights/{z}/{x}/{y}.mvt"
    );
    let ids: Vec<&str> = style["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"flights-point"));
    // MapLibre draws in array order, so ground that arrives last is ground
    // painted over every contact on the map.
    assert_eq!(ids.first(), Some(&"basemap"));
}

/// A live tile expires, and soon.
///
/// This is load-bearing rather than cosmetic. MapLibre Native has no way to
/// invalidate a vector source in place — a tile is re-requested when its cached
/// copy expires and at no other time — so the freshness of the whole live map
/// is decided by this one header. Served as `no-store` it froze until the user
/// panned.
#[tokio::test]
async fn a_live_tile_expires_soon_enough_for_the_map_to_advance() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    seed_aircraft(&state.store).await;
    // Over the seeded fixture near Austin, so there is a body and therefore
    // headers — an empty tile returns 204 before it reaches them.
    let (z, x, y) = tile_for(-97.74, 30.27, 6);

    let (status, headers, _) =
        get_full(&state, &format!("/v1/tiles/flights/{z}/{x}/{y}.mvt"), LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(axum::http::header::CACHE_CONTROL).unwrap(),
        "public, max-age=15",
        "a live tile must expire, or the map never advances on its own"
    );
}

/// The style carries the DVR instant into the tile URLs it generates.
///
/// This is what makes the Android scrubber possible at all: MapLibre fixes a
/// vector source's tile template when the style loads, so the instant has to be
/// in the style or every source has to be torn down and rebuilt to move it.
#[tokio::test]
async fn a_style_asked_for_a_past_instant_bakes_it_into_every_tile_url() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    // Deliberately asked for in one form and expected back in another: the
    // instant is parsed and re-serialised canonically rather than pasted
    // through, so whatever a client sends becomes one `Z`-form string with no
    // `+` for a query parser to turn into a space.
    let at = "2026-08-30T12:00:00+00:00";
    let canonical = "2026-08-30T12:00:00.000Z";
    let (status, headers, body) =
        get_full(&state, &format!("/v1/style.json?at={at}"), LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    let style = json(&body);

    assert_eq!(
        style["sources"]["flights"]["tiles"][0].as_str().unwrap(),
        format!(
            "http://argus.test:8787/v1/tiles/flights/{{z}}/{{x}}/{{y}}.mvt?at={canonical}"
        ),
        "the instant must survive into the tile template, not just the style URL"
    );
    assert_eq!(style["metadata"]["argus:at"], canonical);
    // A fixed past instant is immutable, so MapLibre may cache it; live never
    // is, and a cached live style is a map of layers that have since changed.
    assert_eq!(
        headers.get(axum::http::header::CACHE_CONTROL).unwrap(),
        "public, max-age=3600"
    );

    let (_, live_headers, live_body) = get_full(&state, "/v1/style.json", LOOPBACK).await;
    assert!(json(&live_body)["metadata"]["argus:at"].is_null());
    assert!(
        !json(&live_body)["sources"]["flights"]["tiles"][0]
            .as_str()
            .unwrap()
            .contains("at="),
    );
    assert_eq!(
        live_headers.get(axum::http::header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
}

/// A named basemap is selectable, and an unknown one falls back rather than
/// failing.
///
/// The fallback matters because the choice is persisted on the client: a phone
/// holding a preference for a basemap the operator has since renamed should get
/// a map, not an error page where the ground used to be.
#[tokio::test]
async fn a_basemap_can_be_chosen_by_name_and_an_unknown_one_falls_back() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, body) = get(&state, "/v1/style.json?basemap=night", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    let style = json(&body);
    assert_eq!(
        style["sources"]["basemap"]["tiles"][0],
        "https://tiles.test/night/{z}/{x}/{y}.png"
    );
    // Paint tuning is how a dark theme is built from light tiles, so it has to
    // survive into the style rather than being a client-side convention.
    let paint = &style["layers"][0]["paint"];
    assert_eq!(paint["raster-brightness-max"], 0.3);
    assert_eq!(paint["raster-saturation"], -0.7);

    // The names on offer travel with the document, so a client needs no second
    // endpoint and no hard-coded list.
    let offered: Vec<&str> = style["metadata"]["argus:basemaps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(offered, ["night"]);

    let (status, body) = get(&state, "/v1/style.json?basemap=gone", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK, "an unknown name is not an error");
    assert_eq!(
        json(&body)["sources"]["basemap"]["tiles"][0],
        "https://tiles.test/{z}/{x}/{y}.png",
        "it falls back to the configured default"
    );
}

/// Ground without contacts — the style an offline region is cut from.
///
/// MapLibre's offline manager downloads every tile a style references. Handed
/// the full style it would package the live layers too, and a phone with no
/// signal would then show an hour-old sky as though it were now. The basemap is
/// the part worth having offline because it is the part that does not change.
#[tokio::test]
async fn a_basemap_only_style_carries_no_argus_layers() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, body) = get(&state, "/v1/style.json?basemap_only=true", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    let style = json(&body);
    assert!(
        style["sources"]["flights"].is_null(),
        "a live layer must not be downloadable as though it were ground"
    );
    let ids: Vec<&str> = style["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["basemap"], "ground only");

    // And the default is unchanged: everything, as before.
    let (_, full) = get(&state, "/v1/style.json", LOOPBACK).await;
    assert!(json(&full)["sources"]["flights"].is_object());
}

/// A style asked for nonsense fails once, here, rather than loading and then
/// failing on every tile it goes on to request.
#[tokio::test]
async fn a_style_with_an_unparseable_instant_is_refused() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, body) = get(&state, "/v1/style.json?at=yesterday", LOOPBACK).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        String::from_utf8_lossy(&body).contains("yesterday"),
        "the error should name what it could not parse"
    );
}

/// The headline claim of this phase: a client time-travels by adding one query
/// parameter, and the tiler honours it, so the phone gets the DVR for free.
///
/// Proved against a real past instant rather than an empty one. The rollup that
/// backs the DVR is materialised on a timer, so this refreshes it explicitly
/// rather than waiting a minute for the policy to fire.
#[tokio::test]
async fn the_dvr_serves_a_past_instant_through_both_entities_and_tiles() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };

    // An aircraft that was over Austin two hours ago and is not there now.
    let then = Utc::now() - Duration::hours(2);
    let historic = Observation::new(
        SourceId::new("test-adsb"),
        EntityId::aircraft("h15tor"),
        then,
        Quality::Live,
    )
    .with_position(Position {
        lon: -97.74,
        lat: 30.27,
        alt_m: Some(9_000.0),
        datum: argus_core::entity::AltitudeDatum::Barometric,
    })
    .with_label("PAST01");
    state
        .store
        .write_observations(&[historic])
        .await
        .expect("seed history");
    sqlx::query("CALL refresh_continuous_aggregate('tracks_1m', NULL, NULL)")
        .execute(state.store.pool())
        .await
        .expect("materialise the rollup");

    let at = then.to_rfc3339();
    let (status, body) = get(
        &state,
        &format!("/v1/entities?bbox=-98,30,-97,31&at={at}"),
        LOOPBACK,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["live"], false);
    assert!(body["at"].as_str().unwrap().starts_with(&at[..13]));
    let keys: Vec<&str> = body["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["entity_key"].as_str().unwrap())
        .collect();
    assert!(
        keys.contains(&"h15tor"),
        "the DVR should return where it actually was, got {keys:?}"
    );

    // The same instant, through the tiler.
    let coord = tile_for(-97.74, 30.27, 8);
    let (status, body) = get(
        &state,
        &format!(
            "/v1/tiles/flights/{}/{}/{}?at={at}",
            coord.0, coord.1, coord.2
        ),
        LOOPBACK,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    use geozero::mvt::Message;
    let tile = geozero::mvt::Tile::decode(body.as_slice()).expect("valid MVT");
    assert_eq!(tile.layers[0].name, "flights");
    assert!(!tile.layers[0].features.is_empty());
}

/// Web-mercator tile containing a point. The inverse of `TileCoord::bounds`,
/// written out here so the test does not lean on the code it is checking.
fn tile_for(lon: f64, lat: f64, z: u8) -> (u8, u32, u32) {
    let n = f64::from(1u32 << z);
    let x = ((lon + 180.0) / 360.0 * n).floor() as u32;
    let lat_rad = lat.to_radians();
    let y = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
        .floor() as u32;
    (z, x, y)
}

/// The imagery overlays ride in the style hidden, between the ground and
/// the contacts, and stand alone at `/v1/overlays` dated by the DVR.
#[tokio::test]
async fn overlays_are_in_the_style_hidden_and_dated_like_the_contacts() {
    let Some((state, _guard)) = state(AuthMode::LoopbackExempt).await else {
        return;
    };
    let (status, body) = get(&state, "/v1/overlays", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    let overlays = body["overlays"].as_array().unwrap();
    assert!(overlays.iter().any(|o| o["id"] == "night-lights"), "{body:#}");
    let yesterday = (chrono::Utc::now() - chrono::Duration::days(1)).date_naive().to_string();
    assert_eq!(body["date"], yesterday);

    let (status, body) = get(&state, "/v1/overlays?at=2026-09-08T14:00:00Z", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["date"], "2026-09-08");
    assert!(body["overlays"][0]["tiles"].as_str().unwrap().contains("/2026-09-08/"));

    let (status, body) = get(&state, "/v1/style.json?at=2026-09-08T14:00:00Z", LOOPBACK).await;
    assert_eq!(status, StatusCode::OK);
    let style = json(&body);
    let layers = style["layers"].as_array().unwrap();
    let ids: Vec<&str> = layers.iter().map(|l| l["id"].as_str().unwrap()).collect();
    let night = ids.iter().position(|id| *id == "overlay:night-lights").expect("the overlay layer");
    let basemap = ids.iter().position(|id| *id == "basemap");
    let first_contact = ids.iter().position(|id| id.starts_with("flights")).expect("a contact layer");
    if let Some(b) = basemap {
        assert!(b < night, "ground first");
    }
    assert!(night < first_contact, "overlays under the contacts");
    assert_eq!(layers[night]["layout"]["visibility"], "none", "hidden until asked for");
    assert!(style["sources"]["overlay:night-lights"]["tiles"][0].as_str().unwrap().contains("/2026-09-08/"));
    assert!(style["metadata"]["argus:overlays"].as_array().unwrap().iter().any(|o| o["layer"] == "overlay:night-lights"));

    // The ground-only style an offline region is cut from carries none.
    let (_, body) = get(&state, "/v1/style.json?basemap_only=true", LOOPBACK).await;
    assert!(json(&body)["sources"].get("overlay:night-lights").is_none());
}
