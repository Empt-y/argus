//! Paired clients and their bearer tokens.
//!
//! The threat model is narrow and worth stating, because it justifies how
//! little is here. Argus is reachable on a home LAN and over Tailscale, and it
//! holds every provider credential the operator configured. A token exists so
//! that a device on the LAN is not automatically trusted, and so that a lost
//! phone can be cut off without rotating anything else.
//!
//! Tokens are 32 bytes from the OS CSPRNG and stored as a plain SHA-256. See
//! `migrations/0005_api_reads.sql` for why that is the right hash and not a
//! lazy one — briefly: there is no low-entropy secret here for a password hash
//! to protect, and token verification runs on every request.

use crate::{Store, StoreError, model};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// A freshly issued device credential. The plaintext exists only in this
/// struct, on its way to the QR code — it is never written down.
#[derive(Debug, Clone)]
pub struct IssuedDevice {
    pub device: model::DeviceRow,
    pub token: String,
}

/// Hash a bearer token the way the `devices` table stores it.
pub fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Mint a token: 32 random bytes, hex-encoded.
///
/// Hex rather than base64 so the string survives being typed by hand off a
/// screen, pasted through a shell, and embedded in a URL without escaping.
fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

impl Store {
    /// Pair a device, returning the one and only copy of its token.
    pub async fn create_device(
        &self,
        name: &str,
        scopes: &[String],
    ) -> Result<IssuedDevice, StoreError> {
        let token = mint_token();
        let device = sqlx::query_as::<_, model::DeviceRow>(
            r#"
            INSERT INTO devices (device_id, name, token_hash, scopes)
            VALUES (gen_random_uuid(), $1, $2, $3)
            RETURNING device_id, name, scopes, created_at, last_seen_at, revoked_at
            "#,
        )
        .bind(name)
        .bind(token_hash(&token))
        .bind(scopes)
        .fetch_one(&self.pool)
        .await?;
        Ok(IssuedDevice { device, token })
    }

    /// Resolve a bearer token to its device, or `None` if it is unknown or
    /// revoked.
    ///
    /// Also stamps `last_seen_at`, but at most once a minute per device: a
    /// client panning a map issues hundreds of tile requests, and a write per
    /// request would make the busiest table in the read path the one nobody
    /// reads.
    pub async fn authenticate(
        &self,
        token: &str,
    ) -> Result<Option<model::DeviceRow>, StoreError> {
        let row = sqlx::query_as::<_, model::DeviceRow>(
            r#"
            WITH matched AS (
                SELECT device_id FROM devices
                WHERE token_hash = $1 AND revoked_at IS NULL
            ), touched AS (
                UPDATE devices d SET last_seen_at = now()
                FROM matched m
                WHERE d.device_id = m.device_id
                  AND (d.last_seen_at IS NULL
                       OR d.last_seen_at < now() - INTERVAL '1 minute')
            )
            SELECT d.device_id, d.name, d.scopes, d.created_at,
                   d.last_seen_at, d.revoked_at
            FROM devices d
            JOIN matched m ON m.device_id = d.device_id
            "#,
        )
        .bind(token_hash(token))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn list_devices(&self) -> Result<Vec<model::DeviceRow>, StoreError> {
        let rows = sqlx::query_as::<_, model::DeviceRow>(
            r#"
            SELECT device_id, name, scopes, created_at, last_seen_at, revoked_at
            FROM devices ORDER BY created_at
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Revoke rather than delete. A device row is the only record that a paired
    /// client ever existed, and "this phone was cut off on the 3rd" is worth
    /// more than a reclaimed row.
    pub async fn revoke_device(&self, device_id: uuid::Uuid) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE devices SET revoked_at = now()
             WHERE device_id = $1 AND revoked_at IS NULL",
        )
        .bind(device_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether any device has ever been paired. Drives whether `argusd` shows a
    /// pairing QR on startup.
    pub async fn has_devices(&self) -> Result<bool, StoreError> {
        let any: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM devices WHERE revoked_at IS NULL)",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(any)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_is_stable_and_matches_the_documented_algorithm() {
        // Pinned against `printf 'argus' | sha256sum`, so a dependency bump
        // that changed the digest could not slip through and silently
        // invalidate every paired device.
        assert_eq!(
            token_hash("argus"),
            "444b759c5264422ea582403ae2083d2447fd226a2e40795968dd740e9202cb97"
        );
    }

    #[test]
    fn minted_tokens_are_full_entropy_and_distinct() {
        let a = mint_token();
        let b = mint_token();
        assert_eq!(a.len(), 64, "32 bytes hex-encoded");
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
