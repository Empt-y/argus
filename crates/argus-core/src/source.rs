//! The driver contract every feed implements, and the honest health model that
//! goes with it.
//!
//! A source declares what it is and what it costs; the scheduler in
//! `argus-ingest` decides when to call it. Drivers do not own their own timers,
//! retry logic, caching or budget accounting — centralising that is the whole
//! reason this trait is narrow.

use crate::entity::{EntityKind, Observation};
use crate::geo::BoundingBox;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable identifier for a driver, e.g. `opensky`, `adsb-local`, `usgs-quakes`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(String);

impl SourceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier for a user-facing layer. Several sources may feed one layer —
/// OpenSky and a local dongle both feed `flights`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LayerId(String);

impl LayerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a source needs before it can run at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthRequirement {
    /// Works with no credentials.
    None,
    /// Runs without a key, but a key raises limits or fidelity. The layer is
    /// still fully usable keyless — this must never be reported as unavailable.
    Optional { config_key: String },
    /// Cannot run without a key. The layer reports `KeyRequired`, which is a
    /// *configured* terminal state and not a failure.
    Required { config_key: String },
    /// Needs an OAuth client credential pair.
    OAuth {
        client_id_key: String,
        client_secret_key: String,
    },
    /// Needs local hardware (an SDR) or a local helper process.
    Hardware { description: String },
}

/// What calling this source costs, so the scheduler can be careful with the
/// expensive ones and relaxed with the free ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    /// Public, unmetered. Still rate-limited out of politeness.
    Free,
    /// Free allowance then billed, or a hard quota that must be rationed across
    /// the day.
    Metered,
    /// Local hardware; costs nothing but is capacity-limited.
    Local,
}

/// How often a source should be polled.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Cadence {
    /// Poll on a fixed interval.
    Fixed { every: std::time::Duration },
    /// Poll between these bounds, tightening when the source is cheap and
    /// healthy and backing off as its remaining budget drains. The scheduler
    /// owns the interpolation.
    Adaptive {
        floor: std::time::Duration,
        ceiling: std::time::Duration,
    },
    /// The source pushes; there is nothing to poll. Implement [`StreamSource`].
    Streaming,
    /// Fetched once at startup and then only on explicit refresh — bundled
    /// datasets and slow-moving geography.
    Static,
}

impl Cadence {
    pub const fn every(secs: u64) -> Self {
        Self::Fixed {
            every: std::time::Duration::from_secs(secs),
        }
    }
}

/// Where a source can answer about. Global feeds are polled once; bounded feeds
/// are polled per area of interest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Coverage {
    /// One request returns the whole world.
    Global,
    /// Must be asked about a bounding box at a time.
    Bounded,
    /// Only ever covers this fixed region — a city camera network, a national
    /// rail API.
    Fixed { bbox: BoundingBox },
}

/// Licence and credit for a source. Required by several upstream terms (ODbL,
/// CC BY-NC-SA, NASA) and surfaced verbatim in both clients' attribution panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    pub provider: String,
    pub url: String,
    pub license: String,
    /// Exact credit line to display, when the licence dictates specific wording.
    pub notice: Option<String>,
}

/// A provider's usage allowance, as the provider itself defines it.
///
/// Declared per driver rather than configured centrally, because these are
/// facts about the upstream — OpenSky's anonymous tier is 400 credits a day
/// whatever Argus thinks — and a chain can only ration what it can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    /// Units available per window.
    pub limit: u32,
    /// How long the window lasts before the allowance resets.
    pub window: std::time::Duration,
    /// Units consumed by one poll. Not always one: OpenSky charges more credits
    /// for a global state vector than for a bounded one, and a driver that
    /// undercounts will sail past its allowance and start collecting 429s.
    pub cost_per_poll: u32,
}

impl Quota {
    /// A simple daily allowance costing one unit per poll.
    pub const fn daily(limit: u32) -> Self {
        Self {
            limit,
            window: std::time::Duration::from_secs(86_400),
            cost_per_poll: 1,
        }
    }

    /// How many polls remain from a given consumption.
    pub const fn polls_remaining(&self, used: u32) -> u32 {
        if self.cost_per_poll == 0 {
            return u32::MAX;
        }
        self.limit.saturating_sub(used) / self.cost_per_poll
    }

