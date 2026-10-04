//! Parameterized SQL for every metadata state transition.
//!
//! Functions take a `&Connection` (a `Transaction` derefs to one). Callers run
//! mutations inside `with_write_tx` so each transition is one short
//! `BEGIN IMMEDIATE` transaction.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::checksums::{Algorithm, ChecksumType, StoredChecksum};
use crate::error::{Error, Result};
use crate::ids::{BucketId, GenerationId, StorageId, StoreId, random_bytes};
use crate::secrets::Sealed;

// ---------------------------------------------------------------------------
// Row types

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentHeaders {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content_disposition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content_encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content_language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cache_control: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expires: Option<String>,
}

/// Lowercased `x-amz-meta-*` names (without prefix) to values.
pub type UserMetadata = BTreeMap<String, String>;

#[derive(Clone, Debug)]
pub struct StoreMeta {
    pub store_id: StoreId,
    pub region: String,
    pub owner_id: String,
    pub cursor_key: Vec<u8>,
    pub format_version: i64,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct BucketRow {
    pub id: BucketId,
    pub name: String,
    pub created_at_ms: i64,
    pub object_count: i64,
    pub logical_bytes: i64,
    pub quota_bytes: Option<i64>,
    pub cors_json: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ObjectRow {
    pub bucket_id: BucketId,
    pub key: Vec<u8>,
    pub storage_id: StorageId,
    pub generation_id: GenerationId,
    pub etag: String,
    pub headers: ContentHeaders,
    pub user_metadata: UserMetadata,
    pub last_modified_ms: i64,
    pub size: u64,
    pub checksum: Option<StoredChecksum>,
    /// Part sizes of an assembled multipart object.
    pub part_sizes: Option<Vec<u64>>,
}

/// Final facts about a durably published blob, installed at commit.
#[derive(Clone, Debug)]
pub struct BlobFinal {
    pub storage_id: StorageId,
    pub size: u64,
    pub md5: [u8; 16],
    pub sha256: [u8; 32],
    pub checksum: Option<StoredChecksum>,
    pub part_sizes: Option<Vec<u64>>,
}

#[derive(Clone, Debug)]
pub struct NewObject {
    pub bucket_id: BucketId,
    pub key: Vec<u8>,
    pub blob: BlobFinal,
    pub etag: String,
    pub headers: ContentHeaders,
    pub user_metadata: UserMetadata,
    pub now_ms: i64,
}

/// Write preconditions evaluated against committed state at commit time.
#[derive(Clone, Debug, Default)]
pub struct WriteConditions {
    /// ETags without quotes; `*` matches any existing object.
    pub if_match: Option<Vec<String>>,
    pub if_none_match_any: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectCommit {
    Committed {
        generation_id: GenerationId,
        last_modified_ms: i64,
        replaced: Option<(StorageId, u64)>,
    },
    NoSuchBucket,
    NoSuchKey,
    PreconditionFailed,
    QuotaExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    NoSuchBucket,
    Absent,
    Deleted { storage_id: StorageId, size: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadState {
    Open,
    Completing,
    Completed,
    Aborted,
}

impl UploadState {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "open" => Self::Open,
            "completing" => Self::Completing,
            "completed" => Self::Completed,
            "aborted" => Self::Aborted,
            _ => return Err(Error::integrity(format!("unknown upload state {s}"))),
        })
    }
}

#[derive(Clone, Debug)]
pub struct UploadRow {
    pub upload_id: String,
    pub bucket_id: BucketId,
    pub key: Vec<u8>,
    pub state: UploadState,
    pub creator_key_id: String,
    pub headers: ContentHeaders,
    pub user_metadata: UserMetadata,
    pub checksum_algorithm: Algorithm,
    pub checksum_type: ChecksumType,
    pub checksum_explicit: bool,
    pub created_at_ms: i64,
    pub last_activity_ms: i64,
    pub completion_fingerprint: Option<Vec<u8>>,
    pub result_json: Option<String>,
    pub receipt_expires_at_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct PartRow {
    pub part_number: u32,
    pub storage_id: StorageId,
    pub etag: String,
    pub size: u64,
    pub md5: [u8; 16],
    pub checksum: Option<StoredChecksum>,
    pub last_modified_ms: i64,
}

#[derive(Clone, Debug)]
pub struct ListedObject {
    pub key: Vec<u8>,
    pub etag: String,
    pub size: u64,
    pub last_modified_ms: i64,
    pub checksum: Option<StoredChecksum>,
}

// ---------------------------------------------------------------------------
// Helpers

fn storage_id(v: Vec<u8>) -> rusqlite::Result<StorageId> {
    StorageId::from_slice(&v).ok_or_else(|| invalid("storage_id"))
}

fn bucket_id(v: Vec<u8>) -> rusqlite::Result<BucketId> {
    BucketId::from_slice(&v).ok_or_else(|| invalid("bucket_id"))
}

fn invalid(what: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(0, what.to_string(), rusqlite::types::Type::Blob)
}

fn json_or_default<T: for<'de> Deserialize<'de> + Default>(s: &str) -> T {
    serde_json::from_str(s).unwrap_or_default()
}

fn checksum_from_json(s: &str) -> Option<StoredChecksum> {
    serde_json::from_str(s).ok()
}

fn checksum_to_json(c: &Option<StoredChecksum>) -> String {
    match c {
        Some(c) => serde_json::to_string(c).unwrap_or_else(|_| "{}".into()),
        None => "{}".into(),
    }
}

fn to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| Error::other("value exceeds signed 64-bit range"))
}

/// Compare an ETag against a client list (quotes stripped by the caller).
pub fn etag_matches(etag: &str, candidates: &[String]) -> bool {
    candidates.iter().any(|c| c == "*" || c == etag)
}

// ---------------------------------------------------------------------------
// Store metadata

pub fn init_store_meta(conn: &Connection, region: &str, now_ms: i64) -> Result<StoreMeta> {
    let store_id = StoreId::random();
    let owner_id = hex::encode(random_bytes::<32>());
    let cursor_key = random_bytes::<32>().to_vec();
    let put = |k: &str, v: Vec<u8>| -> Result<()> {
        conn.execute(
            "INSERT INTO store_meta(key, value) VALUES (?1, ?2)",
            params![k, v],
        )?;
        Ok(())
    };
    put(
        "format_version",
        super::migrations::FORMAT_VERSION.to_string().into_bytes(),
    )?;
    put("store_id", store_id.as_bytes().to_vec())?;
    put("region", region.as_bytes().to_vec())?;
    put("owner_id", owner_id.clone().into_bytes())?;
    put("cursor_hmac_key", cursor_key.clone())?;
    put("created_at_ms", now_ms.to_string().into_bytes())?;
    Ok(StoreMeta {
        store_id,
        region: region.to_string(),
        owner_id,
        cursor_key,
        format_version: super::migrations::FORMAT_VERSION,
        created_at_ms: now_ms,
    })
}

pub fn load_store_meta(conn: &Connection) -> Result<StoreMeta> {
    let get = |k: &str| -> Result<Vec<u8>> {
        conn.query_row("SELECT value FROM store_meta WHERE key = ?1", [k], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| Error::integrity(format!("store_meta is missing {k}")))
    };
    let text = |k: &str| -> Result<String> {
        String::from_utf8(get(k)?)
            .map_err(|_| Error::integrity(format!("store_meta {k} is not UTF-8")))
    };
    let format_version: i64 = text("format_version")?
        .parse()
        .map_err(|_| Error::integrity("invalid format_version"))?;
    let store_id = StoreId::from_slice(&get("store_id")?)
        .ok_or_else(|| Error::integrity("invalid store_id"))?;
    let region = text("region")?;
    crate::config::validate_region(&region)
        .map_err(|_| Error::integrity("invalid stored region"))?;
    let owner_id = text("owner_id")?;
    if owner_id.len() != 64 || !owner_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::integrity("invalid owner_id"));
    }
    let cursor_key = get("cursor_hmac_key")?;
    if cursor_key.len() != 32 {
        return Err(Error::integrity("invalid cursor_hmac_key"));
    }
    let created_at_ms = text("created_at_ms")?.parse().unwrap_or(0);
    Ok(StoreMeta {
        store_id,
        region,
        owner_id,
        cursor_key,
        format_version,
        created_at_ms,
    })
}

