//! The ingest runtime: one supervised task per source, writing into the DVR.

use crate::scheduler::{PollOutcome, SchedulerConfig, SourceState, auth_state, next_delay, poll_once, startup_jitter};
use argus_core::geo::BoundingBox;
use argus_core::source::{Cadence, Coverage, Source, SourceError};
use argus_store::Store;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How often to re-measure the store against its budget.
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(300);

/// Owns the running ingest tasks.
pub struct Runtime {
    store: Store,
    config: SchedulerConfig,
    sources: Vec<Arc<dyn Source>>,
    /// Areas polled at full cadence. Bounded sources are asked about each in
    /// turn; global ones ignore them.
    aois: Vec<BoundingBox>,
    /// Flipped by the disk guard. Read per poll rather than captured at
    /// startup, so degradation takes effect on a running daemon instead of
    /// waiting for a restart that may never come.
    degraded: Arc<AtomicBool>,
    cancel: CancellationToken,
}

impl Runtime {
    pub fn new(store: Store, config: SchedulerConfig) -> Self {
        Self {
            store,
            config,
            sources: Vec::new(),
            aois: Vec::new(),
            degraded: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
        }
    }

    /// Whether the disk guard has tripped.
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    /// Declare the areas bounded sources should cover.
    ///
    /// A bounded source with no areas declared would poll nothing at all, so
    /// an empty list falls back to a global box at the source's own cadence —
    /// which the radius cap then clamps to something the endpoint accepts.
    /// Silently ingesting nothing would be the worse failure.
    pub fn with_aois(mut self, aois: Vec<BoundingBox>) -> Self {
        self.aois = aois;
        self
    }