    /// Fraction of the allowance still unspent, `0.0..=1.0`. Feeds
    /// [`PollCtx::budget_remaining`] so adaptive drivers can ease off before
    /// they are cut off entirely.
    pub fn fraction_remaining(&self, used: u32) -> f64 {
        if self.limit == 0 {
            return 0.0;
        }
        f64::from(self.limit.saturating_sub(used)) / f64::from(self.limit)
    }
}

/// Everything static about a driver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceDescriptor {
    pub id: SourceId,
    pub layer_id: LayerId,
    pub display_name: String,
    pub kind: EntityKind,
    pub cadence: Cadence,
    pub coverage: Coverage,
    pub auth: AuthRequirement,
    pub cost: CostClass,
    pub attribution: Attribution,
    /// Baseline quality for readings from this source. A driver may downgrade a
    /// individual observation, but never upgrade past this — a source that can
    /// only ever estimate must not be able to claim `Live` for one reading.
    pub base_quality: crate::entity::Quality,
    /// The provider's own usage allowance, where it publishes one. `None` means
    /// unmetered — a public feed with no documented cap, or local hardware.
    #[serde(default)]
    pub quota: Option<Quota>,
}

/// Live state of a source, recomputed after every poll and surfaced to clients.
///
/// The distinction that matters: `KeyRequired` and `HardwareAbsent` are
/// *configured* states, not failures. A user who has deliberately not set a
/// FIRMS key should see "key required", never "load failed" — conflating the two
/// is how a UI starts crying wolf and users stop reading it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SourceHealth {
    /// Answering, and current.
    Live { observations: u64 },
    /// Answering, but lagging further behind than this source normally does.
    Delayed { lag: Duration, observations: u64 },
    /// Not answering; we are serving the last good response.
    Stale {
        since: DateTime<Utc>,
        last_error: String,
    },
    /// Answering, but partially — some sub-requests failed, or the budget
    /// governor has throttled it below its normal cadence.
    Degraded { reason: String, observations: u64 },
    /// Configured off: the required credential is absent.
    KeyRequired { config_key: String },
    /// Configured off: the required local hardware is not present.
    HardwareAbsent { description: String },
    /// Has never successfully answered since startup. Deliberately distinct
    /// from `Live { observations: 0 }`: "I asked and the answer was genuinely
    /// nothing" and "I have never had an answer" must not render identically,
    /// or an empty sky looks the same as a broken feed.
    Unknown,
    /// Failing, with no cached value to fall back on.
    Failed { error: String, since: DateTime<Utc> },
}

impl SourceHealth {
    /// Whether this state is the operator's choice rather than a malfunction.
    /// Drives whether the UI shows a neutral chip or an alarming one.
    pub const fn is_configured_off(&self) -> bool {
        matches!(
            self,
            Self::KeyRequired { .. } | Self::HardwareAbsent { .. }
        )
    }

    /// Whether a human should be told something is wrong.
    pub const fn is_problem(&self) -> bool {
        matches!(self, Self::Stale { .. } | Self::Failed { .. })
    }
}

/// Errors a driver may return. The scheduler treats these differently:
/// `Auth` and `HardwareMissing` disable the source rather than retrying it,
/// while `Transport` and `RateLimited` back off and try again.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("upstream transport failure: {0}")]
    Transport(String),
    #[error("upstream returned malformed data: {0}")]
    Decode(String),
    #[error("credential missing or rejected: {0}")]
    Auth(String),
    /// HTTP 403. Deliberately distinct from [`SourceError::Auth`]: for a source
    /// that sends no credential a 403 cannot mean "your key is wrong", so it is
    /// almost always throttling or IP-level blocking — which retrying *does*
    /// eventually fix. Only the scheduler knows whether the source authenticates,
    /// so only the scheduler can resolve which it is.
    #[error("forbidden by upstream: {0}")]
    Forbidden(String),
    #[error("rate limited{}", .retry_after.map(|d| format!(", retry after {}s", d.as_secs())).unwrap_or_default())]
    RateLimited {
        retry_after: Option<std::time::Duration>,
    },
    #[error("local hardware unavailable: {0}")]
    HardwareMissing(String),
    #[error("response exceeded the {limit} byte cap")]
    ResponseTooLarge { limit: usize },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl SourceError {
    /// Whether retrying could plausibly succeed without operator action.
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport(_)
                | Self::RateLimited { .. }
                | Self::Decode(_)
                | Self::Forbidden(_)
        )
    }
}

