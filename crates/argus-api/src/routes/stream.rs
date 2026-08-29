//! `WS /v1/stream` — live deltas for a viewport.
//!
//! The client subscribes to a box and a set of layers; the server sends a
//! snapshot and then only what changed. That is the difference between a map
//! that repolls ten thousand aircraft every two seconds and one that sends the
//! forty that moved.
//!
//! Deltas are polled from `entities.updated_at` rather than pushed from the
//! write path. A `LISTEN/NOTIFY` fan-out would be lower latency, but it would
//! put a per-row notify on the hottest write in the system to save a second of
//! latency on a map that is already showing data seconds old. The index added
//! in 0005 is what makes the poll cheap.

use crate::params::{parse_bbox, parse_instant};
use crate::ApiState;
use argus_core::geo::BoundingBox;
use argus_store::EntityFilter;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// How often the server looks for changes. Two seconds is below the cadence of
/// every feed Argus has, so nothing is ever missed by waiting; polling faster
/// would only find the same rows again.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// How far back each poll reaches beyond the last row it saw.
///
/// A transaction that commits after a later one can carry an earlier
/// `updated_at`, so a cursor set to the newest row seen can step over rows that
/// were still uncommitted. Re-reading a few seconds closes that window.
///
/// Re-reading is not the same as re-sending. The overlap alone was tried and is
/// wrong: pinning the cursor at `newest - overlap` means that once a feed goes
/// quiet, nothing ever pushes the cursor past the last batch, and the same rows
/// go out on every tick forever — 119 aircraft every two seconds for a source
/// that polls every fifteen. So the window is re-queried and what has already
/// been sent is filtered out by `(entity, updated_at)`, which is the only thing
/// that distinguishes a genuinely new update from the same one seen twice.
const CURSOR_OVERLAP: Duration = Duration::seconds(3);

/// Cap on rows in one frame, so a client that subscribes to the planet gets a
/// large first frame rather than an unbounded one.
const MAX_ROWS: i64 = 5_000;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    /// Replace the current subscription. Sending a second one is how a client
    /// pans: there is no unsubscribe, because there is only ever one viewport.
    Subscribe {
        bbox: Option<String>,
        layers: Option<String>,
        kinds: Option<String>,
        /// A DVR instant for the opening snapshot. The stream that follows is
        /// always live — a subscription to the past would have nothing to say.
        at: Option<String>,
    },
    Ping,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame {
    /// Everything in the box right now (or at `at`), sent once per subscribe.
    Snapshot {
        at: DateTime<Utc>,
        count: usize,
        entities: Vec<argus_store::EntityRow>,
    },
    /// What has changed since the last frame.
    Delta {
        at: DateTime<Utc>,
        count: usize,
        entities: Vec<argus_store::EntityRow>,
    },
    Pong,
    Error {
        message: String,
    },
}

pub async fn stream(ws: WebSocketUpgrade, State(state): State<ApiState>) -> Response {
    ws.on_upgrade(move |socket| run(socket, state))
}

struct Subscription {
    bbox: BoundingBox,
    filter: EntityFilter,
    /// Floor for the next query. Sits `CURSOR_OVERLAP` behind the newest row
    /// seen, so a late-committing row is still caught.
    cursor: DateTime<Utc>,
    /// The `updated_at` last sent for each entity, for everything inside the
    /// overlap window. Bounded by what one poll can return, and pruned to the
    /// cursor on every tick — an entry older than the query floor can never
    /// come back, so keeping it would be pure leak.
    sent: HashMap<(String, String), DateTime<Utc>>,
}

impl Subscription {
    /// Rows that are genuinely new to this client, in the order they happened.
    fn undelivered(&mut self, rows: Vec<argus_store::DeltaRow>) -> Vec<argus_store::EntityRow> {
        let mut fresh = Vec::with_capacity(rows.len());
        let mut newest = None::<DateTime<Utc>>;
        for row in rows {
            let key = (
                row.entity.entity_kind.clone(),
                row.entity.entity_key.clone(),
            );
            newest = Some(newest.map_or(row.updated_at, |n: DateTime<Utc>| n.max(row.updated_at)));
            if self.sent.get(&key) == Some(&row.updated_at) {
                continue;
            }
            self.sent.insert(key, row.updated_at);
            fresh.push(row.entity);
        }
        if let Some(newest) = newest {
            self.cursor = newest - CURSOR_OVERLAP;
        }
        let floor = self.cursor;
        self.sent.retain(|_, at| *at >= floor);
        fresh
    }
}

