//! Provider failover: one layer, several interchangeable upstreams, tried in
//! order.
//!
//! Almost every layer here can be served by more than one free provider, and
//! every free provider has a limit. ADS-B alone has four keyless options;
//! earthquakes, routing, geocoding and weather each have two or three. Treating
//! them as a ranked chain rather than as one hard-wired choice means a spent
//! allowance degrades the layer instead of ending it.
//!
//! The chain is itself a [`Source`], so the scheduler needs to know nothing
//! about any of this — it polls a chain exactly as it polls a single driver.
//!
//! Three rules shape the behaviour, and each exists because the obvious
//! alternative is worse:
//!
//! 1. **Stick, don't re-probe every poll.** Once a provider answers, keep using
//!    it. Walking the chain from the top every time spends the scarce primary
//!    allowance on liveness checks.
//! 2. **But do return home.** Periodically retry higher-ranked providers, or a
//!    daily quota that resets at midnight would never be noticed and the layer
//!    would sit on its worst provider forever.
//! 3. **Distinguish "spent" from "broken".** An exhausted allowance is expected
//!    and recovers on a known schedule; a rejected key never recovers on its
//!    own. They get different cooldowns and different health.

use argus_core::source::{
    Attribution, Cadence, PollCtx, Quota, Source, SourceDescriptor, SourceError,
    SourceHealth, SourceId,
};
use argus_core::Observation;
use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;
use tokio::sync::Mutex;

/// How long to sideline a provider after a transport failure. Short: transport
/// failures are usually momentary.
const TRANSPORT_COOLDOWN: Duration = Duration::minutes(2);

/// How long to sideline a provider that rate-limited us without saying for how
/// long.
const DEFAULT_RATE_LIMIT_COOLDOWN: Duration = Duration::minutes(15);

/// How often to retry a higher-ranked provider while running on a fallback.
const PROMOTION_RETRY_INTERVAL: Duration = Duration::minutes(30);

/// Per-provider runtime state. Persisted via the `sources` table so an
/// allowance spent before a restart is still spent after it — otherwise
/// restarting the daemon would silently reset every quota and walk straight
/// into a wall of 429s.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderState {
    /// Not eligible until this instant.
    pub cooldown_until: Option<DateTime<Utc>>,
    /// Units consumed in the current quota window.
    pub used: u32,
    /// When the current quota window opened.
    pub window_started: DateTime<Utc>,
    /// Set when the provider cannot recover without operator action — a
    /// rejected credential, a missing key. Never cleared by a timer.
    pub unavailable: Option<String>,
    /// Last successful poll, for reporting.
    pub last_success: Option<DateTime<Utc>>,
    /// Observations this provider has contributed while serving.
    pub observations: u64,
}

impl ProviderState {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            cooldown_until: None,
            used: 0,
            window_started: now,
            unavailable: None,
            last_success: None,
            observations: 0,
        }
    }

    /// Roll the quota window over if it has elapsed.
    pub fn refresh_window(&mut self, quota: Option<Quota>, now: DateTime<Utc>) {
        let Some(quota) = quota else { return };
        let Ok(window) = Duration::from_std(quota.window) else {
            return;
        };
        if now - self.window_started >= window {
            self.used = 0;
            self.window_started = now;
        }
    }

    /// Whether this provider can be called right now.
    pub fn is_eligible(&self, quota: Option<Quota>, now: DateTime<Utc>) -> bool {
        if self.unavailable.is_some() {
            return false;
        }
        if self.cooldown_until.is_some_and(|until| now < until) {
            return false;
        }
        if let Some(quota) = quota
            && quota.polls_remaining(self.used) == 0
        {
            return false;
        }
        true
    }

    /// Why this provider is being skipped, for health reporting.
    pub fn skip_reason(&self, quota: Option<Quota>, now: DateTime<Utc>) -> Option<String> {
        if let Some(reason) = &self.unavailable {
            return Some(format!("unavailable: {reason}"));
        }
        if let Some(until) = self.cooldown_until
            && now < until
        {
            return Some(format!(
                "cooling down for {}s",
                (until - now).num_seconds().max(0)
            ));
        }
        if let Some(quota) = quota
            && quota.polls_remaining(self.used) == 0
        {
            let resets_in = Duration::from_std(quota.window)
                .map(|w| (self.window_started + w - now).num_minutes().max(0))
                .unwrap_or(0);
            return Some(format!(
                "allowance spent ({}/{}), resets in {resets_in}m",
                self.used, quota.limit
            ));
        }
        None
    }
}

