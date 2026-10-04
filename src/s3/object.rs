//! Object operations: PutObject, GetObject/HeadObject, DeleteObject,
//! CopyObject, DeleteObjects.

use std::io::{Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use futures_util::StreamExt;
use http::{Response, StatusCode};

use super::error::{S3Error, S3Result};
use super::headers::{
    self, CondResult, content_headers, evaluate, http_date, iso8601, quote_etag, read_conditions,
    response, user_metadata, write_conditions, xml,
};
use super::integrity::ChecksumRequest;
use super::payload::Payload;
use super::xml::XmlWriter;
use super::{Cx, lookup_bucket, payload_length, read_control_body};
use crate::capacity::PermitKind;
use crate::checksums::{Algorithm, ChecksumType, StoredChecksum};
use crate::credentials::Action;
use crate::error::Error;
use crate::fsutil::Area;
use crate::ids::BucketId;
use crate::keys::ObjectKey;
use crate::metadata::queries::{
    self, BlobArea, ContentHeaders, DeleteOutcome, NewObject, ObjectCommit, ObjectRow,
    UserMetadata, WriteConditions,
};
use crate::metadata::{now_ms, with_named_write_tx};
use crate::store::{CopySource, Reconciled, StagedBlob, Store, blocking};

/// Facts for the object row, minus the blob (known only after publication).
pub struct ObjectSpec {
    pub bucket_id: BucketId,
    pub key: ObjectKey,
    pub etag: String,
    pub headers: ContentHeaders,
    pub user_metadata: UserMetadata,
    pub checksum: Option<StoredChecksum>,
}

#[derive(Debug, Clone)]
pub struct Committed {
    pub last_modified_ms: i64,
}

/// Map a non-committing outcome to its S3 error.
pub fn commit_error(outcome: &ObjectCommit) -> S3Error {
    match outcome {
        ObjectCommit::NoSuchBucket => S3Error::no_such_bucket(),
        ObjectCommit::NoSuchKey => S3Error::no_such_key(),
        ObjectCommit::PreconditionFailed => {
            S3Error::precondition_failed().with_extra("Condition", "If-Match")
        }
        ObjectCommit::QuotaExceeded => S3Error::quota_exceeded(),
        ObjectCommit::Committed { .. } => S3Error::internal(),
    }
}

/// Publish a staged object file and commit its mapping under the per-key
/// commit guard. Runs as a supervised task: it reaches a known outcome even
/// if the client disconnects.
pub async fn publish_and_commit(
    store: &Arc<Store>,
    staged: StagedBlob,
    spec: ObjectSpec,
    cond: WriteConditions,
) -> S3Result<Committed> {
    let store2 = store.clone();
    store
        .supervise(async move {
            let store = store2;
            let published = staged.publish().await?;
            let blob = published.final_facts(spec.checksum.clone());
            let id = blob.storage_id;
            let new = NewObject {
                bucket_id: spec.bucket_id,
                key: spec.key.as_bytes().to_vec(),
                blob,
                etag: spec.etag,
                headers: spec.headers,
                user_metadata: spec.user_metadata,
                now_ms: now_ms(),
            };
            let garbage_after = store.garbage_after();
            let _guard = store
                .key_locks
                .lock((spec.bucket_id, spec.key.as_bytes().to_vec()))
                .await;
            let res = store
                .db
                .write(move |c| {
                    with_named_write_tx(c, "object", |tx| {
                        queries::commit_object(tx, &new, &cond, garbage_after)
                    })
                })
                .await;
            match res {
                Ok(ObjectCommit::Committed {
                    last_modified_ms, ..
                }) => {
                    published.into_ticket().committed();
                    Ok(Committed { last_modified_ms })
                }
                Ok(other) => Err(commit_error(&other)),
                Err(Error::CommitUncertain) => match store.reconcile(id).await {
                    Reconciled::Committed => {
                        published.into_ticket().committed();
                        let (b, k) = (spec.bucket_id, spec.key.as_bytes().to_vec());
                        let row = store
                            .db
                            .read(move |c| queries::get_object(c, &b, &k))
                            .await?;
                        Ok(Committed {
                            last_modified_ms: row
                                .map(|r| r.last_modified_ms)
                                .unwrap_or_else(now_ms),
                        })
                    }
                    Reconciled::NotCommitted => Err(S3Error::internal()
                        .with_detail("commit failed (reconciled: not committed)")),
                    Reconciled::Unknown => {
                        published.into_ticket().leave_for_recovery();
                        store.halt("unreconciled metadata commit");
                        Err(S3Error::internal().with_detail("commit outcome unknown"))
                    }
                },
                Err(e) => Err(e.into()),
            }
        })
        .await
}

/// Choose the S3 checksum to store for a single-part object.
fn object_checksum(
    verified: Option<(Algorithm, Vec<u8>)>,
    requested: Option<Algorithm>,
    digests: &crate::checksums::Digests,
) -> StoredChecksum {
    if let Some((alg, d)) = verified {
        return StoredChecksum::full(alg, &d);
    }
    let alg = requested.unwrap_or(Algorithm::Crc64Nvme);
    let d = digests.get(alg).unwrap_or_default();
    StoredChecksum {
        algorithm: alg,
        kind: ChecksumType::FullObject,
        value: crate::checksums::b64(d),
    }
}

pub fn checksum_headers(c: &StoredChecksum, out: &mut Vec<(String, String)>) {
    out.push((c.algorithm.header_name().into(), c.value.clone()));
    out.push(("x-amz-checksum-type".into(), c.kind.as_str().into()));
}

pub async fn put_object(cx: &Cx, body: Body) -> S3Result<Response<Body>> {
    cx.require_object(Action::Write)?;
    let store = &cx.store;
    let key = cx.req.object_key().clone();
    let headers = content_headers(&cx.req)?;
    let meta = user_metadata(&cx.req, store.config.limits.max_user_metadata_bytes)?;
    let cond = write_conditions(&cx.req)?;
    let integrity = ChecksumRequest::parse(&cx.req, &cx.auth.payload)?;
    let len =
        payload_length(&cx.req, &cx.auth.payload)?.ok_or_else(S3Error::missing_content_length)?;
    if len > store.config.limits.max_single_put_bytes {
        return Err(S3Error::entity_too_large()
            .with_extra("ProposedSize", len.to_string())
            .with_extra(
                "MaxSizeAllowed",
                store.config.limits.max_single_put_bytes.to_string(),
            ));
    }
    let bucket = cx.bucket().await?;
    let _permit = store.capacity.acquire(PermitKind::Upload).await?;
    let mut reservation = store.capacity.reserve(len)?;
    let ticket = store.new_blob(BlobArea::Object).await?;
    let trailer = cx
        .req
        .header("x-amz-trailer")?
        .map(|t| t.trim().to_ascii_lowercase());
    let mut payload = Payload::new(
        body,
        cx.auth.payload.clone(),
        Some(cx.auth.signing.clone()),
        Some(len),
        trailer,
        cx.idle_timeout(),
    );
    let algs = integrity.algorithms(&[Algorithm::Crc64Nvme]);
    let received = store
        .receive(ticket, &mut payload, &algs, len, Some(&mut reservation))
        .await?;
    let verified = integrity.verify(&received.digests, payload.trailers())?;
    let explicit = verified.is_some() || integrity.sdk.is_some();
    let checksum = object_checksum(verified, integrity.algorithm(), &received.digests);
    let etag = hex::encode(received.digests.md5);
    let staged = received.sync().await?;
    let spec = ObjectSpec {
        bucket_id: bucket.id,
        key,
        etag: etag.clone(),
        headers,
        user_metadata: meta,
        checksum: Some(checksum.clone()),
    };
    publish_and_commit(store, staged, spec, cond).await?;
    drop(reservation);
    let mut h = vec![("etag".to_string(), quote_etag(&etag))];
    if explicit {
        checksum_headers(&checksum, &mut h);
    }
    Ok(response(StatusCode::OK, h, Body::empty()))
}

/// Error for a missing key: NoSuchKey only when the caller may list it.
fn missing_key(cx: &Cx, bucket: &str, key: &ObjectKey) -> S3Error {
    if cx.auth.credential.allows_list(bucket, key.as_bytes()) {
        S3Error::no_such_key()
    } else {
        S3Error::access_denied()
    }
}

fn object_headers(
    row: &ObjectRow,
    out: &mut Vec<(String, String)>,
    overrides: &[(&'static str, String)],
) {
    let h = &row.headers;
    let over = |name: &str| {
        overrides
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
    };
    out.push((
        "content-type".into(),
        over("content-type")
            .or_else(|| h.content_type.clone())
            .unwrap_or_else(|| "application/octet-stream".into()),
    ));
    for (name, stored) in [
        ("content-disposition", &h.content_disposition),
        ("content-encoding", &h.content_encoding),
        ("content-language", &h.content_language),
        ("cache-control", &h.cache_control),
        ("expires", &h.expires),
    ] {
        if let Some(v) = over(name).or_else(|| stored.clone()) {
            out.push((name.into(), v));
        }
    }
    out.push(("etag".into(), quote_etag(&row.etag)));
    out.push(("last-modified".into(), http_date(row.last_modified_ms)));
    out.push(("accept-ranges".into(), "bytes".into()));
    for (k, v) in &row.user_metadata {
        out.push((format!("x-amz-meta-{k}"), v.clone()));
    }
}

const OVERRIDES: &[(&str, &str)] = &[
    ("response-content-type", "content-type"),
    ("response-content-language", "content-language"),
    ("response-expires", "expires"),
    ("response-cache-control", "cache-control"),
    ("response-content-disposition", "content-disposition"),
    ("response-content-encoding", "content-encoding"),
];

pub async fn get_object(cx: &Cx, head: bool) -> S3Result<Response<Body>> {
    cx.require_object(Action::Read)?;
    let store = &cx.store;
    let key = cx.req.object_key().clone();
    let bucket = cx.bucket().await?;
    let conds = read_conditions(&cx.req, "")?;
    let range = match cx.req.header("range")? {
        Some(v) => headers::parse_range(v)?,
        None => None,
    };
    let part_number = match cx.req.q("partNumber") {
        Some(v) => {
            let n = v
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=10_000).contains(n))
                .ok_or_else(|| {
                    S3Error::invalid_argument(
                        "Part number must be an integer between 1 and 10000, inclusive",
                    )
                })?;
            if cx.req.headers.contains_key("range") {
                return Err(S3Error::invalid_request(
                    "Cannot specify both Range header and partNumber query parameter",
                ));
            }
            Some(n)
        }
        None => None,
    };
    let mut overrides = Vec::new();
    for (param, header) in OVERRIDES {
        if let Some(v) = cx.req.q(param) {
            if v.contains(['\r', '\n']) {
                return Err(S3Error::invalid_argument(format!("invalid {param}")));
            }
            overrides.push((*header, v.to_string()));
        }
    }
    let checksum_mode = cx
        .req
        .header("x-amz-checksum-mode")?
        .is_some_and(|v| v.eq_ignore_ascii_case("ENABLED"));
    let permit = if head {
        None
    } else {
        Some(store.capacity.acquire(PermitKind::Download).await?)
    };

    // Select one committed generation and open it before releasing the guard.
    let (row, file) = {
        let _guard = store
            .key_locks
            .lock((bucket.id, key.as_bytes().to_vec()))
            .await;
        let (b, k) = (bucket.id, key.as_bytes().to_vec());
        let row = store
            .db
            .read(move |c| queries::get_object(c, &b, &k))
            .await?;
        let Some(row) = row else {
            return Err(missing_key(cx, &bucket.name, &key));
        };
        let file = if head {
            None
        } else {
            let data = store.data.clone();
            let sid = row.storage_id;
            match blocking(move || data.open_read(Area::Objects, &sid)).await {
                Ok(f) => Some(f),
                Err(e) => {
                    store.integrity_fault(&format!(
                        "object file {} cannot be opened: {e}",
                        row.storage_id
                    ));
                    return Err(
                        S3Error::internal().with_detail("referenced object file unavailable")
                    );
                }
            }
        };
        (row, file)
    };
    if let Some(f) = &file {
        let len = crate::fsutil::file_len(f).map_err(Error::from)?;
        if len != row.size {
            store.integrity_fault(&format!(
                "object file {} has size {len}, expected {}",
                row.storage_id, row.size
            ));
            return Err(
                S3Error::internal().with_detail("referenced object file has unexpected size")
            );
        }
    }

    match evaluate(&conds, &row.etag, row.last_modified_ms) {
        CondResult::Proceed => {}
        CondResult::NotModified => {
            return Ok(response(
                StatusCode::NOT_MODIFIED,
                vec![
                    ("etag", quote_etag(&row.etag)),
                    ("last-modified", http_date(row.last_modified_ms)),
                ],
                Body::empty(),
            ));
        }
        CondResult::PreconditionFailed => return Err(S3Error::precondition_failed()),
    }

    let mut h = Vec::new();
    object_headers(&row, &mut h, &overrides);
    let (status, start, len) = match (part_number, &row.part_sizes) {
        (Some(n), Some(sizes)) => {
            let idx = n as usize - 1;
            if idx >= sizes.len() {
                return Err(S3Error::new(
                    "InvalidPartNumber",
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "The requested partnumber is not satisfiable",
                ));
            }
            let start: u64 = sizes[..idx].iter().sum();
            let len = sizes[idx];
            h.push(("x-amz-mp-parts-count".into(), sizes.len().to_string()));
            if len > 0 {
                h.push((
                    "content-range".into(),
                    format!("bytes {}-{}/{}", start, start + len - 1, row.size),
                ));
            }
            (StatusCode::PARTIAL_CONTENT, start, len)
        }
        (Some(n), None) => {
            if n != 1 {
                return Err(S3Error::new(
                    "InvalidPartNumber",
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "The requested partnumber is not satisfiable",
                ));
            }
            (StatusCode::OK, 0, row.size)
        }
        (None, _) => match range {
            Some(r) => {
                let (start, len) = headers::resolve_range(r, row.size)?;
                h.push((
                    "content-range".into(),
                    format!("bytes {}-{}/{}", start, start + len - 1, row.size),
                ));
                (StatusCode::PARTIAL_CONTENT, start, len)
            }
            None => {
                if checksum_mode && let Some(c) = &row.checksum {
                    checksum_headers(c, &mut h);
                }
                (StatusCode::OK, 0, row.size)
            }
        },
    };
    h.push(("content-length".into(), len.to_string()));
    if head {
        return Ok(response(status, h, Body::empty()));
    }
    let mut file = file.expect("GET opened the file");
    if start > 0 {
        file.seek(SeekFrom::Start(start)).map_err(Error::from)?;
    }
    let body = file_body(store.clone(), file, len, permit);
    Ok(response(status, h, body))
}

/// Stream exactly `len` bytes from an open file, holding the download permit
/// (and thus the descriptor budget) until the body is finished or dropped.
fn file_body(
    store: Arc<Store>,
    file: std::fs::File,
    len: u64,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Body {
    use tokio::io::AsyncReadExt;
    let reader = tokio::fs::File::from_std(file).take(len);
    let cap = store.config.limits.transfer_buffer_bytes;
    let stream = tokio_util::io::ReaderStream::with_capacity(reader, cap).map(move |chunk| {
        let _held = &permit;
        if let Ok(c) = &chunk {
            store
                .metrics
                .bytes_sent
                .fetch_add(c.len() as u64, Ordering::Relaxed);
        }
        chunk
    });
    Body::from_stream(stream)
}

pub async fn delete_object(cx: &Cx) -> S3Result<Response<Body>> {
    cx.require_object(Action::Delete)?;
    let bucket = cx.bucket().await?;
    let key = cx.req.object_key().clone();
    delete_one(&cx.store, bucket.id, &key).await?;
    Ok(response(
        StatusCode::NO_CONTENT,
        Vec::<(&str, String)>::new(),
        Body::empty(),
    ))
}

async fn delete_one(store: &Arc<Store>, bucket: BucketId, key: &ObjectKey) -> S3Result<()> {
    let k = key.as_bytes().to_vec();
    let _guard = store.key_locks.lock((bucket, k.clone())).await;
    let after = store.garbage_after();
    let res = store
        .db
        .write(move |c| {
            with_named_write_tx(c, "delete", |tx| {
                queries::delete_object(tx, &bucket, &k, after)
            })
        })
        .await?;
    match res {
        DeleteOutcome::NoSuchBucket => Err(S3Error::no_such_bucket()),
        DeleteOutcome::Absent | DeleteOutcome::Deleted { .. } => Ok(()),
    }
}

pub async fn delete_objects(cx: &Cx, body: Body) -> S3Result<Response<Body>> {
    let store = &cx.store;
    let limit = store.config.limits.max_xml_body_bytes;
    let body = read_control_body(cx, body, limit, true).await?;
    let doc = super::xml::parse(&body)?;
    if doc.name != "Delete" {
        return Err(S3Error::malformed_xml());
    }
    let quiet = doc
        .child_text("Quiet")
        .map(|v| v.trim() == "true")
        .unwrap_or(false);
    let mut keys = Vec::new();
    for o in doc.children_named("Object") {
        let k = o.child_text("Key").ok_or_else(S3Error::malformed_xml)?;
        if let Some(v) = o.child_text("VersionId")
            && v != "null"
        {
            return Err(S3Error::not_implemented(
                "Object versioning is not supported by this service.",
            ));
        }
        for c in &o.children {
            if !matches!(c.name.as_str(), "Key" | "VersionId") {
                return Err(S3Error::not_implemented(format!(
                    "DeleteObjects element {} is not supported",
                    c.name
                )));
            }
        }
        keys.push(k.to_string());
    }
    if keys.is_empty() {
        return Err(S3Error::malformed_xml());
    }
    if keys.len() > store.config.limits.max_delete_entries {
        return Err(S3Error::malformed_xml().with_detail("too many keys"));
    }
    // Validate every key before mutating anything.
    let parsed: Vec<Result<ObjectKey, &'static str>> =
        keys.iter().map(|k| ObjectKey::parse(k.clone())).collect();
    let bucket = cx.bucket().await?;
    let mut w = XmlWriter::new();
    w.root("DeleteResult");
    for (raw, key) in keys.iter().zip(parsed) {
        let result = match key {
            Err(_) => Err(S3Error::invalid_argument("Invalid key")),
            Ok(key) => {
                if !cx
                    .auth
                    .credential
                    .allows_object(&bucket.name, key.as_bytes(), Action::Delete)
                {
                    Err(S3Error::access_denied())
                } else {
                    delete_one(store, bucket.id, &key).await
                }
            }
        };
        match result {
            Ok(()) => {
                if !quiet {
                    w.open("Deleted").elem("Key", raw).close("Deleted");
                }
            }
            Err(e) => {
                w.open("Error")
                    .elem("Key", raw)
                    .elem("Code", e.code)
                    .elem("Message", &e.message)
                    .close("Error");
            }
        }
    }
    w.close("DeleteResult");
    Ok(xml(StatusCode::OK, w.finish()))
}

/// Parse `x-amz-copy-source`: `[/]bucket/key[?versionId=null]`, URL-encoded.
fn parse_copy_source(v: &str) -> S3Result<(String, ObjectKey)> {
    let (path, query) = match v.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (v, None),
    };
    if let Some(q) = query {
        match q.strip_prefix("versionId=") {
            Some("null") => {}
            _ => {
                return Err(S3Error::not_implemented(
                    "Copying a specific object version is not supported.",
                ));
            }
        }
    }
    let decoded = crate::sigv4::percent_decode(path.strip_prefix('/').unwrap_or(path));
    let decoded = String::from_utf8(decoded)
        .map_err(|_| S3Error::invalid_argument("Invalid copy source encoding"))?;
    let (b, k) = decoded.split_once('/').ok_or_else(|| {
        S3Error::invalid_argument(
            "Copy Source must mention the source bucket and key: sourcebucket/sourcekey",
        )
    })?;
    if b.contains(':') || b.starts_with("arn") {
        return Err(S3Error::not_implemented(
            "Cross-service copy sources are not supported.",
        ));
    }
    let key = ObjectKey::parse(k.to_string()).map_err(S3Error::invalid_argument)?;
    Ok((b.to_string(), key))
}

pub async fn copy_object(cx: &Cx) -> S3Result<Response<Body>> {
    cx.require_object(Action::Write)?;
    let store = &cx.store;
    let (src_bucket_name, src_key) =
        parse_copy_source(cx.req.header("x-amz-copy-source")?.unwrap_or(""))?;
    if !cx
        .auth
        .credential
        .allows_object(&src_bucket_name, src_key.as_bytes(), Action::Read)
    {
        return Err(S3Error::access_denied());
    }
    let dst_key = cx.req.object_key().clone();
    let dst_bucket = cx.bucket().await?;
    let src_bucket = lookup_bucket(store, &src_bucket_name).await?;
    let src_conds = read_conditions(&cx.req, "x-amz-copy-source-")?;
    let cond = write_conditions(&cx.req)?;
    let directive = cx
        .req
        .header("x-amz-metadata-directive")?
        .unwrap_or("COPY")
        .to_string();
    if directive != "COPY" && directive != "REPLACE" {
        return Err(S3Error::invalid_argument("Unknown metadata directive."));
    }
    let requested_alg = match cx.req.header("x-amz-checksum-algorithm")? {
        Some(a) => Some(
            Algorithm::parse(a)
                .ok_or_else(|| S3Error::invalid_request("Invalid x-amz-checksum-algorithm"))?,
        ),
        None => None,
    };
    let _permit = store.capacity.acquire(PermitKind::Copy).await?;

    // Open one committed source generation under the source key guard only.
    let (src_row, src_file) = {
        let _g = store
            .key_locks
            .lock((src_bucket.id, src_key.as_bytes().to_vec()))
            .await;
        let (b, k) = (src_bucket.id, src_key.as_bytes().to_vec());
        let row = store
            .db
            .read(move |c| queries::get_object(c, &b, &k))
            .await?;
        let Some(row) = row else {
            return Err(
                if cx
                    .auth
                    .credential
                    .allows_list(&src_bucket.name, src_key.as_bytes())
                {
                    S3Error::no_such_key()
                } else {
                    S3Error::access_denied()
                },
            );
        };
        let data = store.data.clone();
        let sid = row.storage_id;
        let file = blocking(move || data.open_read(Area::Objects, &sid))
            .await
            .map_err(|e| {
                store.integrity_fault(&format!(
                    "copy source {} cannot be opened: {e}",
                    row.storage_id
                ));
                S3Error::internal()
            })?;
        (row, file)
    };
    match evaluate(&src_conds, &src_row.etag, src_row.last_modified_ms) {
        CondResult::Proceed => {}
        _ => return Err(S3Error::precondition_failed()),
    }
    let copy_limit = store
        .config
        .limits
        .max_single_put_bytes
        .min(crate::config::HARD_MAX_SINGLE_PUT_BYTES);
    if src_row.size > copy_limit {
        return Err(S3Error::invalid_request(format!(
            "The specified copy source is larger than the maximum allowable size for a copy source: {copy_limit}"
        )));
    }
    let (headers, meta) = if directive == "REPLACE" {
        (
            content_headers(&cx.req)?,
            user_metadata(&cx.req, store.config.limits.max_user_metadata_bytes)?,
        )
    } else {
        (src_row.headers.clone(), src_row.user_metadata.clone())
    };
    let same = src_bucket.id == dst_bucket.id && src_key == dst_key;
    if same && directive == "COPY" && requested_alg.is_none() {
        return Err(S3Error::invalid_request(
            "This copy request is illegal because it is trying to copy an object to itself without changing the object's metadata, storage class, website redirect location or encryption attributes.",
        ));
    }
    let _reservation = store.capacity.reserve(src_row.size)?;
    let ticket = store.new_blob(BlobArea::Object).await?;
    let alg = requested_alg
        .or_else(|| {
            src_row
                .checksum
                .as_ref()
                .filter(|c| c.kind == ChecksumType::FullObject)
                .map(|c| c.algorithm)
        })
        .unwrap_or(Algorithm::Crc64Nvme);
    let received = store
        .copy_into(
            ticket,
            vec![CopySource::File(src_file, src_row.size)],
            &[alg],
        )
        .await?;
    // The copy must reproduce the source bytes exactly.
    let src_sha = {
        let sid = src_row.storage_id;
        store
            .db
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT sha256 FROM blobs WHERE storage_id = ?1",
                    [sid.as_bytes().as_slice()],
                    |r| r.get::<_, Vec<u8>>(0),
                )?)
            })
            .await?
    };
    if src_sha.as_slice() != received.digests.sha256.as_slice() {
        store.integrity_fault(&format!(
            "copy source {} content does not match its digest",
            src_row.storage_id
        ));
        return Err(S3Error::internal().with_detail("copy source digest mismatch"));
    }
    let checksum = StoredChecksum::full(alg, received.digests.get(alg).unwrap_or_default());
    let etag = hex::encode(received.digests.md5);
    let staged = received.sync().await?;
    let spec = ObjectSpec {
        bucket_id: dst_bucket.id,
        key: dst_key,
        etag: etag.clone(),
        headers,
        user_metadata: meta,
        checksum: Some(checksum.clone()),
    };
    let committed = publish_and_commit(store, staged, spec, cond).await?;
    let mut w = XmlWriter::new();
    w.root("CopyObjectResult")
        .elem("ETag", &quote_etag(&etag))
        .elem("LastModified", &iso8601(committed.last_modified_ms))
        .elem("ChecksumType", checksum.kind.as_str())
        .elem(checksum.algorithm.xml_name(), &checksum.value)
        .close("CopyObjectResult");
    Ok(xml(StatusCode::OK, w.finish()))
}
