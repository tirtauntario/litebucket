//! Multipart uploads: Create, UploadPart, Complete, Abort, ListParts.
//!
//! State machine: OPEN -> COMPLETING -> COMPLETED, OPEN -> ABORTED, and
//! COMPLETING -> OPEN on definite failure or restart. The per-upload
//! finalization guard serializes part commits, completion, and abort for one
//! upload without blocking other uploads; no SQLite transaction spans
//! assembly.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use http::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::{S3Error, S3Result};
use super::headers::{content_headers, iso8601, quote_etag, response, user_metadata, write_conditions, xml};
use super::integrity::ChecksumRequest;
use super::object::{checksum_headers, commit_error};
use super::payload::Payload;
use super::xml::XmlWriter;
use super::{Cx, payload_length, read_control_body};
use crate::capacity::PermitKind;
use crate::checksums::{self, Algorithm, ChecksumType, StoredChecksum};
use crate::config::MIN_PART_BYTES;
use crate::credentials::Action;
use crate::error::Error;
use crate::fsutil::Area;
use crate::ids::UploadId;
use crate::metadata::queries::{
    self, BeginCompletion, BlobArea, CreateUpload, NewObject, ObjectCommit, PartCommit, PartRow, UploadRow,
    UploadState, WriteConditions,
};
use crate::metadata::{now_ms, with_write_tx};
use crate::store::{CopySource, Reconciled, Store};

fn upload_id(cx: &Cx) -> S3Result<UploadId> {
    cx.req
        .q("uploadId")
        .and_then(UploadId::parse)
        .ok_or_else(S3Error::no_such_upload)
}

/// Load an upload that targets exactly this bucket and key.
async fn load_upload(cx: &Cx, id: &UploadId, bucket: &crate::ids::BucketId) -> S3Result<UploadRow> {
    let idc = id.to_string();
    let row = cx
        .store
        .db
        .read(move |c| queries::get_upload(c, &idc))
        .await?
        .ok_or_else(S3Error::no_such_upload)?;
    if row.bucket_id != *bucket || row.key != cx.req.object_key().as_bytes() {
        return Err(S3Error::no_such_upload());
    }
    Ok(row)
}

fn checksum_choice(cx: &Cx) -> S3Result<(Algorithm, ChecksumType, bool)> {
    let alg = match cx.req.header("x-amz-checksum-algorithm")? {
        Some(a) => Some(
            Algorithm::parse(a).ok_or_else(|| S3Error::invalid_request(format!("Checksum algorithm {a} is not supported")))?,
        ),
        None => None,
    };
    let kind = match cx.req.header("x-amz-checksum-type")? {
        Some(t) => Some(ChecksumType::parse(t).ok_or_else(|| S3Error::invalid_request("Invalid x-amz-checksum-type"))?),
        None => None,
    };
    match (alg, kind) {
        (None, None) => Ok((Algorithm::Crc64Nvme, ChecksumType::FullObject, false)),
        (None, Some(_)) => Err(S3Error::invalid_request(
            "x-amz-checksum-type requires x-amz-checksum-algorithm",
        )),
        (Some(a), k) => {
            let k = k.unwrap_or_else(|| a.default_multipart_type());
            if !a.supports(k) {
                return Err(S3Error::invalid_request(format!(
                    "The {} checksum type cannot be used with the {} checksum algorithm.",
                    k.as_str(),
                    a.as_str().to_lowercase()
                )));
            }
            Ok((a, k, true))
        }
    }
}

