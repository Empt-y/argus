//! Satellites from Space-Track, as the failover behind CelesTrak.
//!
//! Space-Track is the 18th Space Defense Squadron's own catalogue — the place
//! CelesTrak itself gets its data. That makes it the right thing to fall back
//! to: not a mirror of the service that is down, but the source upstream of it,
//! on entirely separate infrastructure.
//!
//! **There is no API key.** Space-Track issues none; the documentation does not
//! mention the concept. Authentication is an account login POSTed to
//! `/ajaxauth/login`, which answers with an encrypted session cookie. So this
//! driver is the only one in Argus holding an operator's actual credentials
//! rather than a scoped machine token, which is why [`AuthRequirement::Login`]
//! exists as its own thing and why the values are never logged.
//!
//! **It follows CelesTrak's curation rather than inventing its own.** Argus
//! tracks a curated ~1,100 objects chosen by CelesTrak group — stations,
//! visual, GNSS, weather, science, the geostationary belt — and Space-Track has
//! no equivalent grouping. Asking it for everything on orbit would be ~26,000
//! objects and a DVR budget gone in days. So the object list comes from the
//! stored catalogue the primary wrote, and this driver asks for exactly those,
//! by number, in one request. The consequence worth stating: this source cannot
//! bootstrap. Until CelesTrak has succeeded once there is nothing to ask for,
//! which is the honest shape of a failover.
//!
//! **Their rate limits are enforced with account suspension**, so the numbers
//! matter: under 30 requests a minute and 300 an hour overall, and the GP class
//! specifically no more than once an hour. One bulk query per element refresh
//! sits far inside that, and the driver only runs at all when the primary is
//! failing.

use crate::http::HttpClient;
use crate::sources::celestrak::propagate_all;
use crate::sources::elements::{ElementCache, ElementStore};
use argus_core::entity::{EntityKind, Observation, Quality};
use argus_core::source::{
    Attribution, AuthRequirement, Cadence, CostClass, Coverage, LayerId, PollCtx, Quota, Source,
    SourceDescriptor, SourceError, SourceId,
};
use chrono::{Duration, Utc};
use tokio::sync::RwLock;

const LOGIN_URL: &str = "https://www.space-track.org/ajaxauth/login";
const BASE: &str = "https://www.space-track.org/basicspacedata/query/class/gp";

/// Config keys the operator fills in. Their own login, not a scoped token.
pub const IDENTITY_KEY: &str = "identity";
pub const PASSWORD_KEY: &str = "password";

/// Matches the primary's refresh interval. Their guidance for the GP class is
/// once an hour; six is comfortably inside it and there is no accuracy to gain
/// from asking more often.
const ELEMENTS_TTL: Duration = Duration::hours(6);
const MAX_ELEMENT_AGE: Duration = Duration::days(7);
const PROPAGATE_CADENCE_SECS: u64 = 60;

/// How many object numbers to put in one URL. Their retrieval guidance is
/// explicit that many objects belong in one comma-delimited query rather than
/// one request each; this only splits to keep the URL itself sane.
const IDS_PER_REQUEST: usize = 500;

pub struct SpaceTrackSatellites {
    descriptor: SourceDescriptor,
    http: HttpClient,
    store: ElementStore,
    cache: RwLock<Option<ElementCache>>,
    logged_in: RwLock<bool>,
    /// Injected at construction rather than read per poll, because `PollCtx`
    /// deliberately carries no secrets: a credential that travels with every
    /// poll context is a credential that ends up in a log line eventually.
    identity: Option<String>,
    password: Option<String>,
    /// Where to look when there is no stored catalogue yet.
    catalogue: Option<std::sync::Arc<dyn argus_core::TrackedCatalogue>>,
}