/// Pick the highest-ranked eligible provider.
///
/// Preference order is the order the chain was built in, so the primary is
/// index 0. Returns `None` when every provider is sidelined.
pub fn select_provider(
    quotas: &[Option<Quota>],
    states: &[ProviderState],
    now: DateTime<Utc>,
) -> Option<usize> {
    states
        .iter()
        .zip(quotas)
        .position(|(state, quota)| state.is_eligible(*quota, now))
}

/// Whether to reach back up the chain for a better provider.
///
/// Running on a fallback is a degraded state, not a new normal — without this
/// a daily allowance that resets at midnight would never be noticed.
pub fn should_retry_promotion(
    current: usize,
    last_promotion_attempt: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    if current == 0 {
        return false;
    }
    last_promotion_attempt.is_none_or(|last| now - last >= PROMOTION_RETRY_INTERVAL)
}

/// How long to sideline a provider after a given failure.
///
/// `None` means "do not sideline on a timer" — the failure is terminal and the
/// provider is marked unavailable instead.
pub fn cooldown_for(err: &SourceError) -> Option<Duration> {
    match err {
        SourceError::RateLimited { retry_after } => Some(
            retry_after
                .and_then(|d| Duration::from_std(d).ok())
                .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN),
        ),
        SourceError::Transport(_) | SourceError::Decode(_) => Some(TRANSPORT_COOLDOWN),
        SourceError::ResponseTooLarge { .. } => Some(TRANSPORT_COOLDOWN),
        // A refused credential, absent hardware, or an unresolved 403 will not
        // fix itself. Marked unavailable rather than retried on a timer.
        SourceError::Auth(_) | SourceError::HardwareMissing(_) | SourceError::Forbidden(_) => None,
        SourceError::Other(_) => Some(TRANSPORT_COOLDOWN),
    }
}

struct ChainRuntime {
    states: Vec<ProviderState>,
    /// Which provider answered last.
    current: usize,
    last_promotion_attempt: Option<DateTime<Utc>>,
}

/// Several interchangeable providers presented as one source.
pub struct ProviderChain {
    descriptor: SourceDescriptor,
    providers: Vec<Arc<dyn Source>>,
    runtime: Mutex<ChainRuntime>,
}

