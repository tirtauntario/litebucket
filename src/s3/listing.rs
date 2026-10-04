//! ListObjectsV2, ListMultipartUploads, bucket-list tokens, and the indexed
//! delimiter-aware page algorithm.
//!
//! Pages are produced from bounded index range scans inside one short read
//! transaction. Rolled-up prefixes are skipped by jumping to the prefix's
//! bytewise successor, so a folder with millions of keys costs one row.

use axum::body::Body;
use base64::Engine;
use http::{Response, StatusCode};

use super::Cx;
use super::error::{S3Error, S3Result};
use super::headers::{iso8601, quote_etag, xml};
use super::xml::XmlWriter;
use crate::ids::BucketId;
use crate::keys::prefix_successor;
use crate::metadata::queries::{self, ListedObject, UploadRow};
use crate::metadata::with_read_tx;
use crate::sigv4;
use crate::store::Store;

const TOKEN_VERSION: u8 = 1;
const MAX_TOKEN_CHARS: usize = 4096;

/// Everything a continuation token is bound to besides its position.
pub struct TokenScope {
    kind: u8,
    bucket: [u8; 16],
    prefix: Vec<u8>,
    delimiter: Vec<u8>,
    flags: u8,
    principal: String,
}

impl TokenScope {
    pub fn objects(
        bucket: &BucketId,
        prefix: &[u8],
        delimiter: &[u8],
        url: bool,
        owner: bool,
        principal: &str,
    ) -> Self {
        Self {
            kind: 1,
            bucket: *bucket.as_bytes(),
            prefix: prefix.to_vec(),
            delimiter: delimiter.to_vec(),
            flags: u8::from(url) | (u8::from(owner) << 1),
            principal: principal.to_string(),
        }
    }

    pub fn buckets(prefix: &str, principal: &str) -> Self {
        Self {
            kind: 2,
            bucket: [0; 16],
            prefix: prefix.as_bytes().to_vec(),
            delimiter: Vec::new(),
            flags: 0,
            principal: principal.to_string(),
        }
    }

    fn mac(&self, store: &Store, inclusive: bool, position: &[u8]) -> [u8; 32] {
        let mut m = Vec::with_capacity(128 + position.len());
        let mut put = |b: &[u8]| {
            m.extend_from_slice(&(b.len() as u32).to_be_bytes());
            m.extend_from_slice(b);
        };
        put(&[TOKEN_VERSION, self.kind, self.flags, u8::from(inclusive)]);
        put(store.meta.store_id.as_bytes());
        put(&self.bucket);
        put(&self.prefix);
        put(&self.delimiter);
        put(self.principal.as_bytes());
        put(position);
        sigv4::hmac(&store.meta.cursor_key, &m)
    }
}

fn encode_token(store: &Store, scope: &TokenScope, inclusive: bool, position: &[u8]) -> String {
    let mut out = vec![TOKEN_VERSION, u8::from(inclusive)];
    out.extend_from_slice(position);
    out.extend_from_slice(&scope.mac(store, inclusive, position));
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out)
}

fn decode_token(store: &Store, token: &str, scope: &TokenScope) -> S3Result<(Vec<u8>, bool)> {
    let invalid = || S3Error::invalid_argument("The continuation token provided is incorrect");
    if token.len() > MAX_TOKEN_CHARS {
        return Err(invalid());
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| invalid())?;
    if raw.len() < 2 + 32 || raw[0] != TOKEN_VERSION || raw[1] > 1 {
        return Err(invalid());
    }
    let inclusive = raw[1] == 1;
    let (body, tag) = raw.split_at(raw.len() - 32);
    let position = &body[2..];
    let expected = scope.mac(store, inclusive, position);
    if !sigv4::signature_eq(&expected, &hex::encode(tag)) {
        return Err(invalid());
    }
    Ok((position.to_vec(), inclusive))
}

pub fn encode_bucket_token(store: &Store, after: &str, scope: &TokenScope) -> String {
    encode_token(store, scope, false, after.as_bytes())
}