// ---------------------------------------------------------------------------
// Buckets

const BUCKET_COLS: &str =
    "id, name, created_at_ms, object_count, logical_bytes, quota_bytes, cors_json";

fn bucket_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<BucketRow> {
    Ok(BucketRow {
        id: bucket_id(r.get(0)?)?,
        name: r.get(1)?,
        created_at_ms: r.get(2)?,
        object_count: r.get(3)?,
        logical_bytes: r.get(4)?,
        quota_bytes: r.get(5)?,
        cors_json: r.get(6)?,
    })
}

pub fn bucket_by_name(conn: &Connection, name: &str) -> Result<Option<BucketRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {BUCKET_COLS} FROM buckets WHERE name = ?1"),
            [name],
            bucket_row,
        )
        .optional()?)
}

pub fn bucket_by_id(conn: &Connection, id: &BucketId) -> Result<Option<BucketRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {BUCKET_COLS} FROM buckets WHERE id = ?1"),
            [id.as_bytes().as_slice()],
            bucket_row,
        )
        .optional()?)
}

/// Buckets with names greater than `after` and starting with `prefix`, ordered.
pub fn list_buckets(
    conn: &Connection,
    after: &str,
    prefix: &str,
    limit: usize,
) -> Result<Vec<BucketRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {BUCKET_COLS} FROM buckets WHERE name > ?1 AND substr(name, 1, length(?2)) = ?2 ORDER BY name LIMIT ?3"
    ))?;
    let rows = stmt
        .query_map(params![after, prefix, limit as i64], bucket_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, PartialEq, Eq)]
pub enum CreateBucket {
    Created(BucketId),
    AlreadyExists,
    TooManyBuckets,
}

pub fn create_bucket(
    conn: &Connection,
    name: &str,
    now_ms: i64,
    max_buckets: u64,
) -> Result<CreateBucket> {
    if bucket_by_name(conn, name)?.is_some() {
        return Ok(CreateBucket::AlreadyExists);
    }
    let count: i64 = conn.query_row("SELECT count(*) FROM buckets", [], |r| r.get(0))?;
    if count as u64 >= max_buckets {
        return Ok(CreateBucket::TooManyBuckets);
    }
    let id = BucketId::random();
    conn.execute(
        "INSERT INTO buckets(id, name, created_at_ms) VALUES (?1, ?2, ?3)",
        params![id.as_bytes().as_slice(), name, now_ms],
    )?;
    Ok(CreateBucket::Created(id))
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeleteBucket {
    Deleted,
    NoSuchBucket,
    NotEmpty,
}

/// Delete an empty bucket. Emptiness is checked in the same transaction.
pub fn delete_bucket(conn: &Connection, id: &BucketId) -> Result<DeleteBucket> {
    let idb = id.as_bytes().as_slice();
    if bucket_by_id(conn, id)?.is_none() {
        return Ok(DeleteBucket::NoSuchBucket);
    }
    let has_object: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM objects WHERE bucket_id = ?1 LIMIT 1",
            [idb],
            |r| r.get(0),
        )
        .optional()?;
    let has_upload: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM multipart_uploads WHERE bucket_id = ?1 AND state IN ('open', 'completing') LIMIT 1",
            [idb],
            |r| r.get(0),
        )
        .optional()?;
    if has_object.is_some() || has_upload.is_some() {
        return Ok(DeleteBucket::NotEmpty);
    }
    // Terminal receipts have no parts; remove them with the bucket.
    conn.execute(
        "DELETE FROM multipart_uploads WHERE bucket_id = ?1 AND state IN ('completed', 'aborted')",
        [idb],
    )?;
    conn.execute("DELETE FROM buckets WHERE id = ?1", [idb])?;
    Ok(DeleteBucket::Deleted)
}

pub fn set_bucket_cors(conn: &Connection, id: &BucketId, cors_json: Option<&str>) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE buckets SET cors_json = ?2 WHERE id = ?1",
        params![id.as_bytes().as_slice(), cors_json],
    )? == 1)
}

pub fn set_bucket_quota(conn: &Connection, name: &str, quota: Option<u64>) -> Result<bool> {
    let q = quota.map(to_i64).transpose()?;
    Ok(conn.execute(
        "UPDATE buckets SET quota_bytes = ?2 WHERE name = ?1",
        params![name, q],
    )? == 1)
}

/// Recompute a bucket's actual usage (used by periodic verification and doctor).
pub fn bucket_actual_usage(conn: &Connection, id: &BucketId) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT count(*), coalesce(sum(b.size_bytes), 0) FROM objects o JOIN blobs b ON b.storage_id = o.storage_id WHERE o.bucket_id = ?1",
        [id.as_bytes().as_slice()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

// ---------------------------------------------------------------------------
// Blobs

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobArea {
    Object,
    Part,
}

impl BlobArea {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Part => "part",
        }
    }

    pub fn fs_area(self) -> crate::fsutil::Area {
        match self {
            Self::Object => crate::fsutil::Area::Objects,
            Self::Part => crate::fsutil::Area::Multipart,
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "object" => Ok(Self::Object),
            "part" => Ok(Self::Part),
            _ => Err(invalid("area")),
        }
    }
}

/// Register a WRITING blob. Returns false on an ID collision.
pub fn register_blob(
    conn: &Connection,
    id: &StorageId,
    area: BlobArea,
    now_ms: i64,
) -> Result<bool> {
    match conn.execute(
        "INSERT INTO blobs(storage_id, area, state, created_at_ms) VALUES (?1, ?2, 'writing', ?3)",
        params![id.as_bytes().as_slice(), area.as_str(), now_ms],
    ) {
        Ok(_) => Ok(true),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

pub fn blob_state(conn: &Connection, id: &StorageId) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT state FROM blobs WHERE storage_id = ?1",
            [id.as_bytes().as_slice()],
            |r| r.get(0),
        )
        .optional()?)
}

/// Mark an unreferenced WRITING blob as garbage after a definite abort.
pub fn abandon_blob(conn: &Connection, id: &StorageId, garbage_after_ms: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE blobs SET state = 'garbage', garbage_after_ms = ?2 WHERE storage_id = ?1 AND state = 'writing'",
        params![id.as_bytes().as_slice(), garbage_after_ms],
    )? == 1)
}

fn finalize_blob(conn: &Connection, b: &BlobFinal, area: BlobArea) -> Result<()> {
    let n = conn.execute(
        "UPDATE blobs SET state = 'ready', size_bytes = ?2, md5 = ?3, sha256 = ?4, checksums_json = ?5,
             part_sizes_json = ?7
         WHERE storage_id = ?1 AND state = 'writing' AND area = ?6",
        params![
            b.storage_id.as_bytes().as_slice(),
            to_i64(b.size)?,
            b.md5.as_slice(),
            b.sha256.as_slice(),
            checksum_to_json(&b.checksum),
            area.as_str(),
            b.part_sizes
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
        ],
    )?;
    if n != 1 {
        return Err(Error::integrity(format!(
            "blob {} is not a WRITING {} blob at commit",
            b.storage_id,
            area.as_str()
        )));
    }
    Ok(())
}

fn garbage_ready_blob(conn: &Connection, id: &StorageId, garbage_after_ms: i64) -> Result<()> {
    conn.execute(
        "UPDATE blobs SET state = 'garbage', garbage_after_ms = ?2 WHERE storage_id = ?1 AND state = 'ready'",
        params![id.as_bytes().as_slice(), garbage_after_ms],
    )?;
    Ok(())
}

