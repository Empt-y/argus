//! The poll scheduler.
//!
//! Drivers declare what they are; this decides when they run. Centralising
//! cadence, jitter, backoff, error classification and health accounting here is
//! the reason a new driver is a ~200-line file — upstream re-implemented all of
//! it per feed, inside one 7,000-line config module.

use argus_core::source::{
    AuthRequirement, Cadence, PollCtx, Source, SourceError, SourceHealth,
};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;

/// Backoff bounds for a failing source. Deliberately capped well below the
/// point where a recovered upstream would sit unnoticed for hours.
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(600);

/// How far behind `observed_at` a source may drift before it is reported as
/// `Delayed` rather than `Live`.
const DELAYED_THRESHOLD: chrono::Duration = chrono::Duration::seconds(120);

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Multiplier applied to cadence outside the declared areas of interest.
    pub global_cadence_scale: f64,
    /// When true, the disk guard has tripped: only AOI-scoped polling runs.
    pub aoi_only: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            global_cadence_scale: 4.0,
            aoi_only: false,
        }
    }
}

/// Per-source runtime state. Distinct from [`SourceHealth`], which is the
/// outward-facing view; this is the bookkeeping that produces it.
#[derive(Debug, Clone)]
pub struct SourceState {
    pub health: SourceHealth,
    pub consecutive_failures: u32,
    pub total_observations: u64,
}

impl Default for SourceState {
    fn default() -> Self {
        Self {
            // Never polled yet. Distinct from a successful poll returning zero
            // rows — see the note on SourceHealth::Unknown.
            health: SourceHealth::Unknown,
            consecutive_failures: 0,
            total_observations: 0,
        }
    }
}

impl SourceState {
    /// Fold one poll outcome into the source's state.
    pub fn record(&mut self, outcome: Result<PollOutcome, &SourceError>) {
        match outcome {
            Ok(o) => {
                self.consecutive_failures = 0;
                self.total_observations += o.accepted;
                self.health = if o.lag > DELAYED_THRESHOLD {
                    SourceHealth::Delayed {
                        lag: o.lag,
                        observations: o.accepted,
                    }
                } else if o.rejected > 0 {
                    SourceHealth::Degraded {
                        reason: format!("{} of {} readings failed validation", o.rejected, o.total()),
                        observations: o.accepted,
                    }
                } else {
                    SourceHealth::Live {
                        observations: o.accepted,
                    }
                };
            }
            Err(err) => {
                self.consecutive_failures += 1;
                self.health = match err {
                    // A rejected credential is not a transient fault, and
                    // hammering an upstream that has said no is how you get
                    // an IP ban rather than a fix.
                    SourceError::Auth(msg) => SourceHealth::Failed {
                        error: msg.clone(),
                        since: Utc::now(),
                    },
                    SourceError::HardwareMissing(desc) => SourceHealth::HardwareAbsent {
                        description: desc.clone(),
                    },
                    // Having answered before means we still hold a usable last
                    // value, which is 'stale'. Never having answered is not.
                    other if self.total_observations > 0 => SourceHealth::Stale {
                        since: Utc::now(),
                        last_error: other.to_string(),
                    },
                    other => SourceHealth::Failed {
                        error: other.to_string(),
                        since: Utc::now(),
                    },
                };
            }
        }
    }

    /// Whether the scheduler should keep polling this source at all.
    pub fn should_continue(&self) -> bool {
        !matches!(
            self.health,
            SourceHealth::Failed { .. } | SourceHealth::HardwareAbsent { .. }
        )
    }
}

/// What one poll produced.
#[derive(Debug, Clone, Copy, Default)]
pub struct PollOutcome {
    pub accepted: u64,
    pub rejected: u64,
    pub lag: chrono::Duration,
}

impl PollOutcome {
    pub fn total(&self) -> u64 {
        self.accepted + self.rejected
    }
}

