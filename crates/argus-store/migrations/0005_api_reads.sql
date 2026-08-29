-- Indexes and corrections the read API needs.
--
-- Everything before this migration was written for the ingest path. Serving
-- clients asks two questions the write path never did: "what changed since I
-- last heard from you" (the WebSocket delta stream) and "is this bearer token
-- one of mine" (device auth on every request).

-- The delta stream polls `updated_at` on a short cycle. Without this it is a
-- sequential scan of every live entity every couple of seconds, per connected
-- client — the one query in the system whose cost scales with the number of
-- people watching.
--
-- Note this is `updated_at`, not `observed_at`. A client resuming after a
-- dropped connection wants everything Argus *learned* since then, which
-- includes a late-arriving fix whose source timestamp is older than the client's
-- cursor. Keying the stream on observed_at would silently skip exactly those.
CREATE INDEX entities_updated_idx ON entities (updated_at);

-- Correcting the record rather than rewriting history: 0001 declared this
-- column an Argon2 hash, and it is not.
--
-- Argon2 exists to make *low-entropy* secrets expensive to guess. A device
-- token here is 32 bytes from the OS CSPRNG and is never chosen by a human, so
-- there is no dictionary to run and no cost factor worth paying — a plain
-- SHA-256 is already beyond brute force, and it keeps token verification off
-- the critical path of every single API request rather than costing tens of
-- milliseconds of deliberate work per call.
--
-- This reasoning holds *only* because the token is machine-generated. If a
-- pairing flow is ever added that lets a human pick the secret, this must
-- become a real password hash.
COMMENT ON COLUMN devices.token_hash IS
    'Lowercase hex SHA-256 of the 32-byte random bearer token. Not a password '
    'hash, and correctly so: the input is full-entropy and machine-generated. '
    'See 0005_api_reads.sql.';

-- Token lookup happens on every authenticated request, keyed by the hash rather
-- than the device id: the client presents only the token, so the hash is what
-- there is to look up by.
CREATE UNIQUE INDEX devices_token_idx ON devices (token_hash);