pub fn decode_bucket_token(store: &Store, token: &str, scope: &TokenScope) -> S3Result<String> {
    let (pos, _) = decode_token(store, token, scope)?;
    String::from_utf8(pos)
        .map_err(|_| S3Error::invalid_argument("The continuation token provided is incorrect"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry<T> {
    Item(T),
    Prefix(Vec<u8>),
}

#[derive(Debug)]
pub struct Page<T> {
    pub entries: Vec<Entry<T>>,
    /// Scan position to resume from when truncated.
    pub next: Option<(Vec<u8>, bool)>,
}

/// Generic delimiter-aware page builder over a bytewise-ordered index.
/// `fetch(from, inclusive, upper, limit)` returns ordered items; `key` maps
/// an item to its object key (for prefix/delimiter grouping) and `pos` to its
/// scan position (which may extend the key, e.g. with an upload ID).
pub fn build_page<T, F, K, P>(
    prefix: &[u8],
    delimiter: Option<&[u8]>,
    max: usize,
    start: (Vec<u8>, bool),
    mut fetch: F,
    key: K,
    pos_of: P,
) -> crate::error::Result<Page<T>>
where
    F: FnMut(&[u8], bool, Option<&[u8]>, usize) -> crate::error::Result<Vec<T>>,
    K: Fn(&T) -> &[u8],
    P: Fn(&T) -> Vec<u8>,
{
    let upper = if prefix.is_empty() {
        None
    } else {
        prefix_successor(prefix)
    };
    let mut pos = if start.0.as_slice() < prefix {
        (prefix.to_vec(), true)
    } else {
        start
    };
    let mut entries: Vec<Entry<T>> = Vec::new();
    if max == 0 {
        return Ok(Page {
            entries,
            next: None,
        });
    }
    'outer: loop {
        let want = (max - entries.len()).min(1000) + 1;
        let rows = fetch(&pos.0, pos.1, upper.as_deref(), want)?;
        if rows.is_empty() {
            return Ok(Page {
                entries,
                next: None,
            });
        }
        for row in rows {
            if entries.len() == max {
                return Ok(Page {
                    entries,
                    next: Some(pos),
                });
            }
            let k = key(&row);
            if let Some(d) = delimiter
                && let Some(i) = find(&k[prefix.len()..], d)
            {
                let cp = k[..prefix.len() + i + d.len()].to_vec();
                let succ = prefix_successor(&cp);
                entries.push(Entry::Prefix(cp));
                match succ {
                    Some(s) => {
                        pos = (s, true);
                        continue 'outer;
                    }
                    None => {
                        return Ok(Page {
                            entries,
                            next: None,
                        });
                    }
                }
            }
            pos = (pos_of(&row), false);
            entries.push(Entry::Item(row));
        }
        if entries.len() == max {
            // Probe for one more row to decide truncation.
            let more = fetch(&pos.0, pos.1, upper.as_deref(), 1)?;
            return Ok(Page {
                next: (!more.is_empty()).then_some(pos),
                entries,
            });
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `encoding-type=url` representation (form-style: space as `+`).
pub fn url_encode(b: &[u8]) -> String {
    sigv4::uri_encode(b, false).replace("%20", "+")
}

fn text(b: &[u8], url: bool) -> String {
    if url {
        url_encode(b)
    } else {
        String::from_utf8_lossy(b).into_owned()
    }
}

fn parse_max(v: Option<&str>, name: &str, cap: usize) -> S3Result<usize> {
    match v {
        None => Ok(cap),
        Some(s) => {
            let n: i64 = s.parse().map_err(|_| {
                S3Error::invalid_argument(format!(
                    "Provided {name} not an integer or within integer range"
                ))
            })?;
            if n < 0 {
                return Err(S3Error::invalid_argument(format!(
                    "{name} must be non-negative"
                )));
            }
            Ok((n as usize).min(cap))
        }
    }
}

fn encoding(cx: &Cx) -> S3Result<bool> {
    match cx.req.q("encoding-type") {
        None => Ok(false),
        Some("url") => Ok(true),
        Some(_) => Err(S3Error::invalid_argument(
            "Invalid Encoding Method specified in Request",
        )),
    }
}

pub async fn list_objects_v2(cx: &Cx) -> S3Result<Response<Body>> {
    let prefix = cx.req.q("prefix").unwrap_or("").as_bytes().to_vec();
    if !cx
        .auth
        .credential
        .allows_list(cx.req.bucket_name(), &prefix)
    {
        return Err(S3Error::access_denied());
    }
    let bucket = cx.bucket().await?;
    let delimiter = cx
        .req
        .q("delimiter")
        .filter(|d| !d.is_empty())
        .map(|d| d.as_bytes().to_vec());
    let cap = cx.store.config.limits.max_listing_entries;
    let max = parse_max(cx.req.q("max-keys"), "max-keys", cap)?;
    let url = encoding(cx)?;
    let fetch_owner = cx.req.q("fetch-owner") == Some("true");
    let start_after = cx.req.q("start-after").unwrap_or("").as_bytes().to_vec();
    let scope = TokenScope::objects(
        &bucket.id,
        &prefix,
        delimiter.as_deref().unwrap_or(b""),
        url,
        fetch_owner,
        cx.credential_id(),
    );
    let token = cx.req.q("continuation-token");
    let start = match token {
        Some(t) => decode_token(&cx.store, t, &scope)?,
        None => (start_after.clone(), false),
    };
    let (p, d, bid) = (prefix.clone(), delimiter.clone(), bucket.id);
    let page = cx
        .store
        .db
        .read(move |c| {
            with_read_tx(c, |tx| {
                build_page(
                    &p,
                    d.as_deref(),
                    max,
                    start,
                    |from, incl, upper, limit| {
                        queries::objects_from(tx, &bid, from, incl, upper, limit)
                    },
                    |o: &ListedObject| o.key.as_slice(),
                    |o: &ListedObject| o.key.clone(),
                )
            })
        })
        .await?;
    let next = page
        .next
        .as_ref()
        .map(|(pos, incl)| encode_token(&cx.store, &scope, *incl, pos));

    let mut w = XmlWriter::new();
    w.root("ListBucketResult")
        .elem("Name", &bucket.name)
        .elem("Prefix", &text(&prefix, url))
        .elem("KeyCount", &page.entries.len().to_string())
        .elem("MaxKeys", &max.to_string());
    if let Some(d) = &delimiter {
        w.elem("Delimiter", &text(d, url));
    }
    if url {
        w.elem("EncodingType", "url");
    }
    w.elem("IsTruncated", if next.is_some() { "true" } else { "false" });
    if let Some(t) = token {
        w.elem("ContinuationToken", t);
    }
    if let Some(n) = &next {
        w.elem("NextContinuationToken", n);
    }
    if cx.req.has_q("start-after") {
        w.elem("StartAfter", &text(&start_after, url));
    }
    for e in &page.entries {
        match e {
            Entry::Item(o) => {
                w.open("Contents")
                    .elem("Key", &text(&o.key, url))
                    .elem("LastModified", &iso8601(o.last_modified_ms))
                    .elem("ETag", &quote_etag(&o.etag))
                    .elem("Size", &o.size.to_string());
                if let Some(c) = &o.checksum {
                    w.elem("ChecksumAlgorithm", c.algorithm.as_str())
                        .elem("ChecksumType", c.kind.as_str());
                }
                w.elem("StorageClass", "STANDARD");
                if fetch_owner {
                    w.open("Owner")
                        .elem("ID", &cx.store.meta.owner_id)
                        .elem("DisplayName", "storlite")
                        .close("Owner");
                }
                w.close("Contents");
            }
            Entry::Prefix(p) => {
                w.open("CommonPrefixes")
                    .elem("Prefix", &text(p, url))
                    .close("CommonPrefixes");
            }
        }
    }
    w.close("ListBucketResult");
    Ok(xml(StatusCode::OK, w.finish()))
}

pub async fn list_multipart_uploads(cx: &Cx) -> S3Result<Response<Body>> {
    let prefix = cx.req.q("prefix").unwrap_or("").as_bytes().to_vec();
    if !cx
        .auth
        .credential
        .allows_list(cx.req.bucket_name(), &prefix)
    {
        return Err(S3Error::access_denied());
    }
    let bucket = cx.bucket().await?;
    let delimiter = cx
        .req
        .q("delimiter")
        .filter(|d| !d.is_empty())
        .map(|d| d.as_bytes().to_vec());
    let max = parse_max(cx.req.q("max-uploads"), "max-uploads", 1000)?;
    let url = encoding(cx)?;
    let key_marker = cx.req.q("key-marker").unwrap_or("").as_bytes().to_vec();
    let upload_marker = cx
        .req
        .q("upload-id-marker")
        .filter(|_| !key_marker.is_empty())
        .unwrap_or("");
    // Scan positions are `key \0 upload_id` (keys never contain NUL, so this
    // preserves (key, upload_id) order). "~" sorts after every hex upload ID.
    let start = if key_marker.is_empty() {
        (Vec::new(), true)
    } else {
        let mut p = key_marker.clone();
        p.push(0);
        p.extend_from_slice(if upload_marker.is_empty() {
            b"~"
        } else {
            upload_marker.as_bytes()
        });
        (p, false)
    };
    let (p, d, bid) = (prefix.clone(), delimiter.clone(), bucket.id);
    let (page, group_ends) = cx
        .store
        .db
        .read(move |c| {
            with_read_tx(c, |tx| {
                let page = build_page(
                    &p,
                    d.as_deref(),
                    max,
                    start,
                    |from, incl, upper, limit| {
                        let (k, u) = match from.iter().position(|b| *b == 0) {
                            Some(i) => (&from[..i], std::str::from_utf8(&from[i + 1..]).unwrap_or("~")),
                            None => (from, ""),
                        };
                        if incl || !from.contains(&0) {
                            queries::uploads_from(tx, &bid, k, "", incl, upper, limit)
                                .map(|v| if incl { v } else { v.into_iter().filter(|x| x.key.as_slice() > k).collect() })
                        } else {
                            queries::uploads_from(tx, &bid, k, u, false, upper, limit)
                        }
                    },
                    |u: &UploadRow| u.key.as_slice(),
                    |u: &UploadRow| {
                        let mut p = u.key.clone();
                        p.push(0);
                        p.extend_from_slice(u.upload_id.as_bytes());
                        p
                    },
                )?;
                // For a trailing common prefix, the next key marker is the
                // greatest key in that group so the group is not repeated.
                let mut ends = Vec::new();
                if page.next.is_some()
                    && let Some(Entry::Prefix(cp)) = page.entries.last()
                {
                    let upper = prefix_successor(cp);
                    let last: Option<Vec<u8>> = tx.query_row(
                        "SELECT max(object_key) FROM multipart_uploads WHERE bucket_id = ?1 AND state IN ('open','completing')
                         AND object_key >= ?2 AND (?3 IS NULL OR object_key < ?3)",
                        rusqlite::params![bid.as_bytes().as_slice(), cp, upper],
                        |r| r.get(0),
                    )?;
                    ends.extend(last);
                }
                Ok((page, ends))
            })
        })
        .await?;
    let truncated = page.next.is_some();
    let (next_key, next_upload) = match page.entries.last() {
        Some(Entry::Item(u)) if truncated => (Some(u.key.clone()), Some(u.upload_id.clone())),
        Some(Entry::Prefix(_)) if truncated => (group_ends.first().cloned(), None),
        _ => (None, None),
    };
    let mut w = XmlWriter::new();
    w.root("ListMultipartUploadsResult")
        .elem("Bucket", &bucket.name)
        .elem("KeyMarker", &text(&key_marker, url))
        .elem("UploadIdMarker", upload_marker);
    if let Some(k) = &next_key {
        w.elem("NextKeyMarker", &text(k, url));
    }
    if let Some(u) = &next_upload {
        w.elem("NextUploadIdMarker", u);
    }
    w.elem("Prefix", &text(&prefix, url));
    if let Some(d) = &delimiter {
        w.elem("Delimiter", &text(d, url));
    }
    if url {
        w.elem("EncodingType", "url");
    }
    w.elem("MaxUploads", &max.to_string())
        .elem("IsTruncated", if truncated { "true" } else { "false" });
    for e in &page.entries {
        match e {
            Entry::Item(u) => {
                w.open("Upload")
                    .elem("Key", &text(&u.key, url))
                    .elem("UploadId", &u.upload_id)
                    .open("Initiator")
                    .elem("ID", &cx.store.meta.owner_id)
                    .elem("DisplayName", "storlite")
                    .close("Initiator")
                    .open("Owner")
                    .elem("ID", &cx.store.meta.owner_id)
                    .elem("DisplayName", "storlite")
                    .close("Owner")
                    .elem("StorageClass", "STANDARD")
                    .elem("Initiated", &iso8601(u.created_at_ms));
                if u.checksum_explicit {
                    w.elem("ChecksumAlgorithm", u.checksum_algorithm.as_str())
                        .elem("ChecksumType", u.checksum_type.as_str());
                }
                w.close("Upload");
            }
            Entry::Prefix(p) => {
                w.open("CommonPrefixes")
                    .elem("Prefix", &text(p, url))
                    .close("CommonPrefixes");
            }
        }
    }
    w.close("ListMultipartUploadsResult");
    Ok(xml(StatusCode::OK, w.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Reference model: filter, roll up, sort, and dedupe in memory.
    fn model(
        keys: &[Vec<u8>],
        prefix: &[u8],
        delim: Option<&[u8]>,
        start_after: &[u8],
    ) -> Vec<Entry<Vec<u8>>> {
        let mut sorted = keys.to_vec();
        sorted.sort();
        sorted.dedup();
        let mut out: Vec<Entry<Vec<u8>>> = Vec::new();
        for k in sorted {
            if !k.starts_with(prefix) || k.as_slice() <= start_after {
                continue;
            }
            let e = match delim.and_then(|d| find(&k[prefix.len()..], d).map(|i| (i, d))) {
                Some((i, d)) => Entry::Prefix(k[..prefix.len() + i + d.len()].to_vec()),
                None => Entry::Item(k.clone()),
            };
            if out.last() != Some(&e) {
                out.push(e);
            }
        }
        out
    }

    fn paged(
        keys: &[Vec<u8>],
        prefix: &[u8],
        delim: Option<&[u8]>,
        start_after: &[u8],
        max: usize,
    ) -> Vec<Entry<Vec<u8>>> {
        let mut sorted = keys.to_vec();
        sorted.sort();
        sorted.dedup();
        let fetch = |from: &[u8], incl: bool, upper: Option<&[u8]>, limit: usize| {
            Ok(sorted
                .iter()
                .filter(|k| {
                    if incl {
                        k.as_slice() >= from
                    } else {
                        k.as_slice() > from
                    }
                })
                .filter(|k| upper.is_none_or(|u| k.as_slice() < u))
                .take(limit)
                .cloned()
                .collect::<Vec<_>>())
        };
        let mut all = Vec::new();
        let mut start = (start_after.to_vec(), false);
        for _ in 0..10_000 {
            let page = build_page(
                prefix,
                delim,
                max,
                start.clone(),
                fetch,
                |k: &Vec<u8>| k.as_slice(),
                |k: &Vec<u8>| k.clone(),
            )
            .unwrap();
            assert!(page.entries.len() <= max);
            all.extend(page.entries);
            match page.next {
                Some(n) => start = n,
                None => return all,
            }
        }
        panic!("pagination did not terminate");
    }

    fn key_strategy() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(
            prop_oneof![
                Just(b'a'),
                Just(b'b'),
                Just(b'/'),
                Just(0xc3u8),
                Just(0xffu8)
            ],
            1..6,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]
        #[test]
        fn paging_matches_reference_model(
            keys in proptest::collection::vec(key_strategy(), 0..40),
            prefix in proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'/'), Just(0xffu8)], 0..3),
            delim in prop_oneof![Just(None), Just(Some(b"/".to_vec())), Just(Some(b"b/".to_vec())), Just(Some(vec![0xffu8]))],
            start_after in proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'b'), Just(b'/')], 0..3),
            max in 1usize..5,
        ) {
            let want = model(&keys, &prefix, delim.as_deref(), &start_after);
            let got = paged(&keys, &prefix, delim.as_deref(), &start_after, max);
            prop_assert_eq!(got, want);
        }
    }

    #[test]
    fn max_zero_is_not_truncated() {
        let page = build_page(
            b"",
            None,
            0,
            (vec![], false),
            |_, _, _, _| Ok(vec![b"a".to_vec()]),
            |k: &Vec<u8>| k.as_slice(),
            |k: &Vec<u8>| k.clone(),
        )
        .unwrap();
        assert!(page.entries.is_empty() && page.next.is_none());
    }

    #[test]
    fn url_encoding_matches_s3() {
        assert_eq!(url_encode("a b+c/é".as_bytes()), "a+b%2Bc/%C3%A9");
    }
}