/// Eligible garbage, oldest first.
pub fn garbage_batch(
    conn: &Connection,
    now_ms: i64,
    limit: usize,
) -> Result<Vec<(StorageId, BlobArea, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT storage_id, area, size_bytes FROM blobs WHERE state = 'garbage' AND garbage_after_ms <= ?1
         ORDER BY garbage_after_ms, storage_id LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![now_ms, limit as i64], |r| {
            Ok((
                storage_id(r.get(0)?)?,
                BlobArea::parse(&r.get::<_, String>(1)?)?,
                r.get::<_, i64>(2)? as u64,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Remove a garbage row after its files are gone, revalidating that nothing
/// references it. Returns false if the row was not eligible.
pub fn delete_garbage_row(conn: &Connection, id: &StorageId) -> Result<bool> {
    let idb = id.as_bytes().as_slice();
    Ok(conn.execute(
        "DELETE FROM blobs WHERE storage_id = ?1 AND state = 'garbage'
           AND NOT EXISTS (SELECT 1 FROM objects WHERE storage_id = ?1)
           AND NOT EXISTS (SELECT 1 FROM multipart_parts WHERE storage_id = ?1)",
        [idb],
    )? == 1)
}

pub fn garbage_backlog(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT count(*), coalesce(sum(size_bytes), 0) FROM blobs WHERE state = 'garbage'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// Bytes held by committed multipart parts (temporary-space accounting).
pub fn committed_part_bytes(conn: &Connection) -> Result<u64> {
    let v: i64 = conn.query_row(
        "SELECT coalesce(sum(b.size_bytes), 0) FROM multipart_parts p JOIN blobs b ON b.storage_id = p.storage_id",
        [],
        |r| r.get(0),
    )?;
    Ok(v.max(0) as u64)
}

// ---------------------------------------------------------------------------
// Objects

const OBJECT_SELECT: &str =
    "SELECT o.bucket_id, o.object_key, o.storage_id, o.generation_id, o.etag,
        o.headers_json, o.user_metadata_json, o.last_modified_ms, b.size_bytes, b.checksums_json,
        b.part_sizes_json
     FROM objects o JOIN blobs b ON b.storage_id = o.storage_id";

fn object_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ObjectRow> {
    let headers: String = r.get(5)?;
    let meta: String = r.get(6)?;
    let checksums: String = r.get(9)?;
    Ok(ObjectRow {
        bucket_id: bucket_id(r.get(0)?)?,
        key: r.get(1)?,
        storage_id: storage_id(r.get(2)?)?,
        generation_id: GenerationId::from_slice(&r.get::<_, Vec<u8>>(3)?)
            .ok_or_else(|| invalid("generation_id"))?,
        etag: r.get(4)?,
        headers: json_or_default(&headers),
        user_metadata: json_or_default(&meta),
        last_modified_ms: r.get(7)?,
        size: r.get::<_, i64>(8)? as u64,
        checksum: checksum_from_json(&checksums),
        part_sizes: r
            .get::<_, Option<String>>(10)?
            .and_then(|j| serde_json::from_str(&j).ok()),
    })
}

pub fn get_object(conn: &Connection, bucket: &BucketId, key: &[u8]) -> Result<Option<ObjectRow>> {
    Ok(conn
        .query_row(
            &format!("{OBJECT_SELECT} WHERE o.bucket_id = ?1 AND o.object_key = ?2"),
            params![bucket.as_bytes().as_slice(), key],
            object_row,
        )
        .optional()?)
}

/// Install a new object generation: final condition and quota checks, blob
/// READY transition, mapping replacement, old blob GARBAGE, counters.
pub fn commit_object(
    conn: &Connection,
    new: &NewObject,
    cond: &WriteConditions,
    garbage_after_ms: i64,
) -> Result<ObjectCommit> {
    let bid = new.bucket_id.as_bytes().as_slice();
    let bucket: Option<(i64, Option<i64>)> = conn
        .query_row(
            "SELECT logical_bytes, quota_bytes FROM buckets WHERE id = ?1",
            [bid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((logical, quota)) = bucket else {
        return Ok(ObjectCommit::NoSuchBucket);
    };
    let current: Option<(Vec<u8>, String, i64)> = conn
        .query_row(
            "SELECT o.storage_id, o.etag, b.size_bytes FROM objects o JOIN blobs b ON b.storage_id = o.storage_id
             WHERE o.bucket_id = ?1 AND o.object_key = ?2",
            params![bid, new.key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if cond.if_none_match_any && current.is_some() {
        return Ok(ObjectCommit::PreconditionFailed);
    }
    if let Some(list) = &cond.if_match {
        match &current {
            None => return Ok(ObjectCommit::NoSuchKey),
            Some((_, etag, _)) if !etag_matches(etag, list) => {
                return Ok(ObjectCommit::PreconditionFailed);
            }
            _ => {}
        }
    }
    let new_size = to_i64(new.blob.size)?;
    let old_size = current.as_ref().map(|c| c.2).unwrap_or(0);
    let delta = new_size - old_size;
    if let Some(q) = quota
        && delta > 0
        && logical.saturating_add(delta) > q
    {
        return Ok(ObjectCommit::QuotaExceeded);
    }
    finalize_blob(conn, &new.blob, BlobArea::Object)?;
    let generation_id = GenerationId::random();
    let headers = serde_json::to_string(&new.headers).map_err(|e| Error::other(e.to_string()))?;
    let meta =
        serde_json::to_string(&new.user_metadata).map_err(|e| Error::other(e.to_string()))?;
    let sid = new.blob.storage_id.as_bytes().as_slice();
    let replaced = match current {
        Some((old_sid, _, old_size)) => {
            conn.execute(
                "UPDATE objects SET storage_id = ?3, generation_id = ?4, etag = ?5, headers_json = ?6,
                     user_metadata_json = ?7, last_modified_ms = ?8
                 WHERE bucket_id = ?1 AND object_key = ?2",
                params![bid, new.key, sid, generation_id.as_bytes().as_slice(), new.etag, headers, meta, new.now_ms],
            )?;
            let old = storage_id(old_sid)?;
            garbage_ready_blob(conn, &old, garbage_after_ms)?;
            conn.execute(
                "UPDATE buckets SET logical_bytes = logical_bytes + ?2 WHERE id = ?1",
                params![bid, delta],
            )?;
            Some((old, old_size as u64))
        }
        None => {
            conn.execute(
                "INSERT INTO objects(bucket_id, object_key, storage_id, generation_id, etag, headers_json,
                     user_metadata_json, last_modified_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![bid, new.key, sid, generation_id.as_bytes().as_slice(), new.etag, headers, meta, new.now_ms],
            )?;
            conn.execute(
                "UPDATE buckets SET object_count = object_count + 1, logical_bytes = logical_bytes + ?2 WHERE id = ?1",
                params![bid, new_size],
            )?;
            None
        }
    };
    Ok(ObjectCommit::Committed {
        generation_id,
        last_modified_ms: new.now_ms,
        replaced,
    })
}

pub fn delete_object(
    conn: &Connection,
    bucket: &BucketId,
    key: &[u8],
    garbage_after_ms: i64,
) -> Result<DeleteOutcome> {
    let bid = bucket.as_bytes().as_slice();
    if bucket_by_id(conn, bucket)?.is_none() {
        return Ok(DeleteOutcome::NoSuchBucket);
    }
    let current: Option<(Vec<u8>, i64)> = conn
        .query_row(
            "SELECT o.storage_id, b.size_bytes FROM objects o JOIN blobs b ON b.storage_id = o.storage_id
             WHERE o.bucket_id = ?1 AND o.object_key = ?2",
            params![bid, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((sid, size)) = current else {
        return Ok(DeleteOutcome::Absent);
    };
    conn.execute(
        "DELETE FROM objects WHERE bucket_id = ?1 AND object_key = ?2",
        params![bid, key],
    )?;
    let sid = storage_id(sid)?;
    garbage_ready_blob(conn, &sid, garbage_after_ms)?;
    conn.execute(
        "UPDATE buckets SET object_count = object_count - 1, logical_bytes = logical_bytes - ?2 WHERE id = ?1",
        params![bid, size],
    )?;
    Ok(DeleteOutcome::Deleted {
        storage_id: sid,
        size: size as u64,
    })
}

/// Objects strictly after (or at, if `inclusive`) `from`, below `upper`.
pub fn objects_from(
    conn: &Connection,
    bucket: &BucketId,
    from: &[u8],
    inclusive: bool,
    upper: Option<&[u8]>,
    limit: usize,
) -> Result<Vec<ListedObject>> {
    let op = if inclusive { ">=" } else { ">" };
    let sql = format!(
        "SELECT o.object_key, o.etag, b.size_bytes, o.last_modified_ms, b.checksums_json
         FROM objects o JOIN blobs b ON b.storage_id = o.storage_id
         WHERE o.bucket_id = ?1 AND o.object_key {op} ?2 AND (?3 IS NULL OR o.object_key < ?3)
         ORDER BY o.object_key LIMIT ?4"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map(
            params![bucket.as_bytes().as_slice(), from, upper, limit as i64],
            |r| {
                let cs: String = r.get(4)?;
                Ok(ListedObject {
                    key: r.get(0)?,
                    etag: r.get(1)?,
                    size: r.get::<_, i64>(2)? as u64,
                    last_modified_ms: r.get(3)?,
                    checksum: checksum_from_json(&cs),
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Multipart uploads

const UPLOAD_COLS: &str =
    "upload_id, bucket_id, object_key, state, creator_key_id, headers_json, user_metadata_json,
    checksum_algorithm, checksum_type, checksum_explicit, created_at_ms, last_activity_ms,
    completion_fingerprint, result_json, receipt_expires_at_ms";

fn upload_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UploadRow> {
    let state: String = r.get(3)?;
    let headers: String = r.get(5)?;
    let meta: String = r.get(6)?;
    let alg: String = r.get(7)?;
    let kind: String = r.get(8)?;
    Ok(UploadRow {
        upload_id: r.get(0)?,
        bucket_id: bucket_id(r.get(1)?)?,
        key: r.get(2)?,
        state: UploadState::parse(&state).map_err(|_| invalid("state"))?,
        creator_key_id: r.get(4)?,
        headers: json_or_default(&headers),
        user_metadata: json_or_default(&meta),
        checksum_algorithm: Algorithm::parse(&alg).ok_or_else(|| invalid("checksum_algorithm"))?,
        checksum_type: ChecksumType::parse(&kind).ok_or_else(|| invalid("checksum_type"))?,
        checksum_explicit: r.get::<_, i64>(9)? == 1,
        created_at_ms: r.get(10)?,
        last_activity_ms: r.get(11)?,
        completion_fingerprint: r.get(12)?,
        result_json: r.get(13)?,
        receipt_expires_at_ms: r.get(14)?,
    })
}

pub fn get_upload(conn: &Connection, upload_id: &str) -> Result<Option<UploadRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {UPLOAD_COLS} FROM multipart_uploads WHERE upload_id = ?1"),
            [upload_id],
            upload_row,
        )
        .optional()?)
}

#[derive(Debug, PartialEq, Eq)]
pub enum CreateUpload {
    Created,
    NoSuchBucket,
    TooManyUploads,
}

pub fn create_upload(conn: &Connection, u: &UploadRow, max_active: u64) -> Result<CreateUpload> {
    if bucket_by_id(conn, &u.bucket_id)?.is_none() {
        return Ok(CreateUpload::NoSuchBucket);
    }
    let active: i64 = conn.query_row(
        "SELECT count(*) FROM multipart_uploads WHERE state IN ('open', 'completing')",
        [],
        |r| r.get(0),
    )?;
    if active as u64 >= max_active {
        return Ok(CreateUpload::TooManyUploads);
    }
    let headers = serde_json::to_string(&u.headers).map_err(|e| Error::other(e.to_string()))?;
    let meta = serde_json::to_string(&u.user_metadata).map_err(|e| Error::other(e.to_string()))?;
    conn.execute(
        "INSERT INTO multipart_uploads(upload_id, bucket_id, object_key, state, creator_key_id, headers_json,
             user_metadata_json, checksum_algorithm, checksum_type, checksum_explicit, created_at_ms, last_activity_ms)
         VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            u.upload_id,
            u.bucket_id.as_bytes().as_slice(),
            u.key,
            u.creator_key_id,
            headers,
            meta,
            u.checksum_algorithm.as_str(),
            u.checksum_type.as_str(),
            u.checksum_explicit as i64,
            u.created_at_ms
        ],
    )?;
    Ok(CreateUpload::Created)
}

#[derive(Debug, PartialEq, Eq)]
pub enum PartCommit {
    Committed { replaced_size: Option<u64> },
    UploadNotOpen,
}

/// Install a part mapping for an OPEN upload targeting exactly this bucket/key.
#[allow(clippy::too_many_arguments)]
pub fn commit_part(
    conn: &Connection,
    upload_id: &str,
    bucket: &BucketId,
    key: &[u8],
    part_number: u32,
    blob: &BlobFinal,
    etag: &str,
    now_ms: i64,
    garbage_after_ms: i64,
) -> Result<PartCommit> {
    let open: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM multipart_uploads WHERE upload_id = ?1 AND state = 'open' AND bucket_id = ?2 AND object_key = ?3",
            params![upload_id, bucket.as_bytes().as_slice(), key],
            |r| r.get(0),
        )
        .optional()?;
    if open.is_none() {
        return Ok(PartCommit::UploadNotOpen);
    }
    finalize_blob(conn, blob, BlobArea::Part)?;
    let old: Option<(Vec<u8>, i64)> = conn
        .query_row(
            "SELECT p.storage_id, b.size_bytes FROM multipart_parts p JOIN blobs b ON b.storage_id = p.storage_id
             WHERE p.upload_id = ?1 AND p.part_number = ?2",
            params![upload_id, part_number],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let sid = blob.storage_id.as_bytes().as_slice();
    let replaced_size = match old {
        Some((old_sid, size)) => {
            conn.execute(
                "UPDATE multipart_parts SET storage_id = ?3, etag = ?4, last_modified_ms = ?5
                 WHERE upload_id = ?1 AND part_number = ?2",
                params![upload_id, part_number, sid, etag, now_ms],
            )?;
            garbage_ready_blob(conn, &storage_id(old_sid)?, garbage_after_ms)?;
            Some(size as u64)
        }
        None => {
            conn.execute(
                "INSERT INTO multipart_parts(upload_id, part_number, storage_id, etag, last_modified_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![upload_id, part_number, sid, etag, now_ms],
            )?;
            None
        }
    };
    conn.execute(
        "UPDATE multipart_uploads SET last_activity_ms = ?2 WHERE upload_id = ?1",
        params![upload_id, now_ms],
    )?;
    Ok(PartCommit::Committed { replaced_size })
}

fn part_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PartRow> {
    let md5: Vec<u8> = r.get(4)?;
    let cs: String = r.get(5)?;
    Ok(PartRow {
        part_number: r.get(0)?,
        storage_id: storage_id(r.get(1)?)?,
        etag: r.get(2)?,
        size: r.get::<_, i64>(3)? as u64,
        md5: md5.try_into().map_err(|_| invalid("md5"))?,
        checksum: checksum_from_json(&cs),
        last_modified_ms: r.get(6)?,
    })
}

/// Committed parts with number greater than `after`, ascending.
pub fn list_parts(
    conn: &Connection,
    upload_id: &str,
    after: u32,
    limit: usize,
) -> Result<Vec<PartRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT p.part_number, p.storage_id, p.etag, b.size_bytes, b.md5, b.checksums_json, p.last_modified_ms
         FROM multipart_parts p JOIN blobs b ON b.storage_id = p.storage_id
         WHERE p.upload_id = ?1 AND p.part_number > ?2 ORDER BY p.part_number LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![upload_id, after, limit as i64], part_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, PartialEq, Eq)]
pub enum BeginCompletion {
    Started,
    NotOpen,
}

/// OPEN -> COMPLETING with the canonical manifest, registering the WRITING
/// output blob in the same transaction.
pub fn begin_completion(
    conn: &Connection,
    upload_id: &str,
    fingerprint: &[u8; 32],
    manifest_json: &str,
    output: &StorageId,
    now_ms: i64,
) -> Result<BeginCompletion> {
    if !register_blob(conn, output, BlobArea::Object, now_ms)? {
        return Err(Error::integrity(
            "storage ID collision while starting completion",
        ));
    }
    let n = conn.execute(
        "UPDATE multipart_uploads SET state = 'completing', completion_fingerprint = ?2,
             completion_manifest_json = ?3, completion_output_id = ?4
         WHERE upload_id = ?1 AND state = 'open'",
        params![
            upload_id,
            fingerprint.as_slice(),
            manifest_json,
            output.as_bytes().as_slice()
        ],
    )?;
    if n != 1 {
        conn.execute(
            "DELETE FROM blobs WHERE storage_id = ?1 AND state = 'writing'",
            [output.as_bytes().as_slice()],
        )?;
        return Ok(BeginCompletion::NotOpen);
    }
    Ok(BeginCompletion::Started)
}

/// COMPLETING -> OPEN after a definite failure; the output blob becomes garbage.
pub fn revert_completion(
    conn: &Connection,
    upload_id: &str,
    output: &StorageId,
    garbage_after_ms: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE multipart_uploads SET state = 'open', completion_fingerprint = NULL,
             completion_manifest_json = NULL, completion_output_id = NULL
         WHERE upload_id = ?1 AND state = 'completing'",
        [upload_id],
    )?;
    abandon_blob(conn, output, garbage_after_ms)?;
    Ok(())
}

#[derive(Debug)]
pub struct CompletionCommit {
    pub outcome: ObjectCommit,
    pub released_part_bytes: u64,
}

/// Atomically publish the assembled object, close the upload with a receipt,
/// and release every part. Non-committing outcomes revert to OPEN in the same
/// transaction.
pub fn finish_completion(
    conn: &Connection,
    upload_id: &str,
    new: &NewObject,
    cond: &WriteConditions,
    result_json: &str,
    receipt_expires_ms: i64,
    garbage_after_ms: i64,
) -> Result<CompletionCommit> {
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM multipart_uploads WHERE upload_id = ?1",
            [upload_id],
            |r| r.get(0),
        )
        .optional()?;
    if state.as_deref() != Some("completing") {
        return Err(Error::integrity(
            "upload left COMPLETING while its finalization guard was held",
        ));
    }
    let outcome = commit_object(conn, new, cond, garbage_after_ms)?;
    if !matches!(outcome, ObjectCommit::Committed { .. }) {
        revert_completion(conn, upload_id, &new.blob.storage_id, garbage_after_ms)?;
        return Ok(CompletionCommit {
            outcome,
            released_part_bytes: 0,
        });
    }
    let released: i64 = conn.query_row(
        "SELECT coalesce(sum(b.size_bytes), 0) FROM multipart_parts p JOIN blobs b ON b.storage_id = p.storage_id
         WHERE p.upload_id = ?1",
        [upload_id],
        |r| r.get(0),
    )?;
    release_parts(conn, upload_id, garbage_after_ms)?;
    conn.execute(
        "UPDATE multipart_uploads SET state = 'completed', result_json = ?2, closed_at_ms = ?3,
             receipt_expires_at_ms = ?4, completion_output_id = NULL
         WHERE upload_id = ?1",
        params![upload_id, result_json, new.now_ms, receipt_expires_ms],
    )?;
    Ok(CompletionCommit {
        outcome,
        released_part_bytes: released.max(0) as u64,
    })
}

fn release_parts(conn: &Connection, upload_id: &str, garbage_after_ms: i64) -> Result<()> {
    conn.execute(
        "UPDATE blobs SET state = 'garbage', garbage_after_ms = ?2
         WHERE storage_id IN (SELECT storage_id FROM multipart_parts WHERE upload_id = ?1) AND state = 'ready'",
        params![upload_id, garbage_after_ms],
    )?;
    conn.execute(
        "DELETE FROM multipart_parts WHERE upload_id = ?1",
        [upload_id],
    )?;
    Ok(())
}

/// OPEN -> ABORTED. Returns released part bytes, or None if not OPEN.
pub fn abort_upload(
    conn: &Connection,
    upload_id: &str,
    now_ms: i64,
    receipt_expires_ms: i64,
    garbage_after_ms: i64,
) -> Result<Option<u64>> {
    let open: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM multipart_uploads WHERE upload_id = ?1 AND state = 'open'",
            [upload_id],
            |r| r.get(0),
        )
        .optional()?;
    if open.is_none() {
        return Ok(None);
    }
    let released: i64 = conn.query_row(
        "SELECT coalesce(sum(b.size_bytes), 0) FROM multipart_parts p JOIN blobs b ON b.storage_id = p.storage_id
         WHERE p.upload_id = ?1",
        [upload_id],
        |r| r.get(0),
    )?;
    release_parts(conn, upload_id, garbage_after_ms)?;
    conn.execute(
        "UPDATE multipart_uploads SET state = 'aborted', closed_at_ms = ?2, receipt_expires_at_ms = ?3
         WHERE upload_id = ?1",
        params![upload_id, now_ms, receipt_expires_ms],
    )?;
    Ok(Some(released.max(0) as u64))
}

/// Active (OPEN/COMPLETING) uploads ordered by (key, upload_id) after a position.
pub fn uploads_from(
    conn: &Connection,
    bucket: &BucketId,
    from_key: &[u8],
    from_upload: &str,
    inclusive_key: bool,
    upper: Option<&[u8]>,
    limit: usize,
) -> Result<Vec<UploadRow>> {
    let cond = if inclusive_key {
        "object_key >= ?2"
    } else {
        "(object_key > ?2 OR (object_key = ?2 AND upload_id > ?3))"
    };
    let sql = format!(
        "SELECT {UPLOAD_COLS} FROM multipart_uploads
         WHERE bucket_id = ?1 AND state IN ('open', 'completing') AND {cond} AND (?4 IS NULL OR object_key < ?4)
         ORDER BY object_key, upload_id LIMIT ?5"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map(
            params![
                bucket.as_bytes().as_slice(),
                from_key,
                from_upload,
                upper,
                limit as i64
            ],
            upload_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn expired_open_uploads(
    conn: &Connection,
    cutoff_ms: i64,
    limit: usize,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT upload_id FROM multipart_uploads WHERE state = 'open' AND last_activity_ms < ?1
         ORDER BY last_activity_ms LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![cutoff_ms, limit as i64], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn delete_expired_receipts(conn: &Connection, now_ms: i64, limit: usize) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM multipart_uploads WHERE upload_id IN (
             SELECT upload_id FROM multipart_uploads
             WHERE state IN ('completed', 'aborted') AND receipt_expires_at_ms <= ?1 LIMIT ?2)",
        params![now_ms, limit as i64],
    )?)
}

pub fn active_upload_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM multipart_uploads WHERE state IN ('open', 'completing')",
        [],
        |r| r.get(0),
    )?)
}

// ---------------------------------------------------------------------------
// Recovery and verification

#[derive(Debug, Default, Clone, Serialize)]
pub struct RecoveryReport {
    pub reopened_uploads: usize,
    pub reclaimed_writing_blobs: usize,
}

/// Conservative restart recovery. No previous task owns any WRITING blob or
/// COMPLETING upload after a restart.
pub fn recover(conn: &Connection, now_ms: i64) -> Result<RecoveryReport> {
    let referenced_writing: i64 = conn.query_row(
        "SELECT count(*) FROM blobs b WHERE b.state = 'writing' AND (
             EXISTS (SELECT 1 FROM objects o WHERE o.storage_id = b.storage_id)
             OR EXISTS (SELECT 1 FROM multipart_parts p WHERE p.storage_id = b.storage_id))",
        [],
        |r| r.get(0),
    )?;
    if referenced_writing > 0 {
        return Err(Error::integrity(format!(
            "{referenced_writing} WRITING blobs are referenced by committed metadata"
        )));
    }
    let reopened = conn.execute(
        "UPDATE multipart_uploads SET state = 'open', completion_fingerprint = NULL,
             completion_manifest_json = NULL, completion_output_id = NULL
         WHERE state = 'completing'",
        [],
    )?;
    let reclaimed = conn.execute(
        "UPDATE blobs SET state = 'garbage', garbage_after_ms = ?1 WHERE state = 'writing'",
        [now_ms],
    )?;
    Ok(RecoveryReport {
        reopened_uploads: reopened,
        reclaimed_writing_blobs: reclaimed,
    })
}

/// Cross-table invariant checks (read-only). Returns human-readable findings.
pub fn invariant_violations(conn: &Connection) -> Result<Vec<String>> {
    let checks: &[(&str, &str)] = &[
        (
            "objects referencing a blob that is not a READY object blob",
            "SELECT count(*) FROM objects o JOIN blobs b ON b.storage_id = o.storage_id
             WHERE b.state != 'ready' OR b.area != 'object'",
        ),
        (
            "parts referencing a blob that is not a READY part blob",
            "SELECT count(*) FROM multipart_parts p JOIN blobs b ON b.storage_id = p.storage_id
             WHERE b.state != 'ready' OR b.area != 'part'",
        ),
        (
            "blobs referenced by both an object and a part",
            "SELECT count(*) FROM objects o JOIN multipart_parts p ON p.storage_id = o.storage_id",
        ),
        (
            "READY blobs without any reference",
            "SELECT count(*) FROM blobs b WHERE b.state = 'ready'
               AND NOT EXISTS (SELECT 1 FROM objects o WHERE o.storage_id = b.storage_id)
               AND NOT EXISTS (SELECT 1 FROM multipart_parts p WHERE p.storage_id = b.storage_id)",
        ),
        (
            "parts belonging to uploads that are not OPEN/COMPLETING",
            "SELECT count(*) FROM multipart_parts p JOIN multipart_uploads u ON u.upload_id = p.upload_id
             WHERE u.state NOT IN ('open', 'completing')",
        ),
        (
            "buckets whose counters disagree with their objects",
            "SELECT count(*) FROM buckets k WHERE k.object_count != (SELECT count(*) FROM objects o WHERE o.bucket_id = k.id)
               OR k.logical_bytes != (SELECT coalesce(sum(b.size_bytes), 0) FROM objects o JOIN blobs b ON b.storage_id = o.storage_id WHERE o.bucket_id = k.id)",
        ),
    ];
    let mut out = Vec::new();
    for (what, sql) in checks {
        let n: i64 = conn.query_row(sql, [], |r| r.get(0))?;
        if n > 0 {
            out.push(format!("{n} {what}"));
        }
    }
    Ok(out)
}

/// All referenced blobs (objects and parts) for full verification and backup.
/// (storage ID, area, size, internal SHA-256) of a referenced blob.
pub type ReferencedBlob = (StorageId, BlobArea, u64, [u8; 32]);

pub fn referenced_blobs(conn: &Connection) -> Result<Vec<ReferencedBlob>> {
    let mut stmt = conn.prepare(
        "SELECT b.storage_id, b.area, b.size_bytes, b.sha256 FROM blobs b
         WHERE b.state = 'ready' AND (EXISTS (SELECT 1 FROM objects o WHERE o.storage_id = b.storage_id)
             OR EXISTS (SELECT 1 FROM multipart_parts p WHERE p.storage_id = b.storage_id))
         ORDER BY b.storage_id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let sha: Vec<u8> = r.get(3)?;
            Ok((
                storage_id(r.get(0)?)?,
                BlobArea::parse(&r.get::<_, String>(1)?)?,
                r.get::<_, i64>(2)? as u64,
                sha.try_into().map_err(|_| invalid("sha256"))?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Whether a storage ID is tracked at all (any state).
pub fn blob_exists(conn: &Connection, id: &StorageId) -> Result<bool> {
    Ok(blob_state(conn, id)?.is_some())
}

// ---------------------------------------------------------------------------
// Access keys, grants, and the admin audit log

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRow {
    pub bucket: String,
    pub prefix: Vec<u8>,
    pub actions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CredentialRow {
    pub id: String,
    pub secret: Sealed,
    /// Previous secret and the time it stops being accepted.
    pub previous: Option<(Sealed, i64)>,
    pub enabled: bool,
    pub description: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub global: Vec<String>,
    pub grants: Vec<GrantRow>,
}

const CREDENTIAL_COLS: &str = "access_key_id, secret_scheme, secret_nonce, secret_value, \
     previous_scheme, previous_nonce, previous_value, previous_expires_at_ms, \
     enabled, description, created_at_ms, updated_at_ms, expires_at_ms";

fn credential_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CredentialRow> {
    let previous = match (
        r.get::<_, Option<i64>>(4)?,
        r.get::<_, Option<Vec<u8>>>(6)?,
        r.get::<_, Option<i64>>(7)?,
    ) {
        (Some(scheme), Some(value), Some(until)) => Some((
            Sealed {
                scheme,
                nonce: r.get(5)?,
                value,
            },
            until,
        )),
        _ => None,
    };
    Ok(CredentialRow {
        id: r.get(0)?,
        secret: Sealed {
            scheme: r.get(1)?,
            nonce: r.get(2)?,
            value: r.get(3)?,
        },
        previous,
        enabled: r.get::<_, i64>(8)? != 0,
        description: r.get(9)?,
        created_at_ms: r.get(10)?,
        updated_at_ms: r.get(11)?,
        expires_at_ms: r.get(12)?,
        global: Vec::new(),
        grants: Vec::new(),
    })
}

/// Every access key with its grants, ordered by id.
pub fn load_credentials(conn: &Connection) -> Result<Vec<CredentialRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CREDENTIAL_COLS} FROM credentials ORDER BY access_key_id"
    ))?;
    let mut rows: BTreeMap<String, CredentialRow> = stmt
        .query_map([], credential_row)?
        .map(|r| r.map(|c| (c.id.clone(), c)))
        .collect::<Result<_, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT access_key_id, grant_name FROM credential_global_grants ORDER BY access_key_id, grant_name",
    )?;
    for g in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (id, name) = g?;
        if let Some(c) = rows.get_mut(&id) {
            c.global.push(name);
        }
    }
    let mut stmt = conn.prepare(
        "SELECT access_key_id, bucket, prefix, actions_json FROM credential_grants ORDER BY access_key_id, bucket, prefix",
    )?;
    for g in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Vec<u8>>(2)?,
            r.get::<_, String>(3)?,
        ))
    })? {
        let (id, bucket, prefix, actions) = g?;
        if let Some(c) = rows.get_mut(&id) {
            c.grants.push(GrantRow {
                bucket,
                prefix,
                actions: json_or_default(&actions),
            });
        }
    }
    Ok(rows.into_values().collect())
}

