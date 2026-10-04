-- storlite migration 0003: access keys, grants, and the admin audit log live
-- in the metadata database (managed through the admin API).
-- secret_scheme 0 = plaintext, 1 = AES-256-GCM under master key version 1.
-- The AEAD binds each ciphertext to the store id and access key id.
CREATE TABLE credentials (
    access_key_id TEXT PRIMARY KEY CHECK(length(access_key_id) BETWEEN 3 AND 128),
    secret_scheme INTEGER NOT NULL CHECK(secret_scheme IN (0, 1)),
    secret_nonce BLOB CHECK(secret_nonce IS NULL OR length(secret_nonce) = 12),
    secret_value BLOB NOT NULL,
    -- Previous secret kept valid during a rotation grace period.
    previous_scheme INTEGER CHECK(previous_scheme IS NULL OR previous_scheme IN (0, 1)),
    previous_nonce BLOB CHECK(previous_nonce IS NULL OR length(previous_nonce) = 12),
    previous_value BLOB,
    previous_expires_at_ms INTEGER,
    enabled INTEGER NOT NULL CHECK(enabled IN (0, 1)),
    description TEXT NOT NULL DEFAULT '' CHECK(length(description) <= 256),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
    expires_at_ms INTEGER,
    CHECK((secret_scheme = 0) = (secret_nonce IS NULL)),
    CHECK((previous_value IS NULL) = (previous_scheme IS NULL)),
    CHECK((previous_value IS NULL) = (previous_expires_at_ms IS NULL)),
    CHECK(previous_value IS NULL OR (previous_scheme = 0) = (previous_nonce IS NULL))
) STRICT, WITHOUT ROWID;

CREATE TABLE credential_global_grants (
    access_key_id TEXT NOT NULL REFERENCES credentials(access_key_id) ON DELETE CASCADE,
    grant_name TEXT NOT NULL CHECK(grant_name IN ('admin', 'list_buckets', 'create_bucket')),
    PRIMARY KEY (access_key_id, grant_name)
) STRICT, WITHOUT ROWID;

CREATE TABLE credential_grants (
    access_key_id TEXT NOT NULL REFERENCES credentials(access_key_id) ON DELETE CASCADE,
    bucket TEXT NOT NULL CHECK(length(bucket) BETWEEN 3 AND 63),
    prefix BLOB NOT NULL CHECK(length(prefix) <= 1024),
    actions_json TEXT NOT NULL CHECK(json_valid(actions_json) AND json_type(actions_json) = 'array'),
    PRIMARY KEY (access_key_id, bucket, prefix)
) STRICT, WITHOUT ROWID;

CREATE TABLE admin_audit (
    id INTEGER PRIMARY KEY,
    at_ms INTEGER NOT NULL CHECK(at_ms >= 0),
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    detail_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(detail_json))
) STRICT;
CREATE INDEX admin_audit_at_idx ON admin_audit(at_ms);
