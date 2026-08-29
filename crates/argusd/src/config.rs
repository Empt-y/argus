//! Daemon configuration.
//!
//! Loaded from `/etc/argus/argus.toml` (or `$ARGUS_CONFIG`). Secrets live here
//! and nowhere else: no provider credential is ever compiled in, and none is
//! ever sent to a client. The two exceptions are stated explicitly in
//! [`ClientKeys`], because they have to reach the browser to work at all.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub client_keys: ClientKeys,
    #[serde(default)]
    pub client: ClientConfig,
    /// Areas where capture runs at full cadence.
    #[serde(default, rename = "aoi")]
    pub aois: Vec<AoiConfig>,
    /// Per-source settings and credentials, keyed by source id.
    #[serde(default)]
    pub sources: std::collections::BTreeMap<String, SourceConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Bind address. Defaults to loopback — a LAN-visible daemon brokers every
    /// configured credential to anyone who can reach it, so widening this is an
    /// explicit choice the operator makes, never a default.
    pub bind: String,
    /// Extra origins allowed to call the API (the web client in development).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Whether a request from this machine may skip the device token.
    ///
    /// Loopback-exempt is the practical default: a process that can reach
    /// 127.0.0.1 on this box can already read the config file that holds every
    /// credential, so demanding a token from it protects nothing while making
    /// `curl` on the server tiresome. Set this to `required` for a deployment
    /// where other people have shell accounts.
    #[serde(default)]
    pub auth: AuthPolicy,
    /// How clients reach this server: the LAN or Tailscale address, used in the
    /// tile URLs of the generated style and in the pairing QR.
    ///
    /// Defaults to the bind address, which is right for loopback development
    /// and wrong the moment a phone is involved — `127.0.0.1` on a phone is the
    /// phone. Set it whenever `bind` is not what a client would type.
    #[serde(default)]
    pub public_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthPolicy {
    /// Loopback callers skip the token; everyone else needs one.
    #[default]
    LoopbackExempt,
    /// Every caller needs a token, including this machine.
    Required,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8787".into(),
            allowed_origins: Vec::new(),
            auth: AuthPolicy::LoopbackExempt,
            public_url: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

const fn default_max_connections() -> u32 {
    16
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    /// Multiplier applied to every source's cadence *outside* the declared
    /// AOIs. Global aircraft capture at full rate is ~57M rows/day; at 4× it is
    /// a quarter of that, and the AOIs still get full fidelity where it matters.
    pub global_cadence_scale: f64,
    /// Stop growing past this. The guard degrades to AOI-only capture rather
    /// than filling the filesystem — running out of disk must not be one of the
    /// ways this daemon can fail.
    pub disk_budget_gb: u64,
    /// Fraction of the budget at which to start degrading, `0.0..=1.0`.
    pub disk_warn_fraction: f64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            global_cadence_scale: 4.0,
            disk_budget_gb: 80,
            disk_warn_fraction: 0.85,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    /// How long raw observations are kept before the rollups are all that
    /// remain. Applied to the hypertable policy at startup, so changing this is
    /// a config edit and a restart, not a migration.
    #[serde(with = "humantime_serde_days")]
    pub raw: Duration,
    #[serde(with = "humantime_serde_days")]
    pub tracks: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            raw: Duration::from_secs(7 * 86_400),
            tracks: Duration::from_secs(90 * 86_400),
        }
    }
}

/// The only credentials that legitimately reach a browser.
///
/// Google Maps and Cesium ion keys are used directly by client-side CesiumJS
/// for the photorealistic tileset; there is no way to broker them server-side
/// without proxying every tile. Restrict both at the provider (HTTP referrer /
/// token scope) — that restriction, not secrecy, is what protects them.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientKeys {
    #[serde(default)]
    pub google_maps_api_key: Option<String>,
    #[serde(default)]
    pub cesium_ion_token: Option<String>,
}

/// Client-side scene configuration that is *not* secret.
///
/// Kept apart from [`ClientKeys`] on purpose. Those two values are credentials
/// that happen to have to reach a browser; these are just settings, and the
/// distinction is worth preserving in the type so nobody later assumes
/// everything the client is told is sensitive.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// A 3D Tiles tileset of buildings, or `None` to draw none.
    ///
    /// Defaults to the Re:Earth Buildings community service, which is global,
    /// ODbL, and needs no key. It publishes no SLA, so point this at a mirror
    /// rather than depending on it — that the URL is configurable at all is the
    /// whole reason this setting exists.
    #[serde(default)]
    pub buildings_tileset_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AoiConfig {
    pub name: String,
    /// `[west, south, east, north]` in degrees.
    pub bbox: [f64; 4],
}