/// How long to wait before the next poll of this source.
///
/// Three inputs, in priority order: an explicit `Retry-After` from the upstream
/// always wins, then exponential backoff while failing, then the declared
/// cadence scaled by budget and by whether we are inside an area of interest.
pub fn next_delay(
    cadence: Cadence,
    state: &SourceState,
    config: &SchedulerConfig,
    budget_remaining: f64,
    retry_after: Option<Duration>,
) -> Duration {
    if let Some(after) = retry_after {
        return after.min(BACKOFF_MAX);
    }

    if state.consecutive_failures > 0 {
        // 5s, 10s, 20s, ... capped. saturating_sub guards the shift on the
        // pathological case of a very long failure run.
        let shift = state.consecutive_failures.saturating_sub(1).min(16);
        let backoff = BACKOFF_BASE.saturating_mul(1u32 << shift);
        return backoff.min(BACKOFF_MAX);
    }

    let base = match cadence {
        Cadence::Fixed { every } => every,
        Cadence::Adaptive { floor, ceiling } => {
            // Spend budget when we have it: full budget polls at the floor,
            // empty budget at the ceiling, linear between.
            let t = budget_remaining.clamp(0.0, 1.0);
            let span = ceiling.saturating_sub(floor);
            floor + Duration::from_secs_f64(span.as_secs_f64() * (1.0 - t))
        }
        // Streaming sources are supervised, not polled; Static ones are fetched
        // once. Neither should be on a timer, so park them well out of the way
        // rather than spinning.
        Cadence::Streaming | Cadence::Static => return BACKOFF_MAX,
    };

    let scaled = if config.aoi_only {
        base.mul_f64(config.global_cadence_scale)
    } else {
        base
    };
    scaled.max(Duration::from_secs(1))
}

/// Spread the first poll of each source across its own interval.
///
/// Without this, every source registered at startup fires simultaneously on
/// every subsequent cycle — a thundering herd against a dozen upstreams, and a
/// write spike into the same hypertable chunk.
pub fn startup_jitter(interval: Duration, source_index: usize, source_count: usize) -> Duration {
    if source_count <= 1 {
        return Duration::ZERO;
    }
    interval.mul_f64(source_index as f64 / source_count as f64)
}

/// Whether a source can run at all with the credentials it has been given.
pub fn auth_state(auth: &AuthRequirement, has_credential: bool) -> Option<SourceHealth> {
    match auth {
        AuthRequirement::Required { config_key } if !has_credential => {
            Some(SourceHealth::KeyRequired {
                config_key: config_key.clone(),
            })
        }
        AuthRequirement::OAuth { client_id_key, .. } if !has_credential => {
            Some(SourceHealth::KeyRequired {
                config_key: client_id_key.clone(),
            })
        }
        AuthRequirement::Hardware { description } if !has_credential => {
            Some(SourceHealth::HardwareAbsent {
                description: description.clone(),
            })
        }
        // Optional keys never gate the source: the layer is fully usable
        // keyless and must not be reported as unavailable.
        _ => None,
    }
}