pub fn load_credential(conn: &Connection, id: &str) -> Result<Option<CredentialRow>> {
    Ok(load_credentials(conn)?.into_iter().find(|c| c.id == id))
}

pub fn credential_exists(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM credentials WHERE access_key_id = ?1",
            [id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub struct NewCredential<'a> {
    pub id: &'a str,
    pub secret: &'a Sealed,
    pub enabled: bool,
    pub description: &'a str,
    pub expires_at_ms: Option<i64>,
    pub global: &'a [&'a str],
    pub grants: &'a [GrantRow],
    pub now_ms: i64,
}

pub fn insert_credential(conn: &Connection, c: &NewCredential<'_>) -> Result<()> {
    conn.execute(
        "INSERT INTO credentials(access_key_id, secret_scheme, secret_nonce, secret_value, enabled, description, created_at_ms, updated_at_ms, expires_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
        params![
            c.id,
            c.secret.scheme,
            c.secret.nonce,
            c.secret.value,
            i64::from(c.enabled),
            c.description,
            c.now_ms,
            c.expires_at_ms
        ],
    )?;
    for g in c.global {
        add_global_grant(conn, c.id, g)?;
    }
    for g in c.grants {
        upsert_grant(conn, c.id, g)?;
    }
    Ok(())
}

