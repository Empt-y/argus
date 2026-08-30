//! A cache for reference geography.
//!
//! Some feeds describe *where* by pointing at a shape rather than carrying one.
//! An NWS alert names the forecast zones it covers; a grid event names a bidding
//! zone; a volcanic advisory names an airspace region. The shape is static, the
//! event is not, and re-fetching a county boundary every sixty seconds because
//! a Small Craft Advisory is still in force would be both slow and rude.
//!
//! This is deliberately *not* the store. Drivers do not get a database handle —
//! that separation is what keeps a driver testable and stops one from inventing
//! its own persistence. What a driver gets is this: a keyed geometry cache with
//! no query language, no history and no notion of an observation. The store
//! happens to implement it, and a `HashMap` implements it just as well in a
//! test.

use geo_types::Geometry;

/// Somewhere to keep geography that is fetched once and reused.
#[async_trait::async_trait]
pub trait GeometryCache: Send + Sync {
    /// Look one up. A miss is `None`; a cache that is broken should log and
    /// return `None` rather than erroring, because a driver that cannot reach
    /// its cache should still fetch and still work.
    async fn get(&self, key: &str) -> Option<Geometry<f64>>;

    /// Remember one. Failure is deliberately not reported: the caller already
    /// has the geometry it needs, and a driver is not the place to decide what
    /// to do about a database that will not write.
    async fn put(&self, key: &str, geometry: &Geometry<f64>);
}

/// An in-memory cache, for tests and for a driver running without a store.
#[derive(Debug, Default)]
pub struct MemoryGeometryCache {
    entries: tokio::sync::RwLock<std::collections::HashMap<String, Geometry<f64>>>,
}

impl MemoryGeometryCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.entries.read().await.is_empty()
    }
}

#[async_trait::async_trait]
impl GeometryCache for MemoryGeometryCache {
    async fn get(&self, key: &str) -> Option<Geometry<f64>> {
        self.entries.read().await.get(key).cloned()
    }

    async fn put(&self, key: &str, geometry: &Geometry<f64>) {
        self.entries
            .write()
            .await
            .insert(key.to_string(), geometry.clone());
    }
}

/// What Argus already knows it tracks.
///
/// A failover provider has a bootstrapping problem the primary does not: the
/// primary defines the catalogue (CelesTrak does it by group), and a fallback
/// that cannot name the same objects can only ask for everything or nothing.
/// The stored element set answers that once the primary has run — but the case
/// that matters most is a cold start *during* an outage, where it has not.
///
/// Argus has been recording these objects for as long as it has been running,
/// so the answer is already on disk in the observation history. This is the
/// interface for asking it. It is the project's own premise turned around: the
/// history is not just something to replay, it is what lets the system carry on
/// when a live feed goes away.
#[async_trait::async_trait]
pub trait TrackedCatalogue: Send + Sync {
    /// NORAD numbers of every satellite Argus holds, sorted. Empty when there
    /// is no history yet, which is a real answer — a genuinely fresh install
    /// has nothing to fall back to and must wait for the primary.
    async fn tracked_norad_ids(&self) -> Vec<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{Coord, LineString, Polygon};

    fn square() -> Geometry<f64> {
        Geometry::Polygon(Polygon::new(
            LineString(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: 1.0, y: 0.0 },
                Coord { x: 1.0, y: 1.0 },
                Coord { x: 0.0, y: 0.0 },
            ]),
            vec![],
        ))
    }

    #[tokio::test]
    async fn a_stored_geometry_comes_back() {
        let cache = MemoryGeometryCache::new();
        assert!(cache.get("MDC031").await.is_none());
        cache.put("MDC031", &square()).await;
        assert_eq!(cache.get("MDC031").await, Some(square()));
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn keys_do_not_collide_across_zones() {
        let cache = MemoryGeometryCache::new();
        cache.put("a", &square()).await;
        assert!(cache.get("b").await.is_none());
    }
}