async fn run(mut socket: WebSocket, state: ApiState) {
    let mut subscription: Option<Subscription> = None;
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else {
                    // None is a closed socket; Err is a broken one. Both mean
                    // the client is gone and the task should end rather than
                    // spin.
                    break;
                };
                match message {
                    Message::Text(text) => {
                        match handle_text(&state, &text).await {
                            Ok((sub, frame)) => {
                                subscription = Some(sub);
                                if !send(&mut socket, &frame).await {
                                    break;
                                }
                            }
                            Err(frame) => {
                                if !send(&mut socket, &frame).await {
                                    break;
                                }
                            }
                        }
                    }
                    Message::Close(_) => break,
                    // Pings are answered by axum's own machinery; binary frames
                    // have no meaning in this protocol and are ignored rather
                    // than treated as an error, so a client library that sends
                    // keepalives its own way does not get disconnected.
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                let Some(sub) = subscription.as_mut() else { continue };
                match state
                    .store
                    .entities_changed_since(sub.bbox, &sub.filter, sub.cursor, MAX_ROWS)
                    .await
                {
                    Ok(rows) if rows.is_empty() => {}
                    Ok(rows) => {
                        let entities = sub.undelivered(rows);
                        // Every row was one this client already has. Normal
                        // between polls of a 15-second feed, and not something
                        // to spend a frame on.
                        if entities.is_empty() {
                            continue;
                        }
                        let frame = ServerFrame::Delta {
                            at: Utc::now(),
                            count: entities.len(),
                            entities,
                        };
                        if !send(&mut socket, &frame).await {
                            break;
                        }
                    }
                    Err(err) => {
                        tracing::warn!("delta query failed: {err}");
                        let frame = ServerFrame::Error {
                            message: "delta query failed".into(),
                        };
                        if !send(&mut socket, &frame).await {
                            break;
                        }
                    }
                }
            }
        }
    }
    tracing::debug!("stream client disconnected");
}

/// Returns the new subscription and its opening snapshot, or the error frame to
/// send instead. A bad subscribe leaves any existing subscription intact —
/// a typo in a pan should not silently blank the client's map.
async fn handle_text(
    state: &ApiState,
    text: &str,
) -> Result<(Subscription, ServerFrame), ServerFrame> {
    let frame: ClientFrame = serde_json::from_str(text).map_err(|err| ServerFrame::Error {
        message: format!("could not parse frame: {err}"),
    })?;

    let ClientFrame::Subscribe {
        bbox,
        layers,
        kinds,
        at,
    } = frame
    else {
        return Err(ServerFrame::Pong);
    };

    let query = crate::params::ViewportQuery {
        bbox,
        layers,
        kinds,
        at: None,
        limit: None,
    };
    let bbox = match query.bbox.as_deref() {
        Some(text) => parse_bbox(text).map_err(|err| ServerFrame::Error {
            message: err.to_string(),
        })?,
        None => BoundingBox::GLOBAL,
    };
    let filter = query.filter().map_err(|err| ServerFrame::Error {
        message: err.to_string(),
    })?;
    let at = at
        .as_deref()
        .map(parse_instant)
        .transpose()
        .map_err(|err| ServerFrame::Error {
            message: err.to_string(),
        })?;

    let entities = match at {
        Some(at) => state.store.entities_at(bbox, at, &filter, MAX_ROWS).await,
        None => state.store.entities_in_bbox(bbox, &filter, MAX_ROWS).await,
    }
    .map_err(|err| {
        tracing::warn!("snapshot query failed: {err}");
        ServerFrame::Error {
            message: "snapshot query failed".into(),
        }
    })?;

    // The cursor starts in the recent past rather than at `now`, so an entity
    // updated between the snapshot query and the first tick is caught rather
    // than lost in the gap. The snapshot carries no `updated_at`, so `sent`
    // starts empty — the cost is that the first delta may repeat a handful of
    // rows the snapshot already carried, which is the safe direction to err.
    let cursor = Utc::now() - CURSOR_OVERLAP;
    Ok((
        Subscription {
            bbox,
            filter,
            cursor,
            sent: HashMap::new(),
        },
        ServerFrame::Snapshot {
            at: at.unwrap_or_else(Utc::now),
            count: entities.len(),
            entities,
        },
    ))
}