fn touch_credential(conn: &Connection, id: &str, now_ms: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE credentials SET updated_at_ms = ?2 WHERE access_key_id = ?1",
        params![id, now_ms],
    )? == 1)
}

pub fn set_credential_enabled(
    conn: &Connection,
    id: &str,
    enabled: bool,
    now_ms: i64,
) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE credentials SET enabled = ?2, updated_at_ms = ?3 WHERE access_key_id = ?1",
        params![id, i64::from(enabled), now_ms],
    )? == 1)
}

pub fn set_credential_description(
    conn: &Connection,
    id: &str,
    d: &str,
    now_ms: i64,
) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE credentials SET description = ?2, updated_at_ms = ?3 WHERE access_key_id = ?1",
        params![id, d, now_ms],
    )? == 1)
}

pub fn set_credential_expiry(
    conn: &Connection,
    id: &str,
    expires: Option<i64>,
    now_ms: i64,
) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE credentials SET expires_at_ms = ?2, updated_at_ms = ?3 WHERE access_key_id = ?1",
        params![id, expires, now_ms],
    )? == 1)
}

pub fn delete_credential(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.execute("DELETE FROM credentials WHERE access_key_id = ?1", [id])? == 1)
}

pub fn delete_all_credentials(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM credentials", [])?)
}

