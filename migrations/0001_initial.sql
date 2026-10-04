-- storlite migration 0001: initial schema.
-- Derived from docs/reference-schema.sql (specification 1.0) with additions
-- marked below. Cross-table invariants are enforced by application transactions.
CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    checksum_sha256 BLOB NOT NULL CHECK(length(checksum_sha256) = 32),
    applied_at_ms INTEGER NOT NULL CHECK(applied_at_ms >= 0)
) STRICT;

CREATE TABLE store_meta (
    key TEXT PRIMARY KEY,
    value BLOB NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE buckets (
    id BLOB PRIMARY KEY CHECK(length(id) = 16),
    name TEXT NOT NULL UNIQUE CHECK(length(name) BETWEEN 3 AND 63),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    object_count INTEGER NOT NULL DEFAULT 0 CHECK(object_count >= 0),
    logical_bytes INTEGER NOT NULL DEFAULT 0 CHECK(logical_bytes >= 0),
    quota_bytes INTEGER CHECK(quota_bytes IS NULL OR quota_bytes >= 0),
    cors_json TEXT CHECK(cors_json IS NULL OR json_valid(cors_json))
) STRICT, WITHOUT ROWID;

CREATE TABLE blobs (
    storage_id BLOB PRIMARY KEY CHECK(length(storage_id) = 16),
    area TEXT NOT NULL CHECK(area IN ('object', 'part')),
    state TEXT NOT NULL CHECK(state IN ('writing', 'ready', 'garbage')),
    size_bytes INTEGER NOT NULL DEFAULT 0 CHECK(size_bytes >= 0),
    md5 BLOB CHECK(md5 IS NULL OR length(md5) = 16),
    sha256 BLOB CHECK(sha256 IS NULL OR length(sha256) = 32),
    checksums_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(checksums_json)),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    garbage_after_ms INTEGER CHECK(garbage_after_ms IS NULL OR garbage_after_ms >= 0),
    CHECK(state != 'ready' OR (md5 IS NOT NULL AND sha256 IS NOT NULL)),
    CHECK(state != 'garbage' OR garbage_after_ms IS NOT NULL)
) STRICT, WITHOUT ROWID;
CREATE INDEX blobs_cleanup_idx ON blobs(state, garbage_after_ms, storage_id);

CREATE TABLE objects (
    bucket_id BLOB NOT NULL REFERENCES buckets(id) ON DELETE RESTRICT,
    object_key BLOB NOT NULL CHECK(length(object_key) BETWEEN 1 AND 1024),
    storage_id BLOB NOT NULL UNIQUE REFERENCES blobs(storage_id) ON DELETE RESTRICT,
    generation_id BLOB NOT NULL CHECK(length(generation_id) = 16),
    etag TEXT NOT NULL CHECK(length(etag) > 0),
    headers_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(headers_json)),
    user_metadata_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(user_metadata_json)),
    last_modified_ms INTEGER NOT NULL CHECK(last_modified_ms >= 0),
    PRIMARY KEY(bucket_id, object_key)
) STRICT, WITHOUT ROWID;

CREATE TABLE multipart_uploads (
    upload_id TEXT PRIMARY KEY CHECK(length(upload_id) = 32),
    bucket_id BLOB NOT NULL REFERENCES buckets(id) ON DELETE RESTRICT,
    object_key BLOB NOT NULL CHECK(length(object_key) BETWEEN 1 AND 1024),
    state TEXT NOT NULL CHECK(state IN ('open', 'completing', 'completed', 'aborted')),
    creator_key_id TEXT NOT NULL,
    headers_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(headers_json)),
    user_metadata_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(user_metadata_json)),
    checksum_algorithm TEXT NOT NULL DEFAULT 'CRC64NVME'
        CHECK(checksum_algorithm IN ('CRC32','CRC32C','CRC64NVME','SHA1','SHA256')),
    checksum_type TEXT NOT NULL DEFAULT 'FULL_OBJECT'
        CHECK(checksum_type IN ('FULL_OBJECT','COMPOSITE')),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    last_activity_ms INTEGER NOT NULL CHECK(last_activity_ms >= 0),
    completion_fingerprint BLOB
        CHECK(completion_fingerprint IS NULL OR length(completion_fingerprint) = 32),
    completion_manifest_json TEXT
        CHECK(completion_manifest_json IS NULL OR json_valid(completion_manifest_json)),
    result_json TEXT CHECK(result_json IS NULL OR json_valid(result_json)),
    closed_at_ms INTEGER CHECK(closed_at_ms IS NULL OR closed_at_ms >= 0),
    receipt_expires_at_ms INTEGER
        CHECK(receipt_expires_at_ms IS NULL OR receipt_expires_at_ms >= 0),
    -- storlite additions to the reference schema:
    -- 1 when the client chose the checksum algorithm/type at initiation.
    checksum_explicit INTEGER NOT NULL DEFAULT 0 CHECK(checksum_explicit IN (0, 1)),
    -- WRITING output blob owned by a COMPLETING upload, for recovery linkage.
    completion_output_id BLOB
        CHECK(completion_output_id IS NULL OR length(completion_output_id) = 16),
    CHECK(checksum_algorithm != 'CRC64NVME' OR checksum_type = 'FULL_OBJECT'),
    CHECK(checksum_algorithm NOT IN ('SHA1','SHA256') OR checksum_type = 'COMPOSITE'),
    CHECK(state != 'completing' OR
        (completion_fingerprint IS NOT NULL AND completion_manifest_json IS NOT NULL)),
    CHECK(state != 'completed' OR
        (completion_fingerprint IS NOT NULL AND result_json IS NOT NULL
         AND closed_at_ms IS NOT NULL AND receipt_expires_at_ms IS NOT NULL)),
    CHECK(state != 'aborted' OR
        (closed_at_ms IS NOT NULL AND receipt_expires_at_ms IS NOT NULL))
) STRICT, WITHOUT ROWID;
CREATE INDEX multipart_listing_idx
    ON multipart_uploads(bucket_id, object_key, upload_id);
CREATE INDEX multipart_expiry_idx
    ON multipart_uploads(state, last_activity_ms, upload_id);
CREATE INDEX multipart_receipt_expiry_idx
    ON multipart_uploads(state, receipt_expires_at_ms, upload_id);

CREATE TABLE multipart_parts (
    upload_id TEXT NOT NULL REFERENCES multipart_uploads(upload_id) ON DELETE RESTRICT,
    part_number INTEGER NOT NULL CHECK(part_number BETWEEN 1 AND 10000),
    storage_id BLOB NOT NULL UNIQUE REFERENCES blobs(storage_id) ON DELETE RESTRICT,
    etag TEXT NOT NULL CHECK(length(etag) > 0),
    last_modified_ms INTEGER NOT NULL CHECK(last_modified_ms >= 0),
    PRIMARY KEY(upload_id, part_number)
) STRICT, WITHOUT ROWID;