pub async fn create(cx: &Cx) -> S3Result<Response<Body>> {
    cx.require_object(Action::Write)?;
    let headers = content_headers(&cx.req)?;
    let meta = user_metadata(&cx.req, cx.store.config.limits.max_user_metadata_bytes)?;
    let (alg, kind, explicit) = checksum_choice(cx)?;
    let bucket = cx.bucket().await?;
    let id = UploadId::random();
    let now = now_ms();
    let row = UploadRow {
        upload_id: id.to_string(),
        bucket_id: bucket.id,
        key: cx.req.object_key().as_bytes().to_vec(),
        state: UploadState::Open,
        creator_key_id: cx.credential_id().to_string(),
        headers,
        user_metadata: meta,
        checksum_algorithm: alg,
        checksum_type: kind,
        checksum_explicit: explicit,
        created_at_ms: now,
        last_activity_ms: now,
        completion_fingerprint: None,
        result_json: None,
        receipt_expires_at_ms: None,
    };
    let max = cx.store.config.multipart.max_active_uploads;
    let res = cx
        .store
        .db
        .write(move |c| with_write_tx(c, |tx| queries::create_upload(tx, &row, max)))
        .await?;
    match res {
        CreateUpload::Created => {}
        CreateUpload::NoSuchBucket => return Err(S3Error::no_such_bucket()),
        CreateUpload::TooManyUploads => {
            return Err(S3Error::slow_down().with_detail("active multipart upload limit reached"));
        }
    }
    let mut w = XmlWriter::new();
    w.root("InitiateMultipartUploadResult")
        .elem("Bucket", &bucket.name)
        .elem("Key", cx.req.object_key().as_str())
        .elem("UploadId", id.as_str())
        .close("InitiateMultipartUploadResult");
    let mut h: Vec<(String, String)> = vec![("content-type".into(), "application/xml".into())];
    if explicit {
        h.push(("x-amz-checksum-algorithm".into(), alg.as_str().into()));
        h.push(("x-amz-checksum-type".into(), kind.as_str().into()));
    }
    Ok(response(StatusCode::OK, h, Body::from(w.finish())))
}

fn part_number(cx: &Cx) -> S3Result<u32> {
    let max = cx.store.config.multipart.max_parts;
    cx.req
        .q("partNumber")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| (1..=max).contains(n))
        .ok_or_else(|| S3Error::invalid_argument(format!("Part number must be an integer between 1 and {max}, inclusive")))
}