impl ProviderChain {
    /// Build a chain. `providers` is in preference order, best first.
    ///
    /// The chain's descriptor is synthesised from the primary: it describes the
    /// *layer*, since that is what the scheduler and the clients care about.
    /// Per-provider detail stays addressable through [`Self::providers`].
    pub fn new(id: impl Into<String>, providers: Vec<Arc<dyn Source>>) -> Self {
        assert!(!providers.is_empty(), "a provider chain needs at least one provider");
        let primary = providers[0].descriptor();

        // A chain runs as often as its *most* capable member allows; a
        // fallback with a slower cadence still gets polled at the chain rate,
        // which is correct — it is standing in for the primary.
        let cadence = providers
            .iter()
            .map(|p| p.descriptor().cadence)
            .min_by_key(|c| match c {
                Cadence::Fixed { every } => every.as_secs(),
                Cadence::Adaptive { floor, .. } => floor.as_secs(),
                Cadence::Streaming | Cadence::Static => u64::MAX,
            })
            .unwrap_or(primary.cadence);

        let descriptor = SourceDescriptor {
            id: SourceId::new(id),
            layer_id: primary.layer_id.clone(),
            display_name: primary.display_name.clone(),
            kind: primary.kind,
            cadence,
            coverage: primary.coverage.clone(),
            // A chain is usable if ANY member is. Reporting the primary's
            // requirement would tell a user a key is required when three
            // keyless fallbacks are standing right behind it.
            auth: providers
                .iter()
                .map(|p| p.descriptor().auth.clone())
                .find(|a| matches!(a, argus_core::AuthRequirement::None))
                .unwrap_or_else(|| primary.auth.clone()),
            cost: primary.cost,
            attribution: Attribution {
                provider: providers
                    .iter()
                    .map(|p| p.descriptor().attribution.provider.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
                url: primary.attribution.url.clone(),
                license: "See individual providers".into(),
                notice: None,
            },
            // The chain can only promise what its weakest member delivers.
            base_quality: providers
                .iter()
                .map(|p| p.descriptor().base_quality)
                .max_by_key(|q| match q {
                    argus_core::Quality::Live => 0,
                    argus_core::Quality::Delayed => 1,
                    argus_core::Quality::Modeled => 2,
                    argus_core::Quality::Estimated => 3,
                    argus_core::Quality::Stale => 4,
                })
                .unwrap_or(primary.base_quality),
            // The chain has no single allowance; each member has its own.
            quota: None,
        };

        let now = Utc::now();
        let runtime = ChainRuntime {
            states: providers.iter().map(|_| ProviderState::new(now)).collect(),
            current: 0,
            last_promotion_attempt: None,
        };

        Self {
            descriptor,
            providers,
            runtime: Mutex::new(runtime),
        }
    }

    pub fn providers(&self) -> &[Arc<dyn Source>] {
        &self.providers
    }

    fn quotas(&self) -> Vec<Option<Quota>> {
        self.providers.iter().map(|p| p.descriptor().quota).collect()
    }

    /// Current per-provider state, for health reporting.
    pub async fn provider_states(&self) -> Vec<(SourceId, ProviderState)> {
        let rt = self.runtime.lock().await;
        self.providers
            .iter()
            .zip(rt.states.iter())
            .map(|(p, s)| (p.descriptor().id.clone(), s.clone()))
            .collect()
    }

    /// Which provider is currently serving, and why the ones above it are not.
    pub async fn status(&self) -> ChainStatus {
        let rt = self.runtime.lock().await;
        let quotas = self.quotas();
        let now = Utc::now();
        ChainStatus {
            serving: self.providers[rt.current].descriptor().id.clone(),
            serving_rank: rt.current,
            skipped: self
                .providers
                .iter()
                .zip(rt.states.iter())
                .zip(quotas.iter())
                .take(rt.current)
                .filter_map(|((p, s), q)| {
                    s.skip_reason(*q, now)
                        .map(|r| (p.descriptor().id.clone(), r))
                })
                .collect(),
        }
    }
}

/// A snapshot of which provider is answering for a layer.
#[derive(Debug, Clone)]
pub struct ChainStatus {
    pub serving: SourceId,
    /// 0 is the primary; anything higher means the layer is degraded.
    pub serving_rank: usize,
    /// Higher-ranked providers and why each is sidelined.
    pub skipped: Vec<(SourceId, String)>,
}

impl ChainStatus {
    /// Whether the layer is being served by anything other than its primary.
    pub fn is_degraded(&self) -> bool {
        self.serving_rank > 0
    }

