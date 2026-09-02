//! The rule engine: what turns a polygon into a notification.
//!
//! It runs as its own loop rather than inside the ingest write path, for the
//! same reason the delta stream does: a per-row hook on the hottest write in
//! the system to save a second of latency on a map already showing data
//! seconds old is a bad trade. The engine polls `entities.updated_at` exactly
//! as the WebSocket does, over the index added in 0005.
//!
//! Two decisions are worth reading before changing anything here.
//!
//! **A restart must not fire a storm.** The engine keeps inside/outside state
//! in memory, so a naive start would consider every aircraft already sitting in
//! a fence to have just entered it and raise an alert for each. On startup the
//! current occupants are therefore recorded as present *without* firing. The
//! cost is that a genuine entry during the seconds the daemon was down is
//! missed, which is the right way round: a missed alert is a gap, a hundred
//! false ones are a reason to turn the feature off.
//!
//! **No fences means no work.** The query is bounded to the union of the
//! enabled fences, so a fence over one approach path costs one small box rather
//! than a scan of every satellite in the catalogue, and no fences at all costs
//! a comparison against an empty vector.

use crate::rule::{Candidate, Rule, Trigger};
use argus_core::entity::EntityId;
use argus_core::geo::BoundingBox;
use argus_store::{EntityFilter, NewAlert, Store};
use chrono::{DateTime, Duration, Utc};
use geo::algorithm::contains::Contains;
use std::collections::HashMap;

/// How often the engine looks for movement. Matches the delta stream: below the
/// cadence of every feed, so nothing is missed by waiting.
pub const TICK: std::time::Duration = std::time::Duration::from_secs(2);

/// How often the fence set is re-read. A fence created through the API should
/// arm itself without a restart, and thirty seconds is a tolerable wait for
/// something a person just drew.
pub const FENCE_REFRESH: Duration = Duration::seconds(30);

/// Cap on entities considered in one tick.
const MAX_ROWS: i64 = 5_000;

/// One fence, ready to test points against.
struct Fence {
    id: i64,
    name: String,
    rule: Rule,
    shape: geo_types::Geometry<f64>,
}

impl Fence {
    fn contains(&self, lon: f64, lat: f64) -> bool {
        let point = geo_types::Point::new(lon, lat);
        match &self.shape {
            geo_types::Geometry::Polygon(p) => p.contains(&point),
            geo_types::Geometry::MultiPolygon(p) => p.contains(&point),
            // The column is declared `geometry(Polygon, 4326)`, so anything
            // else means the schema changed under this code. Refusing to guess
            // beats treating a line as an area.
            _ => false,
        }
    }
}

/// What the engine remembers about one thing in one fence.
#[derive(Debug, Clone)]
struct Presence {
    entered_at: DateTime<Utc>,
    dwell_fired: bool,
    last_alert_at: Option<DateTime<Utc>>,
}

pub struct Engine {
    store: Store,
    fences: Vec<Fence>,
    fences_loaded_at: Option<DateTime<Utc>>,
    /// `(geofence_id, entity)` → since when.
    presence: HashMap<(i64, EntityId), Presence>,
    cursor: DateTime<Utc>,
    primed: bool,
}

