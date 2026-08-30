//! The orbital catalogue, held between runs and shared between providers.
//!
//! Two things live here because two drivers need them to agree.
//!
//! **Persistence.** Element sets stay usable for about a week, so an upstream
//! outage of a few hours should be invisible — there is nothing to fetch that
//! we do not already have. Keeping them only in memory broke that in the least
//! obvious way: the layer survived CelesTrak being down and then died on the
//! next restart, discarding a catalogue still good for days.
//!
//! **The catalogue definition.** CelesTrak decides *which* objects Argus tracks,
//! by group — stations, visual, GNSS, weather, science, the geostationary belt.
//! No other provider has those groupings, so a failover has no way to ask for
//! "the same satellites" except by asking for them individually, by number. The
//! stored set is where those numbers come from. That makes the fallback follow
//! the primary's curation instead of inventing its own, and it means the
//! fallback is only ever asked for objects Argus already decided it wanted.
//!
//! It also means the failover is useless until the primary has succeeded once.
//! That is the honest ordering: a fallback for an outage, not a way to start
//! from nothing.

use chrono::{DateTime, Duration, Utc};
use std::path::{Path, PathBuf};

/// A catalogue as fetched, with the moment it was fetched.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ElementCache {
    pub fetched_at: DateTime<Utc>,
    pub elements: Vec<sgp4::Elements>,
}

impl ElementCache {
    pub fn age(&self) -> Duration {
        Utc::now() - self.fetched_at
    }

    /// The NORAD numbers in this catalogue, sorted, for a provider that can
    /// only be asked by number.
    pub fn norad_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.elements.iter().map(|e| e.norad_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

/// Where the catalogue lives on disk. `None` disables persistence entirely,
/// which is what keeps tests and any embedded use hermetic.
#[derive(Clone, Default)]
pub struct ElementStore {
    path: Option<PathBuf>,
}

impl ElementStore {
    pub fn disabled() -> Self {
        Self { path: None }
    }

    /// Shared by every satellites provider, so the fallback reads the same
    /// catalogue the primary wrote.
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: Some(dir.join("orbital-elements.json")),
        }
    }

    /// The stored catalogue, if it is still worth propagating.
    pub async fn load(&self, max_age: Duration) -> Option<ElementCache> {
        let path = self.path.as_ref()?;
        let text = tokio::fs::read_to_string(path).await.ok()?;
        let cache: ElementCache = serde_json::from_str(&text).ok()?;
        if cache.elements.is_empty() {
            return None;
        }
        if cache.age() > max_age {
            tracing::warn!(
                "stored elements are older than {} days, ignoring",
                max_age.num_days()
            );
            return None;
        }
        Some(cache)
    }

    pub async fn save(&self, cache: &ElementCache) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        // Write beside and rename, so a daemon killed mid-write leaves the
        // previous good catalogue rather than a truncated one.
        let tmp = path.with_extension("json.tmp");
        match serde_json::to_vec(cache) {
            Ok(bytes) => {
                if tokio::fs::write(&tmp, &bytes).await.is_ok() {
                    let _ = tokio::fs::rename(&tmp, path).await;
                }
            }
            Err(err) => tracing::warn!("could not serialise elements: {err}"),
        }
    }
}