    /// A health value describing the chain as a whole.
    pub fn health(&self, observations: u64) -> SourceHealth {
        if self.is_degraded() {
            SourceHealth::Degraded {
                reason: format!(
                    "serving via {} ({})",
                    self.serving,
                    self.skipped
                        .iter()
                        .map(|(id, why)| format!("{id} {why}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                observations,
            }
        } else {
            SourceHealth::Live { observations }
        }
    }
}

#[async_trait::async_trait]
impl Source for ProviderChain {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    fn members(&self) -> Vec<&SourceDescriptor> {
        self.providers.iter().map(|p| p.descriptor()).collect()
    }

    async fn member_health(&self) -> Vec<(SourceId, SourceHealth, u64)> {
        let rt = self.runtime.lock().await;
        let quotas = self.quotas();
        let now = Utc::now();
        self.providers
            .iter()
            .zip(rt.states.iter())
            .zip(quotas.iter())
            .map(|((provider, state), quota)| {
                let id = provider.descriptor().id.clone();
                let health = if let Some(reason) = &state.unavailable {
                    // Needs an operator, not a timer.
                    SourceHealth::Failed {
                        error: reason.clone(),
                        since: state.cooldown_until.unwrap_or(now),
                    }
                } else if let Some(until) = state.cooldown_until.filter(|u| now < *u) {
                    SourceHealth::Stale {
                        since: now,
                        last_error: format!(
                            "sidelined for another {}s",
                            (until - now).num_seconds().max(0)
                        ),
                    }
                } else if let Some(reason) = state.skip_reason(*quota, now) {
                    // Reached only for a spent allowance: expected, recovers on
                    // a schedule, and not a fault.
                    SourceHealth::Degraded {
                        reason,
                        observations: state.observations,
                    }
                } else if state.last_success.is_some() {
                    SourceHealth::Live {
                        observations: state.observations,
                    }
                } else {
                    // Eligible but never called — the primary answered, so this
                    // one has simply not been needed. Not a fault, and not
                    // "live" either.
                    SourceHealth::Unknown
                };
                (id, health, state.observations)
            })
            .collect()
    }

    async fn poll(&self, ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        let quotas = self.quotas();
        let mut rt = self.runtime.lock().await;
        let now = Utc::now();

        for (state, quota) in rt.states.iter_mut().zip(&quotas) {
            state.refresh_window(*quota, now);
        }

        // Reach back up the chain periodically, so a reset allowance is noticed.
        let start_at = if should_retry_promotion(rt.current, rt.last_promotion_attempt, now) {
            rt.last_promotion_attempt = Some(now);
            0
        } else {
            rt.current
        };

        let mut last_error: Option<SourceError> = None;

        for (index, quota) in quotas.iter().enumerate().skip(start_at) {
            if !rt.states[index].is_eligible(*quota, now) {
                continue;
            }

            let provider = self.providers[index].clone();
            let descriptor = provider.descriptor().clone();

            // Let an adaptive driver see how much of its own allowance is left,
            // so it can ask for less rather than be cut off entirely.
            let provider_ctx = PollCtx {
                budget_remaining: quota
                    .map_or(1.0, |q| q.fraction_remaining(rt.states[index].used)),
                ..ctx.clone()
            };

            // Charge the allowance before the call, not after. A request that
            // times out still consumed the upstream's quota, and optimistic
            // accounting is how a chain sails past a limit it thinks it is
            // respecting.
            if let Some(quota) = quota {
                rt.states[index].used = rt.states[index]
                    .used
                    .saturating_add(quota.cost_per_poll);
            }

            // Release the lock across the await: a slow provider must not
            // block status queries for the whole poll.
            drop(rt);
            let result = provider.poll(&provider_ctx).await;
            rt = self.runtime.lock().await;

            match result {
                Ok(observations) => {
                    rt.states[index].cooldown_until = None;
                    rt.states[index].last_success = Some(now);
                    rt.states[index].observations += observations.len() as u64;
                    if rt.current != index {
                        tracing::info!(
                            chain = %self.descriptor.id,
                            provider = %descriptor.id,
                            rank = index,
                            "layer now served by a different provider"
                        );
                    }
                    rt.current = index;
                    return Ok(observations);
                }
                Err(err) => {
                    let resolved = crate::scheduler::resolve_forbidden(err, &descriptor.auth);
                    match cooldown_for(&resolved) {
                        Some(cooldown) => {
                            rt.states[index].cooldown_until = Some(now + cooldown);
                            tracing::warn!(
                                chain = %self.descriptor.id,
                                provider = %descriptor.id,
                                "sidelined for {}s: {resolved}",
                                cooldown.num_seconds()
                            );
                        }
                        None => {
                            rt.states[index].unavailable = Some(resolved.to_string());
                            tracing::warn!(
                                chain = %self.descriptor.id,
                                provider = %descriptor.id,
                                "unavailable: {resolved}"
                            );
                        }
                    }
                    last_error = Some(resolved);
                }
            }
        }

        // Everything is sidelined. Report the most recent real reason rather
        // than a generic "no providers", so the operator sees what actually
        // happened rather than only its consequence.
        Err(last_error.unwrap_or_else(|| {
            SourceError::Transport(format!(
                "every provider for {} is unavailable",
                self.descriptor.id
            ))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_core::source::{AuthRequirement, CostClass, Coverage, LayerId};
    use argus_core::{EntityId, EntityKind, Quality};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A provider whose behaviour the test dictates.
    struct MockProvider {
        descriptor: SourceDescriptor,
        /// Outcomes returned in order; the last repeats once exhausted.
        script: Vec<Result<usize, SourceError>>,
        calls: AtomicUsize,
    }

    impl MockProvider {
        fn new(id: &str, quota: Option<Quota>, script: Vec<Result<usize, SourceError>>) -> Self {
            Self {
                descriptor: SourceDescriptor {
                    id: SourceId::new(id),
                    layer_id: LayerId::new("test-layer"),
                    display_name: format!("Mock {id}"),
                    kind: EntityKind::Aircraft,
                    cadence: Cadence::every(30),
                    coverage: Coverage::Global,
                    auth: AuthRequirement::None,
                    cost: CostClass::Free,
                    attribution: Attribution {
                        provider: id.into(),
                        url: String::new(),
                        license: "None".into(),
                        notice: None,
                    },
                    base_quality: Quality::Live,
                    quota,
                },
                script,
                calls: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Source for MockProvider {
        fn descriptor(&self) -> &SourceDescriptor {
            &self.descriptor
        }

        async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let step = self.script.get(n).or_else(|| self.script.last());
            match step {
                Some(Ok(count)) => Ok((0..*count)
                    .map(|i| {
                        Observation::new(
                            self.descriptor.id.clone(),
                            EntityId::aircraft(format!("mock{i}")),
                            Utc::now(),
                            Quality::Live,
                        )
                        .with_position(argus_core::Position::surface(-97.0, 30.0))
                    })
                    .collect()),
                Some(Err(e)) => Err(match e {
                    SourceError::Auth(m) => SourceError::Auth(m.clone()),
                    SourceError::Transport(m) => SourceError::Transport(m.clone()),
                    SourceError::RateLimited { retry_after } => SourceError::RateLimited {
                        retry_after: *retry_after,
                    },
                    other => SourceError::Transport(other.to_string()),
                }),
                None => Ok(vec![]),
            }
        }
    }

    #[tokio::test]
    async fn a_healthy_primary_serves_and_the_fallback_is_never_called() {
        let primary = Arc::new(MockProvider::new("primary", None, vec![Ok(5)]));
        let fallback = Arc::new(MockProvider::new("fallback", None, vec![Ok(3)]));
        let chain = ProviderChain::new(
            "flights",
            vec![primary.clone(), fallback.clone()],
        );

        let obs = chain.poll(&PollCtx::default()).await.expect("poll");
        assert_eq!(obs.len(), 5);
        assert_eq!(primary.call_count(), 1);
        assert_eq!(fallback.call_count(), 0, "fallback called unnecessarily");
        assert!(!chain.status().await.is_degraded());
    }

    #[tokio::test]
    async fn a_rate_limited_primary_falls_through_within_the_same_poll() {
        let primary = Arc::new(MockProvider::new(
            "primary",
            None,
            vec![Err(SourceError::RateLimited { retry_after: None })],
        ));
        let fallback = Arc::new(MockProvider::new("fallback", None, vec![Ok(7)]));
        let chain = ProviderChain::new("flights", vec![primary.clone(), fallback.clone()]);

        // The caller gets data, not an error — that is the whole point.
        let obs = chain.poll(&PollCtx::default()).await.expect("poll");
        assert_eq!(obs.len(), 7);
        assert_eq!(fallback.call_count(), 1);

        let status = chain.status().await;
        assert!(status.is_degraded());
        assert_eq!(status.serving.as_str(), "fallback");
        assert_eq!(status.skipped.len(), 1);
        assert!(status.skipped[0].1.contains("cooling down"), "{:?}", status.skipped);
    }

    #[tokio::test]
    async fn a_sidelined_primary_is_not_retried_on_the_next_poll() {
        // Re-probing every poll would spend the scarce primary allowance on
        // liveness checks.
        let primary = Arc::new(MockProvider::new(
            "primary",
            None,
            vec![Err(SourceError::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(900)),
            })],
        ));
        let fallback = Arc::new(MockProvider::new("fallback", None, vec![Ok(2)]));
        let chain = ProviderChain::new("flights", vec![primary.clone(), fallback.clone()]);

        for _ in 0..4 {
            chain.poll(&PollCtx::default()).await.expect("poll");
        }
        assert_eq!(primary.call_count(), 1, "primary was re-probed while cooling");
        assert_eq!(fallback.call_count(), 4);
    }

    #[tokio::test]
    async fn an_exhausted_allowance_moves_to_the_fallback() {
        // Two polls allowed, then the chain must move on by itself.
        let primary = Arc::new(MockProvider::new(
            "primary",
            Some(Quota {
                limit: 2,
                window: std::time::Duration::from_secs(86_400),
                cost_per_poll: 1,
            }),
            vec![Ok(4)],
        ));
        let fallback = Arc::new(MockProvider::new("fallback", None, vec![Ok(1)]));
        let chain = ProviderChain::new("flights", vec![primary.clone(), fallback.clone()]);

        assert_eq!(chain.poll(&PollCtx::default()).await.unwrap().len(), 4);
        assert_eq!(chain.poll(&PollCtx::default()).await.unwrap().len(), 4);
        // Allowance spent; the third poll must come from the fallback.
        assert_eq!(chain.poll(&PollCtx::default()).await.unwrap().len(), 1);
        assert_eq!(primary.call_count(), 2, "primary polled past its allowance");

        let status = chain.status().await;
        assert!(status.skipped[0].1.contains("allowance spent"), "{:?}", status.skipped);
    }

    #[tokio::test]
    async fn the_allowance_is_charged_even_when_the_call_fails() {
        // A request that times out still consumed the upstream's quota.
        // Optimistic accounting is how a chain sails past a limit it believes
        // it is respecting.
        let primary = Arc::new(MockProvider::new(
            "primary",
            Some(Quota {
                limit: 3,
                window: std::time::Duration::from_secs(86_400),
                cost_per_poll: 1,
            }),
            vec![Err(SourceError::Transport("timeout".into()))],
        ));
        let fallback = Arc::new(MockProvider::new("fallback", None, vec![Ok(1)]));
        let chain = ProviderChain::new("flights", vec![primary.clone(), fallback.clone()]);

        chain.poll(&PollCtx::default()).await.expect("poll");
        let states = chain.provider_states().await;
        assert_eq!(states[0].1.used, 1, "a failed call did not consume allowance");
    }

    #[tokio::test]
    async fn a_rejected_credential_permanently_sidelines_that_provider() {
        let primary = Arc::new(MockProvider::new(
            "primary",
            None,
            vec![Err(SourceError::Auth("401 unauthorized".into()))],
        ));
        let fallback = Arc::new(MockProvider::new("fallback", None, vec![Ok(9)]));
        let chain = ProviderChain::new("flights", vec![primary.clone(), fallback.clone()]);

        chain.poll(&PollCtx::default()).await.expect("poll");
        let states = chain.provider_states().await;
        assert!(states[0].1.unavailable.is_some());
        // No cooldown was set — this needs an operator, not a timer.
        assert!(states[0].1.cooldown_until.is_none());

        // And it is never called again, however long the chain runs.
        for _ in 0..5 {
            chain.poll(&PollCtx::default()).await.expect("poll");
        }
        assert_eq!(primary.call_count(), 1);
    }

    #[tokio::test]
    async fn every_provider_failing_surfaces_the_real_reason() {
        let primary = Arc::new(MockProvider::new(
            "primary",
            None,
            vec![Err(SourceError::Transport("dns failure".into()))],
        ));
        let fallback = Arc::new(MockProvider::new(
            "fallback",
            None,
            vec![Err(SourceError::Transport("connection refused".into()))],
        ));
        let chain = ProviderChain::new("flights", vec![primary, fallback]);

        let err = chain.poll(&PollCtx::default()).await.unwrap_err();
        // The last real error, not a generic "nothing available" — the operator
        // needs to see what actually happened, not only its consequence.
        assert!(
            err.to_string().contains("connection refused"),
            "unhelpful error: {err}"
        );
    }

    #[tokio::test]
    async fn a_chain_reports_itself_as_keyless_when_any_member_is() {
        // Reporting the primary's requirement would tell a user a key is
        // required when a keyless fallback is standing right behind it.
        let keyed = Arc::new(MockProvider::new("keyed", None, vec![Ok(1)]));
        let mut keyed_desc = keyed.descriptor.clone();
        keyed_desc.auth = AuthRequirement::Required {
            config_key: "api_key".into(),
        };
        let keyed = Arc::new(MockProvider {
            descriptor: keyed_desc,
            script: vec![Ok(1)],
            calls: AtomicUsize::new(0),
        });
        let keyless = Arc::new(MockProvider::new("keyless", None, vec![Ok(1)]));

        let chain = ProviderChain::new("flights", vec![keyed, keyless]);
        assert!(matches!(
            chain.descriptor().auth,
            AuthRequirement::None
        ));
    }

    fn at(mins: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000 + mins * 60, 0).unwrap()
    }

    fn states(n: usize, now: DateTime<Utc>) -> Vec<ProviderState> {
        (0..n).map(|_| ProviderState::new(now)).collect()
    }

    #[test]
    fn the_primary_is_chosen_when_everything_is_healthy() {
        let quotas = vec![None, None, None];
        let s = states(3, at(0));
        assert_eq!(select_provider(&quotas, &s, at(0)), Some(0));
    }

    #[test]
    fn a_cooling_provider_is_skipped_for_the_next_one() {
        let quotas = vec![None, None, None];
        let mut s = states(3, at(0));
        s[0].cooldown_until = Some(at(15));
        assert_eq!(select_provider(&quotas, &s, at(5)), Some(1));
        // And is picked back up once the cooldown lapses.
        assert_eq!(select_provider(&quotas, &s, at(20)), Some(0));
    }

    #[test]
    fn a_spent_allowance_sidelines_a_provider_without_marking_it_broken() {
        // The distinction the whole design turns on: spent is expected and
        // recovers on a schedule; broken does not.
        let quotas = vec![Some(Quota::daily(400)), None];
        let mut s = states(2, at(0));
        s[0].used = 400;
        assert_eq!(select_provider(&quotas, &s, at(0)), Some(1));
        assert!(s[0].unavailable.is_none());
        let reason = s[0].skip_reason(quotas[0], at(0)).unwrap();
        assert!(reason.contains("allowance spent"), "got: {reason}");
        assert!(reason.contains("400/400"), "got: {reason}");
    }

    #[test]
    fn an_allowance_resets_when_its_window_rolls_over() {
        let quota = Some(Quota::daily(400));
        let mut state = ProviderState::new(at(0));
        state.used = 400;
        assert!(!state.is_eligible(quota, at(60)));

        state.refresh_window(quota, at(60));
        assert_eq!(state.used, 400, "window rolled over early");

        // One day later.
        state.refresh_window(quota, at(24 * 60 + 1));
        assert_eq!(state.used, 0);
        assert!(state.is_eligible(quota, at(24 * 60 + 1)));
    }

    #[test]
    fn a_rejected_credential_is_never_retried_on_a_timer() {
        let quotas = vec![None, None];
        let mut s = states(2, at(0));
        s[0].unavailable = Some("401".into());
        assert_eq!(select_provider(&quotas, &s, at(0)), Some(1));
        // Still sidelined a week later — this needs an operator, not a timer.
        assert_eq!(select_provider(&quotas, &s, at(7 * 24 * 60)), Some(1));
    }

    #[test]
    fn no_provider_is_available_when_all_are_sidelined() {
        let quotas = vec![None, None];
        let mut s = states(2, at(0));
        s[0].unavailable = Some("401".into());
        s[1].cooldown_until = Some(at(30));
        assert_eq!(select_provider(&quotas, &s, at(10)), None);
    }

    #[test]
    fn failures_map_to_the_right_kind_of_sidelining() {
        // Terminal failures get no cooldown; they are marked unavailable.
        assert!(cooldown_for(&SourceError::Auth("401".into())).is_none());
        assert!(cooldown_for(&SourceError::HardwareMissing("sdr".into())).is_none());
        assert!(cooldown_for(&SourceError::Forbidden("403".into())).is_none());

        // Transient ones get a short one.
        assert_eq!(
            cooldown_for(&SourceError::Transport("timeout".into())),
            Some(TRANSPORT_COOLDOWN)
        );
        // And an upstream that says how long to wait is believed.
        assert_eq!(
            cooldown_for(&SourceError::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(90))
            }),
            Some(Duration::seconds(90))
        );
        // Or given a sensible default when it does not.
        assert_eq!(
            cooldown_for(&SourceError::RateLimited { retry_after: None }),
            Some(DEFAULT_RATE_LIMIT_COOLDOWN)
        );
    }

    #[test]
    fn a_chain_on_its_primary_never_tries_to_promote() {
        assert!(!should_retry_promotion(0, None, at(0)));
        assert!(!should_retry_promotion(0, Some(at(0)), at(1000)));
    }

    #[test]
    fn a_chain_on_a_fallback_reaches_back_up_periodically() {
        // Without this a daily quota that reset at midnight would never be
        // noticed and the layer would sit on its worst provider forever.
        assert!(should_retry_promotion(2, None, at(0)));
        assert!(!should_retry_promotion(2, Some(at(0)), at(10)));
        assert!(should_retry_promotion(2, Some(at(0)), at(31)));
    }

    #[test]
    fn quota_arithmetic_handles_multi_unit_polls() {
        // OpenSky charges more credits for a global query than a bounded one;
        // a chain that assumes one-per-poll sails past the real limit.
        let q = Quota {
            limit: 400,
            window: std::time::Duration::from_secs(86_400),
            cost_per_poll: 4,
        };
        assert_eq!(q.polls_remaining(0), 100);
        assert_eq!(q.polls_remaining(398), 0);
        assert_eq!(q.polls_remaining(1000), 0, "overspend must not wrap");
        assert!((q.fraction_remaining(200) - 0.5).abs() < 1e-9);
        assert!((q.fraction_remaining(1000) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn a_zero_cost_quota_never_blocks() {
        let q = Quota {
            limit: 10,
            window: std::time::Duration::from_secs(60),
            cost_per_poll: 0,
        };
        assert_eq!(q.polls_remaining(999), u32::MAX);
    }

    #[test]
    fn chain_status_reports_degradation_and_why() {
        let status = ChainStatus {
            serving: SourceId::new("adsb-lol"),
            serving_rank: 1,
            skipped: vec![(
                SourceId::new("opensky"),
                "allowance spent (400/400), resets in 240m".into(),
            )],
        };
        assert!(status.is_degraded());
        match status.health(1234) {
            SourceHealth::Degraded { reason, observations } => {
                assert_eq!(observations, 1234);
                assert!(reason.contains("adsb-lol"), "got: {reason}");
                assert!(reason.contains("allowance spent"), "got: {reason}");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }

        let healthy = ChainStatus {
            serving: SourceId::new("opensky"),
            serving_rank: 0,
            skipped: vec![],
        };
        assert!(!healthy.is_degraded());
        assert!(matches!(healthy.health(10), SourceHealth::Live { .. }));
    }
}