/// Run one poll and classify the result. Kept separate from the driving loop so
/// it can be tested without timers.
pub async fn poll_once(source: &Arc<dyn Source>, ctx: &PollCtx) -> Result<Vec<argus_core::Observation>, SourceError> {
    let observations = source.poll(ctx).await?;
    let descriptor = source.descriptor();
    Ok(observations
        .into_iter()
        // A driver must not be able to claim better provenance than its source
        // declares. Clamping here rather than trusting each driver means a new
        // one cannot accidentally promote estimates to live readings.
        .map(|mut o| {
            if !descriptor.base_quality.is_measured() && o.quality.is_measured() {
                o.quality = descriptor.base_quality;
            }
            o
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_core::source::SourceHealth;

    fn failing_state(n: u32, observations: u64) -> SourceState {
        SourceState {
            health: SourceHealth::Unknown,
            consecutive_failures: n,
            total_observations: observations,
        }
    }

    #[test]
    fn backoff_grows_exponentially_and_is_capped() {
        let cfg = SchedulerConfig::default();
        let c = Cadence::every(15);
        let d1 = next_delay(c, &failing_state(1, 0), &cfg, 1.0, None);
        let d2 = next_delay(c, &failing_state(2, 0), &cfg, 1.0, None);
        let d3 = next_delay(c, &failing_state(3, 0), &cfg, 1.0, None);
        assert_eq!(d1, Duration::from_secs(5));
        assert_eq!(d2, Duration::from_secs(10));
        assert_eq!(d3, Duration::from_secs(20));
        // Never grows without bound, however long the outage.
        assert_eq!(next_delay(c, &failing_state(99, 0), &cfg, 1.0, None), BACKOFF_MAX);
    }

    #[test]
    fn an_upstream_retry_after_overrides_our_own_backoff() {
        let cfg = SchedulerConfig::default();
        let delay = next_delay(
            Cadence::every(15),
            &failing_state(5, 0),
            &cfg,
            1.0,
            Some(Duration::from_secs(42)),
        );
        assert_eq!(delay, Duration::from_secs(42));
    }

    #[test]
    fn adaptive_cadence_slows_as_budget_drains() {
        let cfg = SchedulerConfig::default();
        let c = Cadence::Adaptive {
            floor: Duration::from_secs(10),
            ceiling: Duration::from_secs(60),
        };
        let healthy = SourceState::default();
        assert_eq!(next_delay(c, &healthy, &cfg, 1.0, None), Duration::from_secs(10));
        assert_eq!(next_delay(c, &healthy, &cfg, 0.0, None), Duration::from_secs(60));
        assert_eq!(next_delay(c, &healthy, &cfg, 0.5, None), Duration::from_secs(35));
    }

    #[test]
    fn the_disk_guard_slows_polling_rather_than_stopping_it() {
        let cfg = SchedulerConfig {
            aoi_only: true,
            global_cadence_scale: 4.0,
        };
        let delay = next_delay(Cadence::every(15), &SourceState::default(), &cfg, 1.0, None);
        assert_eq!(delay, Duration::from_secs(60));
    }

    #[test]
    fn streaming_and_static_sources_are_not_put_on_a_timer() {
        let cfg = SchedulerConfig::default();
        let s = SourceState::default();
        assert_eq!(next_delay(Cadence::Streaming, &s, &cfg, 1.0, None), BACKOFF_MAX);
        assert_eq!(next_delay(Cadence::Static, &s, &cfg, 1.0, None), BACKOFF_MAX);
    }

    #[test]
    fn a_source_that_answered_before_goes_stale_not_failed() {
        // The distinction matters: 'stale' means we still hold a usable last
        // value, 'failed' means we have nothing at all.
        let mut had_data = SourceState {
            total_observations: 500,
            ..Default::default()
        };
        had_data.record(Err(&SourceError::Transport("timeout".into())));
        assert!(matches!(had_data.health, SourceHealth::Stale { .. }));

        let mut never_answered = SourceState::default();
        never_answered.record(Err(&SourceError::Transport("timeout".into())));
        assert!(matches!(never_answered.health, SourceHealth::Failed { .. }));
    }

    #[test]
    fn a_rejected_credential_stops_the_source_rather_than_retrying() {
        let mut state = SourceState {
            total_observations: 500,
            ..Default::default()
        };
        state.record(Err(&SourceError::Auth("401".into())));
        // Even though it had data, auth failure is terminal — retrying a
        // rejected key earns an IP ban, not a fix.
        assert!(matches!(state.health, SourceHealth::Failed { .. }));
        assert!(!state.should_continue());
    }

    #[test]
    fn a_lagging_source_reports_delayed_rather_than_live() {
        let mut state = SourceState::default();
        state.record(Ok(PollOutcome {
            accepted: 10,
            rejected: 0,
            lag: chrono::Duration::seconds(300),
        }));
        assert!(matches!(state.health, SourceHealth::Delayed { .. }));
    }

    #[test]
    fn partial_validation_failures_report_degraded() {
        let mut state = SourceState::default();
        state.record(Ok(PollOutcome {
            accepted: 90,
            rejected: 10,
            lag: chrono::Duration::zero(),
        }));
        match state.health {
            SourceHealth::Degraded { ref reason, .. } => {
                assert!(reason.contains("10 of 100"), "got: {reason}");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[test]
    fn a_successful_poll_clears_the_failure_run() {
        let mut state = failing_state(5, 100);
        state.record(Ok(PollOutcome {
            accepted: 1,
            rejected: 0,
            lag: chrono::Duration::zero(),
        }));
        assert_eq!(state.consecutive_failures, 0);
        assert!(matches!(state.health, SourceHealth::Live { .. }));
    }

    #[test]
    fn a_missing_required_key_is_reported_as_configuration_not_failure() {
        let auth = AuthRequirement::Required {
            config_key: "map_key".into(),
        };
        let health = auth_state(&auth, false).expect("gated");
        assert!(health.is_configured_off());
        assert!(!health.is_problem());
        assert!(auth_state(&auth, true).is_none());
    }

    #[test]
    fn an_optional_key_never_gates_a_source() {
        // The layer is fully usable keyless; reporting it unavailable would be
        // a lie that makes users chase a key they do not need.
        let auth = AuthRequirement::Optional {
            config_key: "client_id".into(),
        };
        assert!(auth_state(&auth, false).is_none());
    }

    #[test]
    fn startup_jitter_spreads_sources_across_the_interval() {
        let interval = Duration::from_secs(60);
        assert_eq!(startup_jitter(interval, 0, 4), Duration::ZERO);
        assert_eq!(startup_jitter(interval, 1, 4), Duration::from_secs(15));
        assert_eq!(startup_jitter(interval, 3, 4), Duration::from_secs(45));
        // A lone source has nothing to spread against.
        assert_eq!(startup_jitter(interval, 0, 1), Duration::ZERO);
    }
}