impl Engine {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            fences: Vec::new(),
            fences_loaded_at: None,
            presence: HashMap::new(),
            // Start from now: the daemon has no business raising alerts about
            // things that happened while it was not running.
            cursor: Utc::now(),
            primed: false,
        }
    }

    /// Run until cancelled.
    pub async fn run(mut self, cancel: tokio_util::sync::CancellationToken) {
        tracing::info!("geofence engine started");
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(TICK) => {}
            }
            if let Err(err) = self.tick().await {
                // A failing tick is not a reason to stop watching. The store
                // may be briefly unavailable; the next tick re-reads from the
                // same cursor and loses nothing.
                tracing::warn!("geofence tick failed: {err}");
            }
        }
        tracing::info!("geofence engine stopped");
    }

    /// One pass. Public so a test can drive it without a clock.
    pub async fn tick(&mut self) -> Result<Vec<argus_store::model::AlertRow>, argus_store::StoreError> {
        self.refresh_fences().await?;
        if self.fences.is_empty() {
            // Nothing armed. Keep the cursor moving so that arming a fence
            // later does not replay everything that happened in between.
            self.cursor = Utc::now();
            self.presence.clear();
            return Ok(Vec::new());
        }

        let Some(bbox) = self.watch_box() else {
            return Ok(Vec::new());
        };

        if !self.primed {
            self.prime(bbox).await?;
            return Ok(Vec::new());
        }

        let rows = self
            .store
            .entities_changed_since(bbox, &EntityFilter::default(), self.cursor, MAX_ROWS)
            .await?;

        tracing::debug!(
            considered = rows.len(),
            cursor = %self.cursor,
            tracked = self.presence.len(),
            "geofence tick"
        );

        let mut fired = Vec::new();
        for row in &rows {
            self.cursor = self.cursor.max(row.updated_at);
            for alert in self.evaluate(&row.entity, row.updated_at) {
                match self.store.insert_alert(&alert).await {
                    Ok(written) => {
                        tracing::info!(
                            alert = written.alert_id,
                            severity = %written.severity,
                            "{}",
                            written.message
                        );
                        fired.push(written);
                    }
                    Err(err) => tracing::warn!("could not record an alert: {err}"),
                }
            }
        }
        Ok(fired)
    }

    /// Record who is already inside, without alerting about any of them.
    async fn prime(&mut self, bbox: BoundingBox) -> Result<(), argus_store::StoreError> {
        let now = Utc::now();
        let rows = self
            .store
            .entities_in_bbox(bbox, &EntityFilter::default(), MAX_ROWS)
            .await?;
        let mut occupants = 0usize;
        for entity in &rows {
            let (Some(lon), Some(lat)) = (entity.lon, entity.lat) else {
                continue;
            };
            for fence in &self.fences {
                if fence.contains(lon, lat) {
                    let id = key_of(entity);
                    self.presence.insert(
                        (fence.id, id),
                        Presence {
                            // Dated to now rather than to when it actually
                            // arrived, which is unknown: a dwell rule should
                            // start counting from when we started watching, not
                            // claim a duration nobody measured.
                            entered_at: now,
                            dwell_fired: false,
                            last_alert_at: None,
                        },
                    );
                    occupants += 1;
                }
            }
        }
        self.cursor = now;
        self.primed = true;
        tracing::info!(
            fences = self.fences.len(),
            occupants,
            "geofences armed; existing occupants recorded without alerting"
        );
        Ok(())
    }

    /// Decide what one entity's new position means for every fence.
    fn evaluate(&mut self, entity: &argus_store::EntityRow, at: DateTime<Utc>) -> Vec<NewAlert> {
        evaluate_against(&self.fences, &mut self.presence, entity, at)
    }
}

/// The state machine, lifted out of [`Engine`] so it can be tested without a
/// database.
///
/// This is the part with the interesting behaviour — entry versus exit versus
/// dwell, one alert per stay, cooldowns — and watching real aircraft cross a
/// real fence only ever exercises the entry path. Everything else would have
/// been unverified.
fn evaluate_against(
    fences: &[Fence],
    presence: &mut HashMap<(i64, EntityId), Presence>,
    entity: &argus_store::EntityRow,
    at: DateTime<Utc>,
) -> Vec<NewAlert> {
    {
        let mut out = Vec::new();
        let (Some(lon), Some(lat)) = (entity.lon, entity.lat) else {
            return out;
        };
        let Some(kind) = argus_store::model::parse_entity_kind(&entity.entity_kind) else {
            return out;
        };
        let id = EntityId::new(kind, entity.entity_key.clone());

        for fence in fences {
            let inside = fence.contains(lon, lat);
            let slot = (fence.id, id.clone());
            let was_inside = presence.get(&slot).cloned();

            let candidate = Candidate {
                kind,
                layer: &entity.layer_id,
                label: entity.label.as_deref(),
                alt_m: entity.alt_m,
                speed_mps: entity.speed_mps.map(f64::from),
                quality: &entity.quality,
            };
            let interesting = fence.rule.matches(&candidate);

            match (was_inside, inside) {
                (None, true) => {
                    let fires = interesting && fence.rule.trigger == Trigger::Enters;
                    presence.insert(
                        slot,
                        Presence {
                            entered_at: at,
                            dwell_fired: false,
                            last_alert_at: fires.then_some(at),
                        },
                    );
                    if fires {
                        out.push(alert_for(fence, &id, entity, at, "entered"));
                    }
                }
                (Some(previous), true) => {
                    // Still inside. Only a dwell rule has anything to say, and
                    // only once per stay — leaving and returning is a new stay
                    // because the presence entry is dropped on exit.
                    let dwelled = at - previous.entered_at
                        >= Duration::seconds(fence.rule.dwell_seconds as i64);
                    let fires = interesting
                        && fence.rule.trigger == Trigger::Dwells
                        && dwelled
                        && !previous.dwell_fired
                        && cooled_down(&previous, &fence.rule, at);
                    presence.insert(
                        slot,
                        Presence {
                            entered_at: previous.entered_at,
                            dwell_fired: previous.dwell_fired || fires,
                            last_alert_at: if fires { Some(at) } else { previous.last_alert_at },
                        },
                    );
                    if fires {
                        out.push(alert_for(fence, &id, entity, at, "still inside"));
                    }
                }
                (Some(previous), false) => {
                    presence.remove(&slot);
                    if interesting
                        && fence.rule.trigger == Trigger::Exits
                        && cooled_down(&previous, &fence.rule, at)
                    {
                        out.push(alert_for(fence, &id, entity, at, "left"));
                    }
                }
                // Outside, and was outside. The overwhelmingly common case, and
                // deliberately the cheapest.
                (None, false) => {}
            }
        }
        out
    }
}