/// Context handed to a driver for one poll.
#[derive(Debug, Clone)]
pub struct PollCtx {
    /// The area to ask about, for [`Coverage::Bounded`] sources. `None` for
    /// global ones.
    pub bbox: Option<BoundingBox>,
    /// When the previous successful poll happened, for sources that support
    /// incremental fetches.
    pub since: Option<DateTime<Utc>>,
    /// Remaining budget for metered sources, `0.0..=1.0`. Drivers may use this
    /// to request less data rather than being throttled entirely.
    pub budget_remaining: f64,
}

impl Default for PollCtx {
    fn default() -> Self {
        Self {
            bbox: None,
            since: None,
            budget_remaining: 1.0,
        }
    }
}

/// A feed that Argus asks for data on a schedule.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn descriptor(&self) -> &SourceDescriptor;

    /// Fetch and normalise. Implementations must not sleep, retry or cache —
    /// the scheduler owns all three.
    async fn poll(&self, ctx: &PollCtx) -> Result<Vec<Observation>, SourceError>;

    /// Providers this source delegates to, if it is a composite such as a
    /// failover chain.
    ///
    /// Observations carry the id of the provider that actually produced them,
    /// not the composite's — provenance would be lost otherwise, and the layer
    /// could not say which upstream a given track came from. So every member
    /// must be registered alongside the composite, and this is how the runtime
    /// discovers them.
    fn members(&self) -> Vec<&SourceDescriptor> {
        Vec::new()
    }

    /// Current health of each member, for composites.
    ///
    /// Without this a chain is opaque: the layer reports healthy and there is
    /// no way to see that it has quietly fallen back, which provider is
    /// carrying it, or how long the primary has left. That state is exactly
    /// what an operator needs when deciding whether a key is worth getting.
    async fn member_health(&self) -> Vec<(SourceId, SourceHealth, u64)> {
        Vec::new()
    }
}

/// A feed that pushes to us instead: an AIS websocket, a local SDR, the GDELT
/// firehose. The scheduler supervises the task and restarts it with backoff.
#[async_trait::async_trait]
pub trait StreamSource: Send + Sync {
    fn descriptor(&self) -> &SourceDescriptor;

    /// Run until the stream ends or errors. Send observations as they arrive;
    /// returning `Ok(())` means a clean end-of-stream and will be restarted.
    async fn run(
        &self,
        tx: tokio::sync::mpsc::Sender<Observation>,
    ) -> Result<(), SourceError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_off_states_are_not_reported_as_problems() {
        // The whole point: a deliberately absent key must not look like a fault.
        let key_required = SourceHealth::KeyRequired {
            config_key: "firms_map_key".into(),
        };
        assert!(key_required.is_configured_off());
        assert!(!key_required.is_problem());

        let absent = SourceHealth::HardwareAbsent {
            description: "rtl-sdr".into(),
        };
        assert!(absent.is_configured_off());
        assert!(!absent.is_problem());
    }

    #[test]
    fn genuine_failures_are_reported_as_problems() {
        let failed = SourceHealth::Failed {
            error: "connection refused".into(),
            since: Utc::now(),
        };
        assert!(failed.is_problem());
        assert!(!failed.is_configured_off());
    }

    #[test]
    fn unknown_is_distinct_from_an_empty_live_answer() {
        // "never answered" and "answered, nothing there" must not be the same
        // value, or a broken feed renders as an empty sky.
        assert_ne!(SourceHealth::Unknown, SourceHealth::Live { observations: 0 });
        assert!(!SourceHealth::Unknown.is_problem());
    }

    #[test]
    fn auth_errors_are_not_retryable_but_transport_is() {
        assert!(!SourceError::Auth("bad key".into()).is_retryable());
        assert!(!SourceError::HardwareMissing("no dongle".into()).is_retryable());
        assert!(SourceError::Transport("timeout".into()).is_retryable());
        assert!(SourceError::RateLimited { retry_after: None }.is_retryable());
    }
}