/// Send a frame; `false` means the socket is gone and the loop should end.
async fn send(socket: &mut WebSocket, frame: &ServerFrame) -> bool {
    match serde_json::to_string(frame) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(err) => {
            tracing::error!("could not serialise a stream frame: {err}");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subscribe_frame_parses_with_every_field_optional() {
        let frame: ClientFrame = serde_json::from_str(r#"{"type":"subscribe"}"#).unwrap();
        assert!(matches!(frame, ClientFrame::Subscribe { bbox: None, .. }));

        let frame: ClientFrame = serde_json::from_str(
            r#"{"type":"subscribe","bbox":"-2,51,0.5,52","layers":"flights","at":null}"#,
        )
        .unwrap();
        let ClientFrame::Subscribe { bbox, layers, .. } = frame else {
            panic!("expected a subscribe");
        };
        assert_eq!(bbox.as_deref(), Some("-2,51,0.5,52"));
        assert_eq!(layers.as_deref(), Some("flights"));
    }

    #[test]
    fn an_unknown_frame_type_is_a_parse_error_rather_than_a_default() {
        assert!(serde_json::from_str::<ClientFrame>(r#"{"type":"unsubscribe"}"#).is_err());
    }

    fn subscription() -> Subscription {
        Subscription {
            bbox: BoundingBox::GLOBAL,
            filter: EntityFilter::default(),
            cursor: Utc::now() - Duration::hours(1),
            sent: HashMap::new(),
        }
    }

    fn delta_row(key: &str, updated_at: DateTime<Utc>) -> argus_store::DeltaRow {
        argus_store::DeltaRow {
            entity: argus_store::EntityRow {
                entity_kind: "aircraft".into(),
                entity_key: key.into(),
                source_id: "adsb-lol".into(),
                layer_id: "flights".into(),
                observed_at: updated_at,
                lon: Some(-0.4),
                lat: Some(51.5),
                geom: None,
                alt_m: None,
                alt_datum: None,
                course_deg: None,
                heading_deg: None,
                speed_mps: None,
                vrate_mps: None,
                quality: "live".into(),
                label: None,
                attrs: serde_json::json!({}),
            },
            updated_at,
        }
    }

    #[test]
    fn a_quiet_feed_stops_producing_deltas_instead_of_repeating_itself() {
        // The bug this exists to prevent: the overlap re-queries the same rows
        // on every tick, and without this filter a source polling every fifteen
        // seconds sent its whole batch every two.
        let mut sub = subscription();
        let at = Utc::now();
        let batch = vec![delta_row("aaa", at), delta_row("bbb", at)];

        assert_eq!(sub.undelivered(batch.clone()).len(), 2);
        assert!(sub.undelivered(batch.clone()).is_empty());
        assert!(sub.undelivered(batch).is_empty());
    }

    #[test]
    fn a_genuinely_updated_entity_is_sent_again() {
        let mut sub = subscription();
        let at = Utc::now();
        assert_eq!(sub.undelivered(vec![delta_row("aaa", at)]).len(), 1);
        // Same aircraft, new fix: this must go out.
        let later = at + Duration::seconds(1);
        assert_eq!(sub.undelivered(vec![delta_row("aaa", later)]).len(), 1);
    }

    #[test]
    fn a_late_committing_row_inside_the_overlap_is_still_delivered() {
        // The case the overlap exists for: a transaction that commits after a
        // later one but carries an earlier `updated_at`.
        let mut sub = subscription();
        let at = Utc::now();
        assert_eq!(sub.undelivered(vec![delta_row("aaa", at)]).len(), 1);
        let earlier = at - Duration::seconds(1);
        assert!(earlier > sub.cursor, "the overlap must still cover it");
        assert_eq!(sub.undelivered(vec![delta_row("bbb", earlier)]).len(), 1);
    }

    #[test]
    fn delivered_rows_are_forgotten_once_they_fall_out_of_the_window() {
        // Otherwise a long-lived subscription accumulates every entity it has
        // ever seen.
        let mut sub = subscription();
        let old = Utc::now() - Duration::minutes(10);
        sub.undelivered(vec![delta_row("aaa", old)]);
        assert_eq!(sub.sent.len(), 1);
        sub.undelivered(vec![delta_row("bbb", Utc::now())]);
        assert_eq!(sub.sent.len(), 1, "the ten-minute-old entry should be gone");
        assert!(sub.sent.contains_key(&("aircraft".to_string(), "bbb".to_string())));
    }

    #[test]
    fn the_overlap_is_long_enough_to_cover_a_poll() {
        // The cursor reaches back further than one poll interval, so a row
        // committed out of order cannot fall between two consecutive queries.
        assert!(CURSOR_OVERLAP.to_std().unwrap() > POLL_INTERVAL);
    }
}