impl SpaceTrackSatellites {
    pub fn new(http: HttpClient, store: ElementStore) -> Self {
        Self {
            descriptor: SourceDescriptor {
                id: SourceId::new("spacetrack"),
                layer_id: LayerId::new("satellites"),
                display_name: "Satellites (Space-Track, failover)".into(),
                kind: EntityKind::Satellite,
                cadence: Cadence::every(PROPAGATE_CADENCE_SECS),
                coverage: Coverage::Global,
                auth: AuthRequirement::Login {
                    identity_key: IDENTITY_KEY.into(),
                    password_key: PASSWORD_KEY.into(),
                },
                cost: CostClass::Free,
                attribution: Attribution {
                    provider: "Space-Track.org (18th Space Defense Squadron)".into(),
                    url: "https://www.space-track.org/".into(),
                    license: "US Government data; see Space-Track user agreement".into(),
                    notice: Some("Orbital data courtesy of Space-Track.org".into()),
                },
                // Propagated, never measured — same as the primary.
                base_quality: Quality::Modeled,
                // Their published ceiling is 300 requests an hour. Declared so
                // the chain charges for it like any other allowance; exceeding
                // it costs the account, not just the request.
                quota: Some(Quota {
                    limit: 300,
                    window: std::time::Duration::from_secs(3_600),
                    cost_per_poll: 1,
                }),
            },
            http,
            store,
            cache: RwLock::new(None),
            logged_in: RwLock::new(false),
            identity: None,
            password: None,
            catalogue: None,
        }
    }

    /// Let the driver fall back to Argus's own history for the object list,
    /// for the case where the primary has never succeeded on this machine.
    #[must_use]
    pub fn with_catalogue(
        mut self,
        catalogue: std::sync::Arc<dyn argus_core::TrackedCatalogue>,
    ) -> Self {
        self.catalogue = Some(catalogue);
        self
    }

    /// Supply the operator's Space-Track login. Without it the source reports
    /// `KeyRequired`, which is a configured state and not a fault.
    #[must_use]
    pub fn with_login(mut self, identity: Option<String>, password: Option<String>) -> Self {
        self.identity = identity;
        self.password = password;
        self
    }

    /// Exchange the login for a session cookie, once, lazily.
    async fn ensure_session(&self) -> Result<(), SourceError> {
        if *self.logged_in.read().await {
            return Ok(());
        }
        let identity = self
            .identity
            .as_deref()
            .ok_or_else(|| SourceError::Auth(format!("{IDENTITY_KEY} not configured")))?;
        let password = self
            .password
            .as_deref()
            .ok_or_else(|| SourceError::Auth(format!("{PASSWORD_KEY} not configured")))?;

        self.http
            .post_form(LOGIN_URL, &[("identity", identity), ("password", password)])
            .await?;
        *self.logged_in.write().await = true;
        tracing::info!("spacetrack: session established");
        Ok(())
    }

    /// The objects to ask for: whatever the primary decided Argus tracks.
    async fn wanted_ids(&self) -> Result<Vec<u64>, SourceError> {
        // The stored element set first: it is what the primary most recently
        // decided, and it is exact.
        if let Some(stored) = self.store.load(MAX_ELEMENT_AGE).await {
            let ids = stored.norad_ids();
            if !ids.is_empty() {
                return Ok(ids);
            }
        }

        // Then Argus's own history. Anything in the entities table is something
        // this deployment has been tracking, which is the same question asked a
        // different way.
        if let Some(catalogue) = self.catalogue.as_ref() {
            let ids = catalogue.tracked_norad_ids().await;
            if !ids.is_empty() {
                tracing::info!(
                    objects = ids.len(),
                    "spacetrack: no stored elements; following the tracked catalogue from history"
                );
                return Ok(ids);
            }
        }

        Err(SourceError::Decode(
            "no catalogue to follow: Space-Track is a failover and cannot choose which \
             objects to track on its own, and this deployment has no satellite history yet"
                .into(),
        ))
    }
}