/// Replace the secret; keep `previous` (if any) valid until its deadline.
pub fn rotate_credential(
    conn: &Connection,
    id: &str,
    new: &Sealed,
    previous: Option<(&Sealed, i64)>,
    now_ms: i64,
) -> Result<bool> {
    let (ps, pn, pv, pe) = match previous {
        Some((s, until)) => (
            Some(s.scheme),
            s.nonce.clone(),
            Some(s.value.clone()),
            Some(until),
        ),
        None => (None, None, None, None),
    };
    Ok(conn.execute(
        "UPDATE credentials SET secret_scheme = ?2, secret_nonce = ?3, secret_value = ?4,
             previous_scheme = ?5, previous_nonce = ?6, previous_value = ?7, previous_expires_at_ms = ?8,
             updated_at_ms = ?9
         WHERE access_key_id = ?1",
        params![id, new.scheme, new.nonce, new.value, ps, pn, pv, pe, now_ms],
    )? == 1)
}

/// Rewrite stored secrets in another encoding (protection mode change).
pub fn reencode_credential(
    conn: &Connection,
    id: &str,
    secret: &Sealed,
    previous: Option<&Sealed>,
) -> Result<()> {
    conn.execute(
        "UPDATE credentials SET secret_scheme = ?2, secret_nonce = ?3, secret_value = ?4 WHERE access_key_id = ?1",
        params![id, secret.scheme, secret.nonce, secret.value],
    )?;
    if let Some(p) = previous {
        conn.execute(
            "UPDATE credentials SET previous_scheme = ?2, previous_nonce = ?3, previous_value = ?4 WHERE access_key_id = ?1",
            params![id, p.scheme, p.nonce, p.value],
        )?;
    }
    Ok(())
}

