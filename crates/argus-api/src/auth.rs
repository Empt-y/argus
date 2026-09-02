//! Device authentication and QR pairing.
//!
//! The shape of the problem: this daemon holds every provider credential the
//! operator configured, and it is reachable from a phone on the LAN and from
//! anywhere over Tailscale. So a token is not about secrecy between the phone
//! and the server — it is about a lost phone being revocable, and about a guest
//! on the WiFi not inheriting the operator's API keys.
//!
//! Pairing codes live in memory and nowhere else. That is deliberate: a code is
//! valid for minutes, and surviving a restart would be a liability rather than
//! a feature — a daemon that has been restarted has, by definition, just shown
//! its console to somebody.

use crate::error::ApiError;
use argus_store::model::DeviceRow;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use std::net::SocketAddr;
use std::sync::Mutex;

/// How long a pairing code stays usable. Long enough to walk to the phone,
/// short enough that a code left on a screen is not a standing invitation.
pub const PAIRING_TTL: Duration = Duration::minutes(10);

/// When a request must carry a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Every request needs a valid device token. The right setting for anything
    /// reachable beyond this machine.
    Required,
    /// Requests from loopback skip the check; everyone else needs a token.
    /// Makes `curl` on the server itself work without ceremony, which is worth
    /// a great deal during development and costs nothing: a process that can
    /// bind loopback here can already read the config file.
    LoopbackExempt,
}

/// Who is making a request.
#[derive(Debug, Clone)]
pub enum Caller {
    Device(Box<DeviceRow>),
    /// A loopback caller under [`AuthMode::LoopbackExempt`].
    Local,
}

impl Caller {
    pub fn name(&self) -> &str {
        match self {
            Self::Device(d) => &d.name,
            Self::Local => "localhost",
        }
    }

    /// A stable identifier for delivery tracking.
    ///
    /// The device *id*, not its name: `alerts.delivered_to` is how a phone
    /// knows what it missed, and two tablets both called "kitchen" sharing a
    /// delivery record would mean one of them silently never being told.
    pub fn delivery_id(&self) -> String {
        match self {
            Self::Device(d) => d.device_id.to_string(),
            Self::Local => "localhost".to_string(),
        }
    }

    /// Whether this caller may change server state — pair or revoke devices,
    /// write geofences. Read-only devices exist so a wall display can be handed
    /// a token without handing it the ability to unpair the phone.
    pub fn can_write(&self) -> bool {
        match self {
            Self::Device(d) => d.scopes.iter().any(|s| s == "write" || s == "admin"),
            Self::Local => true,
        }
    }
}

/// One outstanding pairing code.
#[derive(Debug, Clone)]
struct PendingCode {
    code: String,
    expires_at: DateTime<Utc>,
}

/// The set of pairing codes currently offered, held in memory.
#[derive(Debug, Default)]
pub struct PairingCodes {
    pending: Mutex<Vec<PendingCode>>,
}

impl PairingCodes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a code and return it. Short — it is read off a screen or scanned,
    /// and its security comes from a ten-minute life and single use, not from
    /// length.
    pub fn issue(&self) -> String {
        let mut bytes = [0u8; 6];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let code = hex::encode(bytes);
        let now = Utc::now();
        let mut pending = self.pending.lock().expect("pairing lock poisoned");
        pending.retain(|c| c.expires_at > now);
        pending.push(PendingCode {
            code: code.clone(),
            expires_at: now + PAIRING_TTL,
        });
        code
    }

    /// Spend a code. Single use: a code that has paired one device must not
    /// pair a second, or a photograph of the screen is a permanent key.
    pub fn redeem(&self, code: &str) -> bool {
        let now = Utc::now();
        let mut pending = self.pending.lock().expect("pairing lock poisoned");
        pending.retain(|c| c.expires_at > now);
        if let Some(idx) = pending.iter().position(|c| c.code == code) {
            pending.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn outstanding(&self) -> usize {
        let now = Utc::now();
        let mut pending = self.pending.lock().expect("pairing lock poisoned");
        pending.retain(|c| c.expires_at > now);
        pending.len()
    }
}

/// Pull a bearer token out of a request.
///
/// Both the `Authorization` header and a `token` query parameter are accepted,
/// and the query parameter is not a shortcut. MapLibre requests tiles through
/// the browser's own fetch for `<img>`-like loads and MapLibre Native does the
/// same on Android; neither offers a place to attach a header per tile without
/// reimplementing the transport. A token in the query string is the standard
/// answer, and its cost — appearing in access logs — is one this daemon does not
/// pay, because it writes no access log.
fn extract_token(request: &Request) -> Option<String> {
    if let Some(value) = request.headers().get(axum::http::header::AUTHORIZATION)
        && let Ok(text) = value.to_str()
        && let Some(token) = text.strip_prefix("Bearer ")
    {
        return Some(token.trim().to_string());
    }
    request.uri().query().and_then(|q| {
        q.split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == "token")
            .map(|(_, v)| v.to_string())
    })
}