    pub fn register(&mut self, source: Arc<dyn Source>) {
        self.sources.push(source);
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Register every source in the database, then spawn a polling task each.
    ///
    /// Registration happens before any polling so that the first write already
    /// has a `sources` row to resolve its `layer_id` against — otherwise the
    /// first batch from each feed lands with the source id standing in for the
    /// layer, and the clients briefly show a layer that does not exist.
    pub async fn run(
        self,
        credentials: &CredentialResolver,
        budget_bytes: u64,
        warn_fraction: f64,
    ) -> Result<(), argus_store::StoreError> {
        self.spawn_disk_guard(budget_bytes, warn_fraction);

        for source in &self.sources {
            self.store.register_source(source.descriptor()).await?;
            // Members carry their own rows: observations reference the provider
            // that actually produced them, and per-provider health is what
            // makes "serving via the fallback" inspectable rather than folklore.
            for member in source.members() {
                self.store.register_source(member).await?;
            }
        }

        let count = self.sources.len();
        let mut handles = Vec::with_capacity(count);

        for (index, source) in self.sources.into_iter().enumerate() {
            let descriptor = source.descriptor().clone();
            let store = self.store.clone();
            let config = self.config.clone();
            let cancel = self.cancel.clone();
            let has_credential = credentials.has_for(&descriptor);
            let aois = self.aois.clone();
            let degraded = self.degraded.clone();

            handles.push(tokio::spawn(async move {
                // A source that cannot run for want of a key or a dongle is
                // recorded once, honestly, and then left alone. It is not a
                // failure and must not be retried in a loop.
                if let Some(blocked) = auth_state(&descriptor.auth, has_credential) {
                    tracing::info!(
                        source = %descriptor.id,
                        "source is configured off: {blocked:?}"
                    );
                    let _ = store
                        .update_source_health(&descriptor.id, &blocked, 0)
                        .await;
                    return;
                }

                let base_interval = match descriptor.cadence {
                    Cadence::Fixed { every } => every,
                    Cadence::Adaptive { ceiling, .. } => ceiling,
                    Cadence::Streaming | Cadence::Static => Duration::from_secs(60),
                };
                let jitter = startup_jitter(base_interval, index, count);
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(jitter) => {}
                }

                let mut state = SourceState::default();
                loop {
                    let (outcome, retry_after) =
                        poll_and_store(&store, &source, &mut state, &aois).await;

                    if !state.should_continue() {
                        tracing::warn!(
                            source = %descriptor.id,
                            "source disabled: {:?}", state.health
                        );
                        return;
                    }

                    // Read the guard per poll, not once at startup.
                    let effective = SchedulerConfig {
                        aoi_only: degraded.load(Ordering::Relaxed),
                        ..config.clone()
                    };
                    let delay = next_delay(
                        descriptor.cadence,
                        &state,
                        &effective,
                        1.0,
                        retry_after,
                    );
                    tracing::debug!(
                        source = %descriptor.id,
                        accepted = outcome.accepted,
                        rejected = outcome.rejected,
                        next_in_s = delay.as_secs(),
                        "polled"
                    );

                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }
        Ok(())
    }
}

/// One poll, stored, with health folded in. Returns what happened and any
/// upstream-supplied retry hint.
async fn poll_and_store(
    store: &Store,
    source: &Arc<dyn Source>,
    state: &mut SourceState,
    aois: &[BoundingBox],
) -> (PollOutcome, Option<Duration>) {
    let descriptor = source.descriptor();

    match poll_scoped(source, aois).await {
        Ok(observations) => {
            // Only feeds describing current state have a meaningful lag, and
            // the freshest reading is what measures it — the oldest item in a
            // rolling window says nothing about how current the feed is.
            let lag = if descriptor.kind.reports_current_state() {
                observations
                    .iter()
                    .map(argus_core::Observation::lag)
                    .min()
                    .unwrap_or_else(chrono::Duration::zero)
            } else {
                chrono::Duration::zero()
            };

            let written = match store.write_observations(&observations).await {
                Ok(w) => w,
                Err(err) => {
                    // A store failure is ours, not the source's — so say so.
                    // Folding it into the normal rejected count would report
                    // "N of M readings failed validation", blaming perfectly
                    // good upstream data for a local fault and sending whoever
                    // is debugging in exactly the wrong direction.
                    tracing::error!(source = %descriptor.id, "failed to store batch: {err}");
                    let health = argus_core::SourceHealth::Degraded {
                        reason: format!("upstream healthy; store rejected the batch: {err}"),
                        observations: 0,
                    };
                    let _ = store
                        .update_source_health(&descriptor.id, &health, 0)
                        .await;
                    state.health = health;
                    return (PollOutcome::default(), None);
                }
            };

            // Duplicates count as accepted: a quiet event feed returns the same
            // rows every poll and is perfectly healthy. Only genuinely
            // unusable readings are rejected.
            let outcome = PollOutcome {
                accepted: written.accepted(),
                rejected: written.skipped,
                lag,
            };
            state.record(Ok(outcome));
            // Only newly stored rows advance the lifetime counter; re-polled
            // duplicates would otherwise inflate it without bound.
            let _ = store
                .update_source_health(&descriptor.id, &state.health, written.inserted)
                .await;

            // Composites publish their members' state too, so a chain that has
            // fallen back says which provider is carrying it and why.
            for (member_id, member_health, member_observations) in source.member_health().await {
                let _ = store
                    .set_source_health(&member_id, &member_health, member_observations)
                    .await;
            }
            (outcome, None)
        }
        Err(err) => {
            let retry_after = match &err {
                SourceError::RateLimited { retry_after } => *retry_after,
                _ => None,
            };
            tracing::warn!(source = %descriptor.id, "poll failed: {err}");
            state.record(Err(&err));
            let _ = store
                .update_source_health(&descriptor.id, &state.health, 0)
                .await;
            (PollOutcome::default(), retry_after)
        }
    }
}

impl Runtime {
    /// Watch the store against its budget and degrade capture rather than
    /// filling the filesystem.
    ///
    /// Degrading means slowing the cadence outside the declared areas of
    /// interest, not stopping. A daemon that goes silent when disk runs short
    /// is worse than one that keeps a thinner record: the whole point is that
    /// history exists, and the areas the operator actually watches keep full
    /// fidelity either way.
    fn spawn_disk_guard(&self, budget_bytes: u64, warn_fraction: f64) {
        let store = self.store.clone();
        let degraded = self.degraded.clone();
        let cancel = self.cancel.clone();
        let threshold = (budget_bytes as f64 * warn_fraction.clamp(0.0, 1.0)) as u64;

        tokio::spawn(async move {
            loop {
                match store.total_bytes().await {
                    Ok(bytes) => {
                        let used = bytes.max(0) as u64;
                        let over = used > threshold;
                        // Only log on a transition; a five-minute heartbeat
                        // saying "still fine" is noise that trains people to
                        // ignore the log.
                        if degraded.swap(over, Ordering::Relaxed) != over {
                            if over {
                                tracing::warn!(
                                    used_mb = used / 1_048_576,
                                    threshold_mb = threshold / 1_048_576,
                                    "store over threshold — slowing capture outside areas of interest"
                                );
                            } else {
                                tracing::info!(
                                    used_mb = used / 1_048_576,
                                    "store back under threshold — resuming full capture"
                                );
                            }
                        }
                    }
                    // A failed measurement must not silently disarm the guard.
                    // Leave the current state alone and say so.
                    Err(err) => tracing::warn!("disk guard could not measure the store: {err}"),
                }

                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(DISK_CHECK_INTERVAL) => {}
                }
            }
        });
    }
}

/// Poll a source across whatever areas it needs asking about.
///
/// Global sources answer in one call. Bounded ones must be asked per area, and
/// the results are merged — the same aircraft seen from two overlapping areas
/// collapses to one entity downstream, because the entity key is the aircraft's
/// own address rather than anything about the request.
///
/// A partial failure is not a failure: if one area answers and another does
/// not, the data that did arrive is kept. Returning an error would throw away
/// good observations because a neighbouring box timed out.
async fn poll_scoped(
    source: &Arc<dyn Source>,
    aois: &[BoundingBox],
) -> Result<Vec<argus_core::Observation>, SourceError> {
    let descriptor = source.descriptor();
    if !matches!(descriptor.coverage, Coverage::Bounded) {
        return poll_once(source, &argus_core::PollCtx::default()).await;
    }

    let areas: Vec<BoundingBox> = if aois.is_empty() {
        vec![BoundingBox::GLOBAL]
    } else {
        // A wrapped box cannot be expressed as one query; split before asking.
        aois.iter().flat_map(BoundingBox::split_at_antimeridian).collect()
    };

    let mut merged = Vec::new();
    let mut last_error = None;
    for bbox in areas {
        let ctx = argus_core::PollCtx {
            bbox: Some(bbox),
            ..Default::default()
        };
        match poll_once(source, &ctx).await {
            Ok(mut obs) => merged.append(&mut obs),
            Err(err) => {
                tracing::warn!(source = %descriptor.id, "area poll failed: {err}");
                last_error = Some(err);
            }
        }
    }

    if merged.is_empty()
        && let Some(err) = last_error
    {
        return Err(err);
    }
    Ok(merged)
}

/// Looks up whether a source has the credential its `AuthRequirement` names.
#[derive(Debug, Default, Clone)]
pub struct CredentialResolver {
    /// `source_id -> { config_key -> value }`.
    by_source: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

impl CredentialResolver {
    pub fn new(
        by_source: std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<String, String>,
        >,
    ) -> Self {
        Self { by_source }
    }

    pub fn get(&self, source_id: &str, config_key: &str) -> Option<&str> {
        self.by_source
            .get(source_id)?
            .get(config_key)
            .map(String::as_str)
            // An empty string in config means "not set". Treating it as a
            // present credential produces a 401 loop that looks like an
            // upstream outage instead of a configuration mistake.
            .filter(|v| !v.trim().is_empty())
    }

    /// Whether this source has what it needs to run.
    pub fn has_for(&self, descriptor: &argus_core::SourceDescriptor) -> bool {
        use argus_core::AuthRequirement as A;
        let id = descriptor.id.as_str();
        match &descriptor.auth {
            A::None | A::Optional { .. } => true,
            A::Required { config_key } => self.get(id, config_key).is_some(),
            A::OAuth {
                client_id_key,
                client_secret_key,
            } => {
                self.get(id, client_id_key).is_some()
                    && self.get(id, client_secret_key).is_some()
            }
            A::Login {
                identity_key,
                password_key,
            } => {
                self.get(id, identity_key).is_some() && self.get(id, password_key).is_some()
            }
            // Hardware presence is probed by the driver at startup, not
            // configured. Until Phase 10 wires that up, treat it as absent.
            A::Hardware { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_core::source::{
        Attribution, AuthRequirement, CostClass, Coverage, LayerId, SourceDescriptor, SourceId,
    };
    use argus_core::{EntityKind, Quality};

    fn descriptor(auth: AuthRequirement) -> SourceDescriptor {
        SourceDescriptor {
            id: SourceId::new("test"),
            layer_id: LayerId::new("test"),
            display_name: "Test".into(),
            kind: EntityKind::Event,
            cadence: Cadence::every(60),
            coverage: Coverage::Global,
            auth,
            cost: CostClass::Free,
            attribution: Attribution {
                provider: "Test".into(),
                url: String::new(),
                license: "None".into(),
                notice: None,
            },
            base_quality: Quality::Live,
            quota: None,
        }
    }

    fn resolver(pairs: &[(&str, &str)]) -> CredentialResolver {
        let mut inner = std::collections::BTreeMap::new();
        let mut keys = std::collections::BTreeMap::new();
        for (k, v) in pairs {
            keys.insert((*k).to_string(), (*v).to_string());
        }
        inner.insert("test".to_string(), keys);
        CredentialResolver::new(inner)
    }

    #[test]
    fn a_blank_credential_counts_as_absent() {
        // The config template ships commented-out keys; someone uncommenting
        // one and leaving it empty must get "key required", not a 401 loop
        // that looks like an upstream outage.
        let r = resolver(&[("api_key", "   ")]);
        assert!(!r.has_for(&descriptor(AuthRequirement::Required {
            config_key: "api_key".into()
        })));
        let r = resolver(&[("api_key", "real-value")]);
        assert!(r.has_for(&descriptor(AuthRequirement::Required {
            config_key: "api_key".into()
        })));
    }

    #[test]
    fn keyless_and_optional_sources_always_have_what_they_need() {
        let empty = CredentialResolver::default();
        assert!(empty.has_for(&descriptor(AuthRequirement::None)));
        assert!(empty.has_for(&descriptor(AuthRequirement::Optional {
            config_key: "client_id".into()
        })));
    }

    #[test]
    fn oauth_needs_both_halves() {
        let auth = AuthRequirement::OAuth {
            client_id_key: "client_id".into(),
            client_secret_key: "client_secret".into(),
        };
        assert!(!resolver(&[("client_id", "abc")]).has_for(&descriptor(auth.clone())));
        assert!(
            resolver(&[("client_id", "abc"), ("client_secret", "xyz")])
                .has_for(&descriptor(auth))
        );
    }

    #[test]
    fn hardware_sources_are_absent_until_probed() {
        let auth = AuthRequirement::Hardware {
            description: "rtl-sdr".into(),
        };
        assert!(!CredentialResolver::default().has_for(&descriptor(auth)));
    }
}