/// Forget previous secrets whose rotation grace period has ended.
pub fn clear_expired_previous_secrets(conn: &Connection, now_ms: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE credentials SET previous_scheme = NULL, previous_nonce = NULL, previous_value = NULL, previous_expires_at_ms = NULL
         WHERE previous_expires_at_ms IS NOT NULL AND previous_expires_at_ms <= ?1",
        [now_ms],
    )?)
}

/// Insert or replace the grant for (key, bucket, prefix).
pub fn upsert_grant(conn: &Connection, id: &str, g: &GrantRow) -> Result<()> {
    let actions = serde_json::to_string(&g.actions).map_err(|e| Error::other(e.to_string()))?;
    conn.execute(
        "INSERT INTO credential_grants(access_key_id, bucket, prefix, actions_json) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(access_key_id, bucket, prefix) DO UPDATE SET actions_json = excluded.actions_json",
        params![id, g.bucket, g.prefix, actions],
    )?;
    Ok(())
}

pub fn remove_grant(conn: &Connection, id: &str, bucket: &str, prefix: &[u8]) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM credential_grants WHERE access_key_id = ?1 AND bucket = ?2 AND prefix = ?3",
        params![id, bucket, prefix],
    )? == 1)
}

pub fn add_global_grant(conn: &Connection, id: &str, name: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO credential_global_grants(access_key_id, grant_name) VALUES (?1, ?2)",
        params![id, name],
    )?;
    Ok(())
}

pub fn remove_global_grant(conn: &Connection, id: &str, name: &str) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM credential_global_grants WHERE access_key_id = ?1 AND grant_name = ?2",
        params![id, name],
    )? == 1)
}

/// Mark a key as changed (after grant edits).
pub fn credential_touched(conn: &Connection, id: &str, now_ms: i64) -> Result<bool> {
    touch_credential(conn, id, now_ms)
}

/// Enabled, unexpired keys holding the global admin grant.
pub fn usable_admin_count(conn: &Connection, now_ms: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM credentials c JOIN credential_global_grants g
           ON g.access_key_id = c.access_key_id AND g.grant_name = 'admin'
         WHERE c.enabled = 1 AND (c.expires_at_ms IS NULL OR c.expires_at_ms > ?1)",
        [now_ms],
        |r| r.get(0),
    )?)
}

/// (stored with scheme 0, stored with scheme 1) counts, current and previous secrets.
pub fn secret_scheme_counts(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT
           (SELECT count(*) FROM credentials WHERE secret_scheme = 0)
             + (SELECT count(*) FROM credentials WHERE previous_scheme = 0),
           (SELECT count(*) FROM credentials WHERE secret_scheme = 1)
             + (SELECT count(*) FROM credentials WHERE previous_scheme = 1)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRow {
    pub id: i64,
    pub at_ms: i64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: serde_json::Value,
}

pub fn insert_audit(
    conn: &Connection,
    at_ms: i64,
    actor: &str,
    action: &str,
    target: &str,
    detail: &serde_json::Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO admin_audit(at_ms, actor, action, target, detail_json) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![at_ms, actor, action, target, detail.to_string()],
    )?;
    Ok(())
}