impl Engine {
    async fn refresh_fences(&mut self) -> Result<(), argus_store::StoreError> {
        let now = Utc::now();
        if self
            .fences_loaded_at
            .is_some_and(|loaded| now - loaded < FENCE_REFRESH)
        {
            return Ok(());
        }

        let rows = self.store.geofences(true).await?;
        let before: Vec<i64> = self.fences.iter().map(|f| f.id).collect();
        let mut fences = Vec::new();
        for row in rows {
            let rule: Rule = match serde_json::from_value(row.rule.clone()) {
                Ok(rule) => rule,
                Err(err) => {
                    // Named and skipped rather than defaulted. A fence whose
                    // rule will not parse is not "a fence that matches
                    // everything" — it is a fence its author cannot trust.
                    tracing::warn!(
                        geofence = row.geofence_id,
                        name = row.name,
                        "ignoring a geofence whose rule will not parse: {err}"
                    );
                    continue;
                }
            };
            let Some(shape) = row.geom.geometry else {
                tracing::warn!(geofence = row.geofence_id, "geofence has no usable geometry");
                continue;
            };
            fences.push(Fence { id: row.geofence_id, name: row.name, rule, shape });
        }

        let after: Vec<i64> = fences.iter().map(|f| f.id).collect();
        if before != after {
            tracing::info!(fences = after.len(), "geofence set changed");
            // A fence that has just appeared has no presence history, so its
            // current occupants would all read as fresh entries. Re-prime.
            self.primed = false;
            self.presence
                .retain(|(fence_id, _), _| after.contains(fence_id));
        }
        self.fences = fences;
        self.fences_loaded_at = Some(now);
        Ok(())
    }

    /// The smallest box covering every armed fence.
    ///
    /// This is what keeps the engine cheap: without it every tick would pull
    /// every satellite in the catalogue to ask whether it is over a car park in
    /// Hounslow.
    fn watch_box(&self) -> Option<BoundingBox> {
        let mut bounds: Option<(f64, f64, f64, f64)> = None;
        for fence in &self.fences {
            let rect = geo::algorithm::bounding_rect::BoundingRect::bounding_rect(&fence.shape)?;
            let (w, s, e, n) = (rect.min().x, rect.min().y, rect.max().x, rect.max().y);
            bounds = Some(match bounds {
                None => (w, s, e, n),
                Some((pw, ps, pe, pn)) => (pw.min(w), ps.min(s), pe.max(e), pn.max(n)),
            });
        }
        bounds.map(|(w, s, e, n)| BoundingBox::new(w, s, e, n))
    }
}

fn cooled_down(previous: &Presence, rule: &Rule, at: DateTime<Utc>) -> bool {
    previous
        .last_alert_at
        .is_none_or(|last| at - last >= rule.cooldown())
}