/// Reject unauthenticated requests, and hand the rest a [`Caller`].
pub async fn require_device(
    State(state): State<crate::ApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let caller = match extract_token(&request) {
        Some(token) => {
            let device = state.store.authenticate(&token).await?.ok_or_else(|| {
                // Deliberately not distinguishing "no such token" from
                // "revoked": both are "you are not welcome", and telling them
                // apart only helps someone probing.
                ApiError::Unauthorized("unknown or revoked device token".into())
            })?;
            Caller::Device(Box::new(device))
        }
        None
            if state.config.auth == AuthMode::LoopbackExempt
                && peer.ip().is_loopback()
                && !was_forwarded(&request) =>
        {
            Caller::Local
        }
        None => {
            return Err(ApiError::Unauthorized(
                "no bearer token; pair this client first (POST /v1/pair)".into(),
            ));
        }
    };

    request.extensions_mut().insert(caller);
    Ok(next.run(request).await)
}

/// Whether this request reached us through a reverse proxy.
///
/// This closes a hole that only appears once something is put in front of the
/// daemon. `loopback_exempt` justifies itself on the grounds that a process
/// able to reach loopback here could already read the config file — true of a
/// local `curl`, and false the moment a proxy terminates connections on
/// loopback and forwards other people's. `tailscale serve` does exactly that:
/// with it running, every device on the tailnet arrived at `127.0.0.1` and was
/// waved through, and `GET /v1/layers` answered 200 with no token from another
/// machine entirely.
///
/// Measured rather than assumed — `tailscale serve` sets `X-Forwarded-Proto`,
/// which is already how [`crate::routes::tiles`] knows to write `https` into
/// the style it generates. A request carrying any of these did not originate
/// where its socket says it did, so the exemption must not apply to it.
///
/// Forging a header cannot gain anything: it only ever removes an exemption.
fn was_forwarded(request: &Request) -> bool {
    const PROXY_HEADERS: [&str; 4] = [
        "x-forwarded-for",
        "x-forwarded-proto",
        "x-forwarded-host",
        "forwarded",
    ];
    PROXY_HEADERS
        .iter()
        .any(|name| request.headers().contains_key(*name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forwarded_request_is_never_treated_as_local() {
        // The tailnet case: tailscale serve terminates on loopback and proxies
        // the whole tailnet through it, so the peer address is a lie.
        let mut forwarded = Request::new(axum::body::Body::empty());
        forwarded
            .headers_mut()
            .insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(was_forwarded(&forwarded));

        let mut for_header = Request::new(axum::body::Body::empty());
        for_header
            .headers_mut()
            .insert("x-forwarded-for", "100.64.0.9".parse().unwrap());
        assert!(was_forwarded(&for_header));

        // A local curl carries none of them and keeps the exemption, which is
        // the whole convenience the mode exists for.
        assert!(!was_forwarded(&Request::new(axum::body::Body::empty())));
    }

    #[test]
    fn a_pairing_code_works_exactly_once() {
        let codes = PairingCodes::new();
        let code = codes.issue();
        assert!(codes.redeem(&code));
        assert!(
            !codes.redeem(&code),
            "a photographed code must not pair a second device"
        );
    }

    #[test]
    fn an_unknown_code_is_refused() {
        let codes = PairingCodes::new();
        codes.issue();
        assert!(!codes.redeem("000000000000"));
        assert_eq!(codes.outstanding(), 1, "a failed attempt spends nothing");
    }

    #[test]
    fn codes_are_distinct_and_hex() {
        let codes = PairingCodes::new();
        let a = codes.issue();
        let b = codes.issue();
        assert_ne!(a, b);
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(codes.outstanding(), 2);
    }

    #[test]
    fn a_read_only_device_cannot_write() {
        let device = |scopes: &[&str]| {
            Caller::Device(Box::new(DeviceRow {
                device_id: uuid::Uuid::nil(),
                name: "phone".into(),
                scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
                created_at: Utc::now(),
                last_seen_at: None,
                revoked_at: None,
            }))
        };
        assert!(!device(&["read"]).can_write());
        assert!(device(&["read", "write"]).can_write());
        assert!(device(&["admin"]).can_write());
        assert!(Caller::Local.can_write());
    }
}