impl AoiConfig {
    /// Used by the ingest scheduler to decide whether a poll is inside an area
    /// of interest and therefore runs at full cadence.
    #[allow(dead_code, reason = "wired up by the scheduler in phase 1")]
    pub fn to_bbox(&self) -> argus_core::BoundingBox {
        argus_core::BoundingBox::new(self.bbox[0], self.bbox[1], self.bbox[2], self.bbox[3])
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Credential values, keyed by the `config_key` the driver's
    /// `AuthRequirement` names.
    #[serde(default)]
    pub credentials: std::collections::BTreeMap<String, String>,
    /// Override the driver's declared cadence, in seconds.
    #[serde(default)]
    pub cadence_secs: Option<u64>,
}

const fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Catch the mistakes that would otherwise surface hours later as confusing
    /// runtime behaviour rather than as a startup error.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.database.url.trim().is_empty() {
            return Err(ConfigError::Invalid("database.url is empty".into()));
        }
        if self.capture.global_cadence_scale < 1.0 {
            return Err(ConfigError::Invalid(
                "capture.global_cadence_scale must be >= 1.0; values below 1 would poll \
                 *faster* outside your areas of interest than inside them"
                    .into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.capture.disk_warn_fraction) {
            return Err(ConfigError::Invalid(
                "capture.disk_warn_fraction must be between 0.0 and 1.0".into(),
            ));
        }
        if self.retention.tracks < self.retention.raw {
            return Err(ConfigError::Invalid(
                "retention.tracks must be >= retention.raw; the rollup cannot be dropped \
                 before the data it summarises"
                    .into(),
            ));
        }
        for aoi in &self.aois {
            let [w, s, e, n] = aoi.bbox;
            if !(-90.0..=90.0).contains(&s) || !(-90.0..=90.0).contains(&n) || s >= n {
                return Err(ConfigError::Invalid(format!(
                    "aoi '{}' has invalid latitudes (south={s}, north={n})",
                    aoi.name
                )));
            }
            if !(-180.0..=180.0).contains(&w) || !(-180.0..=180.0).contains(&e) {
                return Err(ConfigError::Invalid(format!(
                    "aoi '{}' has invalid longitudes (west={w}, east={e})",
                    aoi.name
                )));
            }
        }
        Ok(())
    }

    /// Whether the daemon is reachable from beyond this machine, which is what
    /// makes credential brokering and rate limiting matter.
    pub fn is_externally_bound(&self) -> bool {
        !(self.server.bind.starts_with("127.") || self.server.bind.starts_with("[::1]"))
    }
}

impl ServerConfig {
    /// The URL clients should use, falling back to the bind address.
    pub fn public_url(&self) -> String {
        self.public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", self.bind))
    }
}

/// Durations in this config are written the way an operator thinks about them
/// ("7 days", "90 days"), not as second counts.
mod humantime_serde_days {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{} days", d.as_secs() / 86_400))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let raw = String::deserialize(d)?;
        let text = raw.trim();
        let (count, unit) = text
            .split_once(char::is_whitespace)
            .ok_or_else(|| serde::de::Error::custom(format!("expected '<n> <unit>', got '{raw}'")))?;
        let n: u64 = count
            .parse()
            .map_err(|_| serde::de::Error::custom(format!("'{count}' is not a number")))?;
        let secs = match unit.trim().trim_end_matches('s') {
            "day" => 86_400,
            "hour" => 3_600,
            "minute" => 60,
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown unit '{other}'; use hours, days or minutes"
                )));
            }
        };
        Ok(Duration::from_secs(n * secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Config {
        toml::from_str(
            r#"
            [database]
            url = "postgres://argus@localhost/argus"
            "#,
        )
        .expect("minimal config should parse")
    }

    #[test]
    fn a_minimal_config_gets_safe_defaults() {
        let c = minimal();
        assert!(c.validate().is_ok());
        // Loopback by default: a LAN-visible daemon must be an explicit choice.
        assert_eq!(c.server.bind, "127.0.0.1:8787");
        assert!(!c.is_externally_bound());
        assert_eq!(c.retention.raw.as_secs(), 7 * 86_400);
        assert_eq!(c.retention.tracks.as_secs(), 90 * 86_400);
    }

    #[test]
    fn binding_beyond_loopback_is_detected() {
        let mut c = minimal();
        c.server.bind = "0.0.0.0:8787".into();
        assert!(c.is_externally_bound());
        c.server.bind = "[::1]:8787".into();
        assert!(!c.is_externally_bound());
    }

    #[test]
    fn durations_parse_the_way_an_operator_writes_them() {
        let c: Config = toml::from_str(
            r#"
            [database]
            url = "postgres://x"
            [retention]
            raw = "3 days"
            tracks = "30 days"
            "#,
        )
        .unwrap();
        assert_eq!(c.retention.raw.as_secs(), 3 * 86_400);
        assert_eq!(c.retention.tracks.as_secs(), 30 * 86_400);
    }

    #[test]
    fn rollups_cannot_be_dropped_before_their_source_data() {
        let c: Config = toml::from_str(
            r#"
            [database]
            url = "postgres://x"
            [retention]
            raw = "30 days"
            tracks = "7 days"
            "#,
        )
        .unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn a_cadence_scale_below_one_is_rejected() {
        // Below 1.0 would poll harder outside the AOIs than inside them —
        // exactly backwards, and easy to typo.
        let mut c = minimal();
        c.capture.global_cadence_scale = 0.5;
        assert!(c.validate().is_err());
    }

    #[test]
    fn nonsense_areas_of_interest_are_rejected_at_startup() {
        let mut c = minimal();
        c.aois = vec![AoiConfig {
            name: "inverted".into(),
            bbox: [-2.0, 52.0, 0.5, 51.0], // south above north
        }];
        assert!(c.validate().is_err());

        c.aois = vec![AoiConfig {
            name: "off-planet".into(),
            bbox: [-200.0, 51.0, 0.5, 52.0],
        }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn antimeridian_areas_of_interest_are_accepted() {
        // west > east is a legal wrapped box, not an error.
        let mut c = minimal();
        c.aois = vec![AoiConfig {
            name: "fiji".into(),
            bbox: [170.0, -20.0, -170.0, -10.0],
        }];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        // A typo'd key that parses fine but does nothing is the worst kind of
        // config bug: everything looks configured and nothing is.
        let result: Result<Config, _> = toml::from_str(
            r#"
            [database]
            url = "postgres://x"
            [capture]
            disk_budget_gigabytes = 40
            "#,
        );
        assert!(result.is_err());
    }
}