fn key_of(entity: &argus_store::EntityRow) -> EntityId {
    let kind = argus_store::model::parse_entity_kind(&entity.entity_kind)
        .unwrap_or(argus_core::EntityKind::Feature);
    EntityId::new(kind, entity.entity_key.clone())
}

fn alert_for(
    fence: &Fence,
    id: &EntityId,
    entity: &argus_store::EntityRow,
    at: DateTime<Utc>,
    verb: &str,
) -> NewAlert {
    let what = entity.label.clone().unwrap_or_else(|| id.key.clone());
    NewAlert {
        geofence_id: Some(fence.id),
        entity: id.clone(),
        fired_at: at,
        severity: fence.rule.severity.as_str().to_string(),
        message: format!("{what} {verb} {}", fence.name),
        lon: entity.lon,
        lat: entity.lat,
        // Carried so a client can render the card without a second fetch, and
        // so the alert stays meaningful after the entity has moved on.
        attrs: serde_json::json!({
            "geofence": fence.name,
            "layer": entity.layer_id,
            "alt_m": entity.alt_m,
            "speed_mps": entity.speed_mps,
            "quality": entity.quality,
            "trigger": fence.rule.trigger,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::Severity;
    use geo_types::{Coord, LineString, Polygon};

    /// A one-degree box at the origin.
    fn fence(rule: Rule) -> Fence {
        let ring = LineString::from(vec![
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 1.0, y: 0.0 },
            Coord { x: 1.0, y: 1.0 },
            Coord { x: 0.0, y: 1.0 },
            Coord { x: 0.0, y: 0.0 },
        ]);
        Fence {
            id: 1,
            name: "the box".into(),
            rule,
            shape: geo_types::Geometry::Polygon(Polygon::new(ring, vec![])),
        }
    }

    fn aircraft(lon: f64, lat: f64) -> argus_store::EntityRow {
        argus_store::EntityRow {
            entity_kind: "aircraft".into(),
            entity_key: "abc123".into(),
            source_id: "test".into(),
            layer_id: "flights".into(),
            observed_at: Utc::now(),
            lon: Some(lon),
            lat: Some(lat),
            geom: None,
            alt_m: Some(500.0),
            alt_datum: Some("barometric".into()),
            course_deg: None,
            heading_deg: None,
            speed_mps: Some(80.0),
            vrate_mps: None,
            quality: "live".into(),
            label: Some("BAW123".into()),
            attrs: serde_json::json!({}),
        }
    }

    const INSIDE: (f64, f64) = (0.5, 0.5);
    const OUTSIDE: (f64, f64) = (2.0, 2.0);

    #[test]
    fn entering_fires_once_and_staying_does_not_fire_again() {
        let fences = [fence(Rule::default())];
        let mut presence = HashMap::new();
        let t0 = Utc::now();

        let first = evaluate_against(&fences, &mut presence, &aircraft(INSIDE.0, INSIDE.1), t0);
        assert_eq!(first.len(), 1);
        assert!(first[0].message.contains("BAW123 entered the box"));

        // Still inside a minute later: an entry rule has nothing more to say,
        // or a single aircraft on approach would raise an alert per fix.
        let again = evaluate_against(
            &fences,
            &mut presence,
            &aircraft(0.6, 0.6),
            t0 + Duration::minutes(1),
        );
        assert!(again.is_empty());
    }

    #[test]
    fn leaving_and_returning_is_a_new_entry() {
        let fences = [fence(Rule::default())];
        let mut presence = HashMap::new();
        let t0 = Utc::now();

        assert_eq!(
            evaluate_against(&fences, &mut presence, &aircraft(INSIDE.0, INSIDE.1), t0).len(),
            1
        );
        // Out: an entry rule says nothing, but the presence record must be
        // dropped or the return below would read as "still inside".
        assert!(evaluate_against(
            &fences,
            &mut presence,
            &aircraft(OUTSIDE.0, OUTSIDE.1),
            t0 + Duration::minutes(1)
        )
        .is_empty());
        assert!(presence.is_empty(), "leaving must forget the stay");

        assert_eq!(
            evaluate_against(
                &fences,
                &mut presence,
                &aircraft(INSIDE.0, INSIDE.1),
                t0 + Duration::minutes(2)
            )
            .len(),
            1,
            "a go-around is a second entry, not a continuation of the first"
        );
    }

    #[test]
    fn an_exit_rule_fires_on_the_way_out_and_not_on_the_way_in() {
        let fences = [fence(Rule { trigger: Trigger::Exits, ..Rule::default() })];
        let mut presence = HashMap::new();
        let t0 = Utc::now();

        assert!(
            evaluate_against(&fences, &mut presence, &aircraft(INSIDE.0, INSIDE.1), t0).is_empty(),
            "an exit rule is silent on entry"
        );
        let out = evaluate_against(
            &fences,
            &mut presence,
            &aircraft(OUTSIDE.0, OUTSIDE.1),
            t0 + Duration::minutes(1),
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].message.contains("left the box"));
    }

    /// Something that was never seen inside cannot be seen leaving.
    #[test]
    fn an_exit_rule_says_nothing_about_something_that_was_never_inside() {
        let fences = [fence(Rule { trigger: Trigger::Exits, ..Rule::default() })];
        let mut presence = HashMap::new();
        assert!(
            evaluate_against(
                &fences,
                &mut presence,
                &aircraft(OUTSIDE.0, OUTSIDE.1),
                Utc::now()
            )
            .is_empty()
        );
    }

    #[test]
    fn a_dwell_rule_waits_for_the_duration_then_fires_once() {
        let fences = [fence(Rule {
            trigger: Trigger::Dwells,
            dwell_seconds: 300,
            ..Rule::default()
        })];
        let mut presence = HashMap::new();
        let t0 = Utc::now();

        // Arriving is not dwelling.
        assert!(
            evaluate_against(&fences, &mut presence, &aircraft(INSIDE.0, INSIDE.1), t0).is_empty()
        );
        // Four minutes in: still traffic, not a hold.
        assert!(
            evaluate_against(
                &fences,
                &mut presence,
                &aircraft(0.51, 0.51),
                t0 + Duration::minutes(4)
            )
            .is_empty()
        );
        // Six: a hold.
        let held = evaluate_against(
            &fences,
            &mut presence,
            &aircraft(0.52, 0.52),
            t0 + Duration::minutes(6),
        );
        assert_eq!(held.len(), 1);
        assert!(held[0].message.contains("still inside"));

        // And once per stay, not once per fix for the rest of the hold.
        assert!(
            evaluate_against(
                &fences,
                &mut presence,
                &aircraft(0.53, 0.53),
                t0 + Duration::minutes(7)
            )
            .is_empty()
        );
    }

    #[test]
    fn a_rule_that_does_not_match_never_fires_but_presence_is_still_tracked() {
        // The distinction matters for exits: a fence watching for low aircraft
        // must not report a high one leaving, but it also must not accumulate a
        // presence record it never clears.
        let fences = [fence(Rule { max_alt_m: Some(100.0), ..Rule::default() })];
        let mut presence = HashMap::new();
        let t0 = Utc::now();

        let mut high = aircraft(INSIDE.0, INSIDE.1);
        high.alt_m = Some(9000.0);
        assert!(evaluate_against(&fences, &mut presence, &high, t0).is_empty());
        assert_eq!(presence.len(), 1, "geometry is tracked even when the rule declines");

        let mut gone = aircraft(OUTSIDE.0, OUTSIDE.1);
        gone.alt_m = Some(9000.0);
        assert!(evaluate_against(&fences, &mut presence, &gone, t0 + Duration::minutes(1)).is_empty());
        assert!(presence.is_empty());
    }

    #[test]
    fn an_entity_with_no_position_is_ignored_rather_than_placed_at_null_island() {
        let fences = [fence(Rule::default())];
        let mut presence = HashMap::new();
        let mut nowhere = aircraft(0.0, 0.0);
        nowhere.lon = None;
        nowhere.lat = None;
        assert!(evaluate_against(&fences, &mut presence, &nowhere, Utc::now()).is_empty());
        assert!(presence.is_empty());
    }

    #[test]
    fn the_alert_carries_the_severity_the_rule_asked_for() {
        let fences = [fence(Rule { severity: Severity::Critical, ..Rule::default() })];
        let mut presence = HashMap::new();
        let fired = evaluate_against(
            &fences,
            &mut presence,
            &aircraft(INSIDE.0, INSIDE.1),
            Utc::now(),
        );
        assert_eq!(fired[0].severity, "critical");
        assert_eq!(fired[0].attrs["geofence"], "the box");
    }
}