/// The GP query for a batch of object numbers.
///
/// `decay_date/null-val` and `epoch/>now-10` are their own recommendation:
/// together they exclude re-entered objects and stale element sets, so what
/// comes back is only what can actually be propagated.
pub fn gp_url(ids: &[u64]) -> String {
    let list = ids
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("{BASE}/NORAD_CAT_ID/{list}/decay_date/null-val/epoch/%3Enow-10/format/json")
}

#[async_trait::async_trait]
impl Source for SpaceTrackSatellites {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    async fn poll(&self, _ctx: &PollCtx) -> Result<Vec<Observation>, SourceError> {
        if let Some(cache) = self.cache.read().await.as_ref()
            && cache.age() < ELEMENTS_TTL
        {
            return Ok(propagate_all(
                &cache.elements,
                Utc::now(),
                &self.descriptor.id,
            ));
        }

        let ids = self.wanted_ids().await?;
        self.ensure_session().await?;

        let mut fetched: Vec<sgp4::Elements> = Vec::new();
        for batch in ids.chunks(IDS_PER_REQUEST) {
            match self
                .http
                .get_json::<Vec<sgp4::Elements>>(&gp_url(batch))
                .await
            {
                Ok(mut batch) => fetched.append(&mut batch),
                // A session can expire between polls. One retry, then give up
                // and let the chain decide — a login loop against a service
                // that suspends accounts is the wrong thing to build.
                Err(SourceError::Auth(msg)) => {
                    *self.logged_in.write().await = false;
                    return Err(SourceError::Auth(msg));
                }
                Err(err) => return Err(err),
            }
        }

        if fetched.is_empty() {
            return Err(SourceError::Decode(
                "Space-Track returned no element sets for the tracked catalogue".into(),
            ));
        }

        let fresh = ElementCache {
            fetched_at: Utc::now(),
            elements: fetched,
        };
        // Written back to the same store: whichever provider last succeeded is
        // the one holding the layer up, and the other should inherit it.
        self.store.save(&fresh).await;
        let observations = propagate_all(&fresh.elements, Utc::now(), &self.descriptor.id);
        *self.cache.write().await = Some(fresh);
        Ok(observations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_format_is_the_same_omm_the_primary_speaks() {
        // Worth pinning: Space-Track sends NORAD_CAT_ID as a *string* where
        // CelesTrak sends a number. That it still deserialises is the reason
        // this driver needs no conversion layer at all, and it would be an
        // easy thing to break silently.
        let raw = include_str!("../../fixtures/spacetrack_gp.json");
        let parsed: Vec<sgp4::Elements> =
            serde_json::from_str(raw).expect("Space-Track GP parses as OMM elements");
        assert_eq!(parsed.len(), 3);
        assert!(parsed.iter().any(|e| e.norad_id == 25544), "ISS present");
    }

    #[test]
    fn the_fixture_propagates() {
        let raw = include_str!("../../fixtures/spacetrack_gp.json");
        let parsed: Vec<sgp4::Elements> = serde_json::from_str(raw).unwrap();
        // At the fixture's own clock, not the wall clock: a captured element
        // set ages past MAX_ELEMENT_AGE and then propagates to nothing, which
        // is correct behaviour and a useless test.
        let at = parsed
            .iter()
            .map(|e| e.datetime.and_utc())
            .max()
            .expect("fixture is not empty")
            + chrono::Duration::hours(1);
        let obs = propagate_all(&parsed, at, &SourceId::new("spacetrack"));
        assert_eq!(obs.len(), parsed.len(), "every Space-Track element must propagate");
    }

    #[test]
    fn many_objects_go_in_one_request() {
        // Their retrieval guidance is explicit that hundreds of single-object
        // queries is the thing not to do.
        let url = gp_url(&[25544, 20580, 48274]);
        assert!(url.contains("/NORAD_CAT_ID/20580,25544,48274/") || url.contains("25544,20580,48274"));
        assert!(url.contains("decay_date/null-val"), "exclude re-entered objects");
        assert!(url.contains("epoch/%3Enow-10"), "exclude stale elements");
    }
}