/// Most recent audit entries first.
pub fn list_audit(conn: &Connection, limit: usize) -> Result<Vec<AuditRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, at_ms, actor, action, target, detail_json FROM admin_audit ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([limit as i64], |r| {
            Ok(AuditRow {
                id: r.get(0)?,
                at_ms: r.get(1)?,
                actor: r.get(2)?,
                action: r.get(3)?,
                target: r.get(4)?,
                detail: serde_json::from_str(&r.get::<_, String>(5)?)
                    .unwrap_or(serde_json::Value::Null),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{create_database, migrations};

    fn db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = create_database(&dir.path().join("m.sqlite3")).unwrap();
        migrations::apply(&conn).unwrap();
        init_store_meta(&conn, "us-east-1", 1).unwrap();
        (dir, conn)
    }

    fn blob(size: u64) -> BlobFinal {
        BlobFinal {
            storage_id: StorageId::random(),
            size,
            md5: [1; 16],
            sha256: [2; 32],
            checksum: None,
            part_sizes: None,
        }
    }

    fn new_object(bucket: BucketId, key: &[u8], size: u64, conn: &Connection) -> NewObject {
        let b = blob(size);
        assert!(register_blob(conn, &b.storage_id, BlobArea::Object, 1).unwrap());
        NewObject {
            bucket_id: bucket,
            key: key.to_vec(),
            blob: b,
            etag: format!("etag{size}"),
            headers: ContentHeaders::default(),
            user_metadata: UserMetadata::new(),
            now_ms: 10,
        }
    }

    #[test]
    fn store_meta_roundtrip() {
        let (_d, conn) = db();
        let m = load_store_meta(&conn).unwrap();
        assert_eq!(m.region, "us-east-1");
        assert_eq!(m.cursor_key.len(), 32);
    }

    #[test]
    fn overwrite_updates_counters_and_garbage() {
        let (_d, conn) = db();
        let CreateBucket::Created(b) = create_bucket(&conn, "docs", 1, 10).unwrap() else {
            panic!()
        };
        let n1 = new_object(b, b"k", 100, &conn);
        let r = commit_object(&conn, &n1, &WriteConditions::default(), 5).unwrap();
        assert!(matches!(r, ObjectCommit::Committed { replaced: None, .. }));
        let n2 = new_object(b, b"k", 40, &conn);
        let r = commit_object(&conn, &n2, &WriteConditions::default(), 5).unwrap();
        assert!(matches!(
            r,
            ObjectCommit::Committed {
                replaced: Some((_, 100)),
                ..
            }
        ));
        let row = bucket_by_id(&conn, &b).unwrap().unwrap();
        assert_eq!((row.object_count, row.logical_bytes), (1, 40));
        assert_eq!(
            blob_state(&conn, &n1.blob.storage_id).unwrap().as_deref(),
            Some("garbage")
        );
        assert!(invariant_violations(&conn).unwrap().is_empty());
        let d = delete_object(&conn, &b, b"k", 5).unwrap();
        assert!(matches!(d, DeleteOutcome::Deleted { size: 40, .. }));
        assert_eq!(
            delete_object(&conn, &b, b"k", 5).unwrap(),
            DeleteOutcome::Absent
        );
        let row = bucket_by_id(&conn, &b).unwrap().unwrap();
        assert_eq!((row.object_count, row.logical_bytes), (0, 0));
        assert!(invariant_violations(&conn).unwrap().is_empty());
    }

    #[test]
    fn conditions_and_quota_are_checked_at_commit() {
        let (_d, conn) = db();
        let CreateBucket::Created(b) = create_bucket(&conn, "docs", 1, 10).unwrap() else {
            panic!()
        };
        let cond = WriteConditions {
            if_match: Some(vec!["x".into()]),
            ..Default::default()
        };
        let n = new_object(b, b"k", 1, &conn);
        assert_eq!(
            commit_object(&conn, &n, &cond, 5).unwrap(),
            ObjectCommit::NoSuchKey
        );
        commit_object(&conn, &n, &WriteConditions::default(), 5).unwrap();
        let n2 = new_object(b, b"k", 1, &conn);
        let none = WriteConditions {
            if_none_match_any: true,
            ..Default::default()
        };
        assert_eq!(
            commit_object(&conn, &n2, &none, 5).unwrap(),
            ObjectCommit::PreconditionFailed
        );
        assert_eq!(
            commit_object(&conn, &n2, &cond, 5).unwrap(),
            ObjectCommit::PreconditionFailed
        );
        let ok = WriteConditions {
            if_match: Some(vec!["etag1".into()]),
            ..Default::default()
        };
        assert!(matches!(
            commit_object(&conn, &n2, &ok, 5).unwrap(),
            ObjectCommit::Committed { .. }
        ));
        set_bucket_quota(&conn, "docs", Some(10)).unwrap();
        let big = new_object(b, b"other", 20, &conn);
        assert_eq!(
            commit_object(&conn, &big, &WriteConditions::default(), 5).unwrap(),
            ObjectCommit::QuotaExceeded
        );
        // Shrinking or same-size overwrites are always allowed.
        set_bucket_quota(&conn, "docs", Some(0)).unwrap();
        let small = new_object(b, b"k", 1, &conn);
        assert!(matches!(
            commit_object(&conn, &small, &WriteConditions::default(), 5).unwrap(),
            ObjectCommit::Committed { .. }
        ));
    }

    #[test]
    fn bucket_deletion_requires_empty() {
        let (_d, conn) = db();
        let CreateBucket::Created(b) = create_bucket(&conn, "docs", 1, 10).unwrap() else {
            panic!()
        };
        assert_eq!(
            create_bucket(&conn, "docs", 1, 10).unwrap(),
            CreateBucket::AlreadyExists
        );
        assert_eq!(
            create_bucket(&conn, "other", 1, 1).unwrap(),
            CreateBucket::TooManyBuckets
        );
        let n = new_object(b, b"k", 1, &conn);
        commit_object(&conn, &n, &WriteConditions::default(), 5).unwrap();
        assert_eq!(delete_bucket(&conn, &b).unwrap(), DeleteBucket::NotEmpty);
        delete_object(&conn, &b, b"k", 5).unwrap();
        assert_eq!(delete_bucket(&conn, &b).unwrap(), DeleteBucket::Deleted);
        assert_eq!(
            delete_bucket(&conn, &b).unwrap(),
            DeleteBucket::NoSuchBucket
        );
    }

    #[test]
    fn recovery_reclaims_writing_and_reopens_completing() {
        let (_d, conn) = db();
        let CreateBucket::Created(b) = create_bucket(&conn, "docs", 1, 10).unwrap() else {
            panic!()
        };
        let up = UploadRow {
            upload_id: crate::ids::UploadId::random().to_string(),
            bucket_id: b,
            key: b"k".to_vec(),
            state: UploadState::Open,
            creator_key_id: "x".into(),
            headers: ContentHeaders::default(),
            user_metadata: UserMetadata::new(),
            checksum_algorithm: Algorithm::Crc64Nvme,
            checksum_type: ChecksumType::FullObject,
            checksum_explicit: false,
            created_at_ms: 1,
            last_activity_ms: 1,
            completion_fingerprint: None,
            result_json: None,
            receipt_expires_at_ms: None,
        };
        assert_eq!(
            create_upload(&conn, &up, 10).unwrap(),
            CreateUpload::Created
        );
        let out = StorageId::random();
        assert_eq!(
            begin_completion(&conn, &up.upload_id, &[0; 32], "[]", &out, 2).unwrap(),
            BeginCompletion::Started
        );
        let stray = StorageId::random();
        register_blob(&conn, &stray, BlobArea::Object, 2).unwrap();
        let r = recover(&conn, 3).unwrap();
        assert_eq!(r.reopened_uploads, 1);
        assert_eq!(r.reclaimed_writing_blobs, 2);
        assert_eq!(
            get_upload(&conn, &up.upload_id).unwrap().unwrap().state,
            UploadState::Open
        );
        assert_eq!(blob_state(&conn, &out).unwrap().as_deref(), Some("garbage"));
        assert_eq!(garbage_batch(&conn, 3, 10).unwrap().len(), 2);
        assert!(delete_garbage_row(&conn, &out).unwrap());
    }

    #[test]
    fn byte_ordering_matches_reference_model() {
        let (_d, conn) = db();
        let CreateBucket::Created(b) = create_bucket(&conn, "docs", 1, 10).unwrap() else {
            panic!()
        };
        let keys: Vec<&str> = vec![
            "a",
            "a/b",
            "a b",
            "A",
            "é",
            "e\u{0301}",
            "~",
            "a%",
            "a_",
            "z",
            "\u{10348}",
            "a\tb",
        ];
        for k in &keys {
            let n = new_object(b, k.as_bytes(), 1, &conn);
            commit_object(&conn, &n, &WriteConditions::default(), 5).unwrap();
        }
        let listed: Vec<Vec<u8>> = objects_from(&conn, &b, b"", true, None, 100)
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        let mut expected: Vec<Vec<u8>> = keys.iter().map(|k| k.as_bytes().to_vec()).collect();
        expected.sort();
        assert_eq!(listed, expected);
        // `%` and `_` are literal, not LIKE wildcards.
        let upper = crate::keys::prefix_successor(b"a%");
        let l = objects_from(&conn, &b, b"a%", true, upper.as_deref(), 100).unwrap();
        assert_eq!(l.len(), 1);
    }
}