pub async fn upload_part(cx: &Cx, body: Body) -> S3Result<Response<Body>> {
    cx.require_object(Action::Write)?;
    let store = &cx.store;
    let id = upload_id(cx)?;
    let number = part_number(cx)?;
    let integrity = ChecksumRequest::parse(&cx.req, &cx.auth.payload)?;
    let len = payload_length(&cx.req, &cx.auth.payload)?.ok_or_else(S3Error::missing_content_length)?;
    if len > store.config.limits.max_part_bytes {
        return Err(S3Error::entity_too_large()
            .with_extra("ProposedSize", len.to_string())
            .with_extra("MaxSizeAllowed", store.config.limits.max_part_bytes.to_string()));
    }
    let bucket = cx.bucket().await?;
    let upload = load_upload(cx, &id, &bucket.id).await?;
    if upload.state != UploadState::Open {
        return Err(S3Error::no_such_upload());
    }
    if upload.checksum_explicit
        && let Some(a) = integrity.algorithm()
        && a != upload.checksum_algorithm
    {
        return Err(S3Error::invalid_request(format!(
            "Checksum Type mismatch occurred, expected checksum Type: {}, actual checksum Type: {}",
            upload.checksum_algorithm.as_str().to_lowercase(),
            a.as_str().to_lowercase()
        )));
    }
    let _activity = store.upload_activity(id.as_str());
    let _permit = store.capacity.acquire(PermitKind::Upload).await?;
    let mut reservation = store.capacity.reserve(len)?;
    let ticket = store.new_blob(BlobArea::Part).await?;
    let trailer = cx.req.header("x-amz-trailer")?.map(|t| t.trim().to_ascii_lowercase());
    let mut payload = Payload::new(
        body,
        cx.auth.payload.clone(),
        Some(cx.auth.signing.clone()),
        Some(len),
        trailer,
        cx.idle_timeout(),
    );
    let algs = integrity.algorithms(&[upload.checksum_algorithm]);
    let received = store
        .receive(ticket, &mut payload, &algs, len, Some(&mut reservation))
        .await?;
    let verified = integrity.verify(&received.digests, payload.trailers())?;
    let part_checksum = StoredChecksum::full(
        upload.checksum_algorithm,
        received.digests.get(upload.checksum_algorithm).unwrap_or_default(),
    );
    let reply_checksum = match verified {
        Some((a, d)) => Some(StoredChecksum::full(a, &d)),
        None if upload.checksum_explicit => Some(part_checksum.clone()),
        None => None,
    };
    let etag = hex::encode(received.digests.md5);
    let size = received.digests.len;
    let staged = received.sync().await?;
    let store2 = store.clone();
    let (bid, key) = (bucket.id, cx.req.object_key().as_bytes().to_vec());
    let etag2 = etag.clone();
    let uid = id.to_string();
    store
        .supervise(async move {
            let store = store2;
            let published = staged.publish().await?;
            let blob = published.final_facts(Some(part_checksum));
            let sid = blob.storage_id;
            let _g = store.upload_locks.lock(uid.clone()).await;
            let (now, after) = (now_ms(), store.garbage_after());
            let res = store
                .db
                .write(move |c| {
                    with_write_tx(c, |tx| queries::commit_part(tx, &uid, &bid, &key, number, &blob, &etag2, now, after))
                })
                .await;
            let replaced = match res {
                Ok(PartCommit::Committed { replaced_size }) => replaced_size,
                Ok(PartCommit::UploadNotOpen) => return Err(S3Error::no_such_upload()),
                Err(Error::CommitUncertain) => match store.reconcile(sid).await {
                    Reconciled::Committed => None,
                    Reconciled::NotCommitted => return Err(S3Error::internal()),
                    Reconciled::Unknown => {
                        published.into_ticket().leave_for_recovery();
                        store.halt("unreconciled part commit");
                        return Err(S3Error::internal());
                    }
                },
                Err(e) => return Err(e.into()),
            };
            published.into_ticket().committed();
            store.capacity.add_part_bytes(size);
            if let Some(r) = replaced {
                store.capacity.release_part_bytes(r);
            }
            Ok(())
        })
        .await?;
    drop(reservation);
    let mut h: Vec<(String, String)> = vec![("etag".into(), quote_etag(&etag))];
    if let Some(c) = &reply_checksum {
        h.push((c.algorithm.header_name().into(), c.value.clone()));
    }
    Ok(response(StatusCode::OK, h, Body::empty()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestPart {
    number: u32,
    etag: String,
    checksum: Option<(Algorithm, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Receipt {
    bucket: String,
    key: String,
    etag: String,
    checksum: Option<StoredChecksum>,
}

fn parse_manifest(body: &[u8]) -> S3Result<Vec<ManifestPart>> {
    let doc = super::xml::parse(body)?;
    if doc.name != "CompleteMultipartUpload" {
        return Err(S3Error::malformed_xml());
    }
    let mut parts = Vec::new();
    for p in &doc.children {
        if p.name != "Part" {
            return Err(S3Error::malformed_xml());
        }
        let mut number = None;
        let mut etag = None;
        let mut checksum = None;
        for c in &p.children {
            match c.name.as_str() {
                "PartNumber" => number = c.text.trim().parse::<u32>().ok(),
                "ETag" => etag = Some(super::headers::etag_list(&c.text).into_iter().next().unwrap_or_default()),
                name => {
                    let alg = Algorithm::ALL
                        .into_iter()
                        .find(|a| a.xml_name() == name)
                        .ok_or_else(S3Error::malformed_xml)?;
                    if checksum.is_some() {
                        return Err(S3Error::invalid_request("A part may carry only one checksum"));
                    }
                    checksum = Some((alg, c.text.trim().to_string()));
                }
            }
        }
        let number = number.ok_or_else(S3Error::malformed_xml)?;
        let etag = etag.filter(|e| !e.is_empty()).ok_or_else(S3Error::malformed_xml)?;
        parts.push(ManifestPart { number, etag, checksum });
    }
    if parts.is_empty() {
        return Err(S3Error::malformed_xml()
            .with_detail("The XML you provided was not well-formed or did not validate against our published schema"));
    }
    if !parts.windows(2).all(|w| w[0].number < w[1].number) {
        return Err(S3Error::invalid_part_order());
    }
    if parts.iter().any(|p| p.number == 0 || p.number > 10_000) {
        return Err(S3Error::invalid_part("Part numbers must be between 1 and 10000"));
    }
    Ok(parts)
}

fn fingerprint(
    upload_id: &str,
    parts: &[ManifestPart],
    cond: &WriteConditions,
    checksum_type: Option<&str>,
    mp_size: Option<u64>,
    full_checksum: Option<(Algorithm, String)>,
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"storlite-complete-v1\n");
    h.update(upload_id.as_bytes());
    for p in parts {
        h.update(format!("\n{}:{}:", p.number, p.etag).as_bytes());
        if let Some((a, v)) = &p.checksum {
            h.update(format!("{}={v}", a.as_str()).as_bytes());
        }
    }
    h.update(format!("\nif-match={:?}\nif-none-match={}", cond.if_match, cond.if_none_match_any).as_bytes());
    h.update(format!("\ntype={checksum_type:?}\nsize={mp_size:?}\nfull={full_checksum:?}").as_bytes());
    h.finalize().into()
}

fn completion_xml(cx: &Cx, r: &Receipt) -> String {
    let mut w = XmlWriter::new();
    w.root("CompleteMultipartUploadResult")
        .elem("Location", &format!("/{}/{}", r.bucket, crate::sigv4::uri_encode(r.key.as_bytes(), false)))
        .elem("Bucket", &r.bucket)
        .elem("Key", &r.key)
        .elem("ETag", &quote_etag(&r.etag));
    if let Some(c) = &r.checksum {
        w.elem(c.algorithm.xml_name(), &c.value).elem("ChecksumType", c.kind.as_str());
    }
    let _ = cx;
    w.close("CompleteMultipartUploadResult");
    w.finish()
}

pub async fn complete(cx: &Cx, body: Body) -> S3Result<Response<Body>> {
    cx.require_object(Action::Write)?;
    let store = &cx.store;
    let id = upload_id(cx)?;
    let bucket = cx.bucket().await?;
    let body = read_control_body(cx, body, store.config.limits.max_xml_body_bytes, false).await?;
    let manifest = parse_manifest(&body)?;
    let cond = write_conditions(&cx.req)?;
    let req_type = cx.req.header("x-amz-checksum-type")?.map(|s| s.to_string());
    let mp_size = match cx.req.header("x-amz-mp-object-size")? {
        Some(v) => Some(v.parse::<u64>().map_err(|_| S3Error::invalid_argument("invalid x-amz-mp-object-size"))?),
        None => None,
    };
    let mut full_checksum = None;
    for a in Algorithm::ALL {
        if let Some(v) = cx.req.header(a.header_name())? {
            full_checksum = Some((a, v.trim().to_string()));
        }
    }
    let fp = fingerprint(id.as_str(), &manifest, &cond, req_type.as_deref(), mp_size, full_checksum.clone());

    let _assembly = store.capacity.acquire(PermitKind::Assembly).await?;
    let _activity = store.upload_activity(id.as_str());
    let upload_guard = store.upload_locks.lock(id.to_string()).await;
    let upload = load_upload(cx, &id, &bucket.id).await?;
    match upload.state {
        UploadState::Open => {}
        UploadState::Completed => {
            // Idempotent retry of the identical request: replay the receipt
            // without writing (and never resurrecting) the object.
            let fresh = upload.receipt_expires_at_ms.is_some_and(|e| e > now_ms());
            if fresh
                && upload.completion_fingerprint.as_deref() == Some(fp.as_slice())
                && let Some(r) = upload.result_json.as_deref().and_then(|j| serde_json::from_str::<Receipt>(j).ok())
            {
                return Ok(xml(StatusCode::OK, completion_xml(cx, &r)));
            }
            return Err(S3Error::no_such_upload());
        }
        UploadState::Aborted | UploadState::Completing => return Err(S3Error::no_such_upload()),
    }
    if let Some(t) = &req_type
        && ChecksumType::parse(t) != Some(upload.checksum_type)
    {
        return Err(S3Error::invalid_request(format!(
            "The upload was created using the {} checksum mode. The complete request must use the same checksum mode.",
            upload.checksum_type.as_str()
        )));
    }
    let idc = id.to_string();
    let committed: Vec<PartRow> = store
        .db
        .read(move |c| queries::list_parts(c, &idc, 0, 10_001))
        .await?;
    let by_number: HashMap<u32, &PartRow> = committed.iter().map(|p| (p.part_number, p)).collect();

    // Validate the manifest against committed parts.
    let composite = upload.checksum_explicit && upload.checksum_type == ChecksumType::Composite;
    if composite && !manifest.iter().enumerate().all(|(i, p)| p.number as usize == i + 1) {
        return Err(S3Error::invalid_part(
            "Composite checksum uploads require consecutive part numbers starting at 1.",
        ));
    }
    let mut selected = Vec::with_capacity(manifest.len());
    for mp in &manifest {
        let part = by_number.get(&mp.number).filter(|p| p.etag == mp.etag).ok_or_else(|| {
            S3Error::invalid_part(
                "One or more of the specified parts could not be found. The part may not have been uploaded, or the specified entity tag may not match the part's entity tag.",
            )
        })?;
        match (&mp.checksum, &part.checksum) {
            (Some((a, v)), Some(stored)) => {
                if *a != stored.algorithm || *v != stored.value {
                    return Err(S3Error::invalid_part(format!(
                        "The {} checksum for part {} does not match the uploaded part.",
                        a.as_str(),
                        mp.number
                    )));
                }
            }
            (Some(_), None) => return Err(S3Error::invalid_part("part checksum unavailable")),
            (None, _) if composite => {
                return Err(S3Error::invalid_request(format!(
                    "The upload was created using a {} checksum. The complete request must include the checksum for each part.",
                    upload.checksum_algorithm.as_str().to_lowercase()
                )));
            }
            (None, _) => {}
        }
        selected.push((*part).clone());
    }
    let mut total: u64 = 0;
    for (i, part) in selected.iter().enumerate() {
        let last = i + 1 == selected.len();
        if !last && part.size < MIN_PART_BYTES {
            return Err(S3Error::entity_too_small()
                .with_extra("ProposedSize", part.size.to_string())
                .with_extra("MinSizeAllowed", MIN_PART_BYTES.to_string())
                .with_extra("PartNumber", part.part_number.to_string()));
        }
        if part.size > store.config.limits.max_part_bytes {
            return Err(S3Error::entity_too_large());
        }
        total = total.checked_add(part.size).ok_or_else(S3Error::entity_too_large)?;
    }
    if total > store.config.limits.max_object_bytes {
        return Err(S3Error::entity_too_large()
            .with_extra("ProposedSize", total.to_string())
            .with_extra("MaxSizeAllowed", store.config.limits.max_object_bytes.to_string()));
    }
    if mp_size.is_some_and(|s| s != total) {
        return Err(S3Error::invalid_request(
            "The provided x-amz-mp-object-size does not match the size of the completed object.",
        ));
    }
    if let Some((a, _)) = &full_checksum
        && (*a != upload.checksum_algorithm || !upload.checksum_explicit)
    {
        return Err(S3Error::invalid_request(
            "The full-object checksum algorithm does not match the upload's checksum algorithm.",
        ));
    }

    let reservation = store.capacity.reserve(total)?;
    let manifest_json = serde_json::to_string(
        &selected
            .iter()
            .map(|p| (p.part_number, p.storage_id.to_hex(), p.size))
            .collect::<Vec<_>>(),
    )
    .map_err(|e| S3Error::internal().with_detail(e.to_string()))?;
    let receipt_ttl = store.config.multipart.receipt_retention_seconds as i64 * 1000;
    let job = CompletionJob {
        store: store.clone(),
        upload,
        selected,
        cond,
        full_checksum,
        bucket_name: bucket.name.clone(),
        bucket_id: bucket.id,
        key: cx.req.object_key().clone(),
        receipt_ttl,
    };
    // Everything from COMPLETING onward is supervised so a disconnect cannot
    // strand the upload; the finalization guard and reservation move with it.
    let receipt = store
        .supervise(async move {
            let _guard = upload_guard;
            let _reservation = reservation;
            job.run(fp, manifest_json).await
        })
        .await?;
    Ok(xml(StatusCode::OK, completion_xml(cx, &receipt)))
}

struct CompletionJob {
    store: Arc<Store>,
    upload: UploadRow,
    selected: Vec<PartRow>,
    cond: WriteConditions,
    full_checksum: Option<(Algorithm, String)>,
    bucket_name: String,
    bucket_id: crate::ids::BucketId,
    key: crate::keys::ObjectKey,
    receipt_ttl: i64,
}

impl CompletionJob {
    /// OPEN -> COMPLETING, assemble, commit; revert to OPEN on any definite
    /// failure.
    async fn run(self, fp: [u8; 32], manifest_json: String) -> S3Result<Receipt> {
        let store = self.store.clone();
        let output = crate::ids::StorageId::allocate();
        let (idc, now) = (self.upload.upload_id.clone(), now_ms());
        let begin = store
            .db
            .write(move |c| {
                with_write_tx(c, |tx| queries::begin_completion(tx, &idc, &fp, &manifest_json, &output, now))
            })
            .await?;
        if begin != BeginCompletion::Started {
            return Err(S3Error::no_such_upload());
        }
        let ticket = store.adopt_blob(output, BlobArea::Object);
        let uid = self.upload.upload_id.clone();
        let result = self.assemble_and_commit(ticket).await;
        if let Err(e) = &result
            && e.detail.as_deref() != Some("completion outcome unknown")
        {
            let after = store.garbage_after();
            let _ = store
                .db
                .write(move |c| with_write_tx(c, |tx| queries::revert_completion(tx, &uid, &output, after)))
                .await;
        }
        result
    }

    async fn assemble_and_commit(self, ticket: crate::store::WriteTicket) -> S3Result<Receipt> {
        let store = &self.store;
        let alg = self.upload.checksum_algorithm;
        let full_type = self.upload.checksum_type == ChecksumType::FullObject;
        let sources = self
            .selected
            .iter()
            .map(|p| CopySource::Tracked {
                area: Area::Multipart,
                id: p.storage_id,
                size: p.size,
                md5: Some(p.md5),
            })
            .collect();
        let algs: Vec<Algorithm> = if full_type { vec![alg] } else { vec![] };
        crate::failpoint::hit("during_assembly");
        let received = store.copy_into(ticket, sources, &algs).await?;
        let checksum = if full_type {
            StoredChecksum::full(alg, received.digests.get(alg).unwrap_or_default())
        } else {
            let digests: Vec<Vec<u8>> = self
                .selected
                .iter()
                .map(|p| p.checksum.as_ref().and_then(|c| c.digest()).unwrap_or_default())
                .collect();
            StoredChecksum {
                algorithm: alg,
                kind: ChecksumType::Composite,
                value: checksums::composite(alg, &digests),
            }
        };
        if let Some((_, want)) = &self.full_checksum
            && *want != checksum.value
            && want.split('-').next() != checksum.value.split('-').next()
        {
            return Err(S3Error::bad_checksum(alg.as_str()));
        }
        let md5s: Vec<[u8; 16]> = self.selected.iter().map(|p| p.md5).collect();
        let etag = checksums::multipart_etag(&md5s);
        let staged = received.sync().await?;
        let published = staged.publish().await?;
        let blob = published.final_facts(Some(checksum.clone()));
        let sid = blob.storage_id;
        let receipt = Receipt {
            bucket: self.bucket_name.clone(),
            key: self.key.as_str().to_string(),
            etag: etag.clone(),
            checksum: self.upload.checksum_explicit.then_some(checksum),
        };
        let result_json = serde_json::to_string(&receipt).map_err(|e| S3Error::internal().with_detail(e.to_string()))?;
        let now = now_ms();
        let new = NewObject {
            bucket_id: self.bucket_id,
            key: self.key.as_bytes().to_vec(),
            blob,
            etag,
            headers: self.upload.headers.clone(),
            user_metadata: self.upload.user_metadata.clone(),
            now_ms: now,
        };
        let (uid, cond, after, expires) = (
            self.upload.upload_id.clone(),
            self.cond.clone(),
            store.garbage_after(),
            now.saturating_add(self.receipt_ttl),
        );
        let _key_guard = store
            .key_locks
            .lock((self.bucket_id, self.key.as_bytes().to_vec()))
            .await;
        crate::failpoint::hit("before_completion_commit");
        let res = store
            .db
            .write(move |c| {
                with_write_tx(c, |tx| queries::finish_completion(tx, &uid, &new, &cond, &result_json, expires, after))
            })
            .await;
        match res {
            Ok(done) => match done.outcome {
                ObjectCommit::Committed { .. } => {
                    published.into_ticket().committed();
                    store.capacity.release_part_bytes(done.released_part_bytes);
                    Ok(receipt)
                }
                other => Err(commit_error(&other)),
            },
            Err(Error::CommitUncertain) => match store.reconcile(sid).await {
                Reconciled::Committed => {
                    published.into_ticket().committed();
                    let total: u64 = self.selected.iter().map(|p| p.size).sum();
                    store.capacity.release_part_bytes(total);
                    Ok(receipt)
                }
                Reconciled::NotCommitted => Err(S3Error::internal()),
                Reconciled::Unknown => {
                    published.into_ticket().leave_for_recovery();
                    store.halt("unreconciled multipart completion");
                    Err(S3Error::internal().with_detail("completion outcome unknown"))
                }
            },
            Err(e) => Err(e.into()),
        }
    }
}

pub async fn abort(cx: &Cx) -> S3Result<Response<Body>> {
    cx.require_object(Action::Delete)?;
    let store = &cx.store;
    let id = upload_id(cx)?;
    let bucket = cx.bucket().await?;
    let _g = store.upload_locks.lock(id.to_string()).await;
    let upload = load_upload(cx, &id, &bucket.id).await?;
    if upload.state != UploadState::Open {
        return Err(S3Error::no_such_upload());
    }
    let (idc, now, after) = (id.to_string(), now_ms(), store.garbage_after());
    let ttl = store.config.multipart.receipt_retention_seconds as i64 * 1000;
    let released = store
        .db
        .write(move |c| with_write_tx(c, |tx| queries::abort_upload(tx, &idc, now, now + ttl, after)))
        .await?
        .ok_or_else(S3Error::no_such_upload)?;
    store.capacity.release_part_bytes(released);
    store.metrics.multipart_aborted.fetch_add(1, Ordering::Relaxed);
    Ok(response(StatusCode::NO_CONTENT, Vec::<(&str, String)>::new(), Body::empty()))
}

pub async fn list_parts(cx: &Cx) -> S3Result<Response<Body>> {
    let key = cx.req.object_key().clone();
    if !cx.auth.credential.allows_list(cx.req.bucket_name(), key.as_bytes()) {
        return Err(S3Error::access_denied());
    }
    let id = upload_id(cx)?;
    let bucket = cx.bucket().await?;
    let upload = load_upload(cx, &id, &bucket.id).await?;
    if !matches!(upload.state, UploadState::Open | UploadState::Completing) {
        return Err(S3Error::no_such_upload());
    }
    let marker: u32 = match cx.req.q("part-number-marker") {
        Some(v) => v.parse().map_err(|_| S3Error::invalid_argument("invalid part-number-marker"))?,
        None => 0,
    };
    let max: usize = match cx.req.q("max-parts") {
        Some(v) => v
            .parse::<usize>()
            .map_err(|_| S3Error::invalid_argument("invalid max-parts"))?
            .min(1000),
        None => 1000,
    };
    let idc = id.to_string();
    let mut parts = cx
        .store
        .db
        .read(move |c| queries::list_parts(c, &idc, marker, max + 1))
        .await?;
    let truncated = parts.len() > max;
    parts.truncate(max);
    let mut w = XmlWriter::new();
    w.root("ListPartsResult")
        .elem("Bucket", &bucket.name)
        .elem("Key", key.as_str())
        .elem("UploadId", id.as_str())
        .elem("PartNumberMarker", &marker.to_string());
    if let Some(last) = parts.last() {
        w.elem("NextPartNumberMarker", &last.part_number.to_string());
    }
    w.elem("MaxParts", &max.to_string())
        .elem("IsTruncated", if truncated { "true" } else { "false" });
    for p in &parts {
        w.open("Part")
            .elem("PartNumber", &p.part_number.to_string())
            .elem("LastModified", &iso8601(p.last_modified_ms))
            .elem("ETag", &quote_etag(&p.etag))
            .elem("Size", &p.size.to_string());
        if upload.checksum_explicit
            && let Some(c) = &p.checksum
        {
            w.elem(c.algorithm.xml_name(), &c.value);
        }
        w.close("Part");
    }
    w.open("Initiator")
        .elem("ID", &cx.store.meta.owner_id)
        .elem("DisplayName", "storlite")
        .close("Initiator")
        .open("Owner")
        .elem("ID", &cx.store.meta.owner_id)
        .elem("DisplayName", "storlite")
        .close("Owner")
        .elem("StorageClass", "STANDARD");
    if upload.checksum_explicit {
        w.elem("ChecksumAlgorithm", upload.checksum_algorithm.as_str())
            .elem("ChecksumType", upload.checksum_type.as_str());
    }
    w.close("ListPartsResult");
    let _ = checksum_headers;
    Ok(xml(StatusCode::OK, w.finish()))
}
