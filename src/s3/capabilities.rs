//! Operation dispatch and the centralized capability validator.
//!
//! Subresources are matched before ordinary operations, and every query
//! parameter and `x-amz-*` header must be on the selected operation's
//! allowlist. Unsupported protection features (encryption, retention,
//! versioning, ACL grants, tagging, ...) fail before any mutation.

use http::Method;

use super::error::{S3Error, S3Result};
use super::request::S3Request;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    ListBuckets,
    CreateBucket,
    HeadBucket,
    DeleteBucket,
    GetBucketLocation,
    ListObjectsV2,
    DeleteObjects,
    ListMultipartUploads,
    PutBucketCors,
    GetBucketCors,
    DeleteBucketCors,
    PutObject,
    CopyObject,
    GetObject,
    HeadObject,
    DeleteObject,
    CreateMultipartUpload,
    UploadPart,
    CompleteMultipartUpload,
    AbortMultipartUpload,
    ListParts,
    Preflight,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::ListBuckets => "ListBuckets",
            Op::CreateBucket => "CreateBucket",
            Op::HeadBucket => "HeadBucket",
            Op::DeleteBucket => "DeleteBucket",
            Op::GetBucketLocation => "GetBucketLocation",
            Op::ListObjectsV2 => "ListObjectsV2",
            Op::DeleteObjects => "DeleteObjects",
            Op::ListMultipartUploads => "ListMultipartUploads",
            Op::PutBucketCors => "PutBucketCors",
            Op::GetBucketCors => "GetBucketCors",
            Op::DeleteBucketCors => "DeleteBucketCors",
            Op::PutObject => "PutObject",
            Op::CopyObject => "CopyObject",
            Op::GetObject => "GetObject",
            Op::HeadObject => "HeadObject",
            Op::DeleteObject => "DeleteObject",
            Op::CreateMultipartUpload => "CreateMultipartUpload",
            Op::UploadPart => "UploadPart",
            Op::CompleteMultipartUpload => "CompleteMultipartUpload",
            Op::AbortMultipartUpload => "AbortMultipartUpload",
            Op::ListParts => "ListParts",
            Op::Preflight => "CorsPreflight",
        }
    }

    pub fn is_mutation(self) -> bool {
        matches!(
            self,
            Op::CreateBucket
                | Op::DeleteBucket
                | Op::DeleteObjects
                | Op::PutBucketCors
                | Op::DeleteBucketCors
                | Op::PutObject
                | Op::CopyObject
                | Op::DeleteObject
                | Op::CreateMultipartUpload
                | Op::UploadPart
                | Op::CompleteMultipartUpload
                | Op::AbortMultipartUpload
        )
    }
}

/// S3 subresources this service recognizes but does not implement.
const UNSUPPORTED_SUBRESOURCES: &[&str] = &[
    "acl",
    "policy",
    "policyStatus",
    "versioning",
    "versions",
    "tagging",
    "lifecycle",
    "website",
    "logging",
    "notification",
    "replication",
    "encryption",
    "object-lock",
    "retention",
    "legal-hold",
    "accelerate",
    "analytics",
    "inventory",
    "metrics",
    "ownershipControls",
    "publicAccessBlock",
    "requestPayment",
    "intelligent-tiering",
    "torrent",
    "restore",
    "select",
    "select-type",
    "attributes",
    "session",
    "renameObject",
    "metadataTable",
    "metadataConfiguration",
    "abac",
];

const PRESIGN_PARAMS: &[&str] = &[
    "X-Amz-Algorithm",
    "X-Amz-Credential",
    "X-Amz-Date",
    "X-Amz-Expires",
    "X-Amz-SignedHeaders",
    "X-Amz-Signature",
    "X-Amz-Content-Sha256",
    "X-Amz-Security-Token",
];

const RESPONSE_OVERRIDES: &[&str] = &[
    "response-content-type",
    "response-content-language",
    "response-expires",
    "response-cache-control",
    "response-content-disposition",
    "response-content-encoding",
];

/// Resolve the operation from method, path shape, and subresources.
pub fn resolve(req: &S3Request) -> S3Result<Op> {
    for sub in UNSUPPORTED_SUBRESOURCES {
        if req.has_q(sub) {
            return Err(S3Error::not_implemented(format!(
                "The '{sub}' subresource is not supported by this service."
            )));
        }
    }
    let m = &req.method;
    if *m == Method::OPTIONS {
        if req.bucket.is_none() {
            return Err(S3Error::method_not_allowed());
        }
        return Ok(Op::Preflight);
    }
    let op = match (&req.bucket, &req.key) {
        (None, _) => match *m {
            Method::GET => Op::ListBuckets,
            _ => return Err(S3Error::method_not_allowed()),
        },
        (Some(_), None) => {
            if req.has_q("cors") {
                match *m {
                    Method::PUT => Op::PutBucketCors,
                    Method::GET => Op::GetBucketCors,
                    Method::DELETE => Op::DeleteBucketCors,
                    _ => return Err(S3Error::method_not_allowed()),
                }
            } else if req.has_q("location") {
                match *m {
                    Method::GET => Op::GetBucketLocation,
                    _ => return Err(S3Error::method_not_allowed()),
                }
            } else if req.has_q("uploads") {
                match *m {
                    Method::GET => Op::ListMultipartUploads,
                    _ => return Err(S3Error::method_not_allowed()),
                }
            } else if req.has_q("delete") {
                match *m {
                    Method::POST => Op::DeleteObjects,
                    _ => return Err(S3Error::method_not_allowed()),
                }
            } else {
                match *m {
                    Method::GET => {
                        if req.q("list-type") == Some("2") {
                            Op::ListObjectsV2
                        } else {
                            return Err(S3Error::not_implemented(
                                "ListObjects (v1) is not supported; use ListObjectsV2 (list-type=2).",
                            ));
                        }
                    }
                    Method::PUT => Op::CreateBucket,
                    Method::HEAD => Op::HeadBucket,
                    Method::DELETE => Op::DeleteBucket,
                    _ => return Err(S3Error::method_not_allowed()),
                }
            }
        }
        (Some(_), Some(_)) => match *m {
            Method::PUT if req.has_q("uploadId") || req.has_q("partNumber") => {
                if req.headers.contains_key("x-amz-copy-source") {
                    return Err(S3Error::not_implemented(
                        "UploadPartCopy is not supported by this service.",
                    ));
                }
                Op::UploadPart
            }
            Method::PUT if req.headers.contains_key("x-amz-copy-source") => Op::CopyObject,
            Method::PUT => Op::PutObject,
            Method::GET if req.has_q("uploadId") => Op::ListParts,
            Method::GET => Op::GetObject,
            Method::HEAD => Op::HeadObject,
            Method::DELETE if req.has_q("uploadId") => Op::AbortMultipartUpload,
            Method::DELETE => Op::DeleteObject,
            Method::POST if req.has_q("uploads") => Op::CreateMultipartUpload,
            Method::POST if req.has_q("uploadId") => Op::CompleteMultipartUpload,
            _ => return Err(S3Error::method_not_allowed()),
        },
    };
    Ok(op)
}

fn allowed_params(op: Op) -> &'static [&'static str] {
    match op {
        Op::ListBuckets => &[
            "max-buckets",
            "continuation-token",
            "prefix",
            "bucket-region",
        ],
        Op::ListObjectsV2 => &[
            "list-type",
            "prefix",
            "delimiter",
            "max-keys",
            "start-after",
            "continuation-token",
            "encoding-type",
            "fetch-owner",
        ],
        Op::GetBucketLocation => &["location"],
        Op::PutBucketCors | Op::GetBucketCors | Op::DeleteBucketCors => &["cors"],
        Op::ListMultipartUploads => &[
            "uploads",
            "prefix",
            "delimiter",
            "key-marker",
            "upload-id-marker",
            "max-uploads",
            "encoding-type",
        ],
        Op::DeleteObjects => &["delete"],
        Op::GetObject | Op::HeadObject => &["versionId", "partNumber"],
        Op::DeleteObject => &["versionId"],
        Op::CreateMultipartUpload => &["uploads"],
        Op::UploadPart => &["partNumber", "uploadId"],
        Op::CompleteMultipartUpload | Op::AbortMultipartUpload => &["uploadId"],
        Op::ListParts => &["uploadId", "max-parts", "part-number-marker"],
        Op::CreateBucket
        | Op::HeadBucket
        | Op::DeleteBucket
        | Op::PutObject
        | Op::CopyObject
        | Op::Preflight => &[],
    }
}

fn op_headers(op: Op) -> &'static [&'static str] {
    match op {
        Op::PutObject => &["x-amz-acl", "x-amz-storage-class"],
        Op::CopyObject => &[
            "x-amz-acl",
            "x-amz-storage-class",
            "x-amz-copy-source",
            "x-amz-copy-source-if-match",
            "x-amz-copy-source-if-none-match",
            "x-amz-copy-source-if-modified-since",
            "x-amz-copy-source-if-unmodified-since",
            "x-amz-metadata-directive",
            "x-amz-checksum-algorithm",
        ],
        Op::GetObject | Op::HeadObject => &["x-amz-checksum-mode"],
        Op::CreateMultipartUpload => &[
            "x-amz-acl",
            "x-amz-storage-class",
            "x-amz-checksum-algorithm",
            "x-amz-checksum-type",
        ],
        Op::CompleteMultipartUpload => &["x-amz-checksum-type", "x-amz-mp-object-size"],
        Op::CreateBucket => &[
            "x-amz-acl",
            "x-amz-object-ownership",
            "x-amz-bucket-object-lock-enabled",
        ],
        _ => &[],
    }
}

fn takes_metadata(op: Op) -> bool {
    matches!(
        op,
        Op::PutObject | Op::CopyObject | Op::CreateMultipartUpload
    )
}

/// Headers valid on any request; integrity headers are checked by the
/// operations that carry bodies.
const GLOBAL_HEADERS: &[&str] = &[
    "x-amz-date",
    "x-amz-content-sha256",
    "x-amz-user-agent",
    "x-amz-security-token",
    "x-amz-expected-bucket-owner",
    "x-amz-request-payer",
    "x-amz-sdk-checksum-algorithm",
    "x-amz-checksum-crc32",
    "x-amz-checksum-crc32c",
    "x-amz-checksum-crc64nvme",
    "x-amz-checksum-sha1",
    "x-amz-checksum-sha256",
    "x-amz-trailer",
    "x-amz-decoded-content-length",
];

fn unsupported_header_message(name: &str) -> Option<&'static str> {
    if name.starts_with("x-amz-server-side-encryption")
        || name.starts_with("x-amz-copy-source-server-side-encryption")
    {
        Some("Server-side encryption is not supported by this service.")
    } else if name.starts_with("x-amz-object-lock") || name == "x-amz-bypass-governance-retention" {
        Some("Object Lock and retention are not supported by this service.")
    } else if name.starts_with("x-amz-grant-") {
        Some("ACL grants are not supported by this service.")
    } else if name == "x-amz-tagging" || name == "x-amz-tagging-directive" {
        Some("Object tagging is not supported by this service.")
    } else if name == "x-amz-website-redirect-location" {
        Some("Website redirects are not supported by this service.")
    } else if name == "x-amz-copy-source-range" {
        Some("UploadPartCopy is not supported by this service.")
    } else if name == "x-amz-write-offset-bytes" {
        Some("Appends are not supported by this service.")
    } else {
        None
    }
}

/// Validate query parameters and `x-amz-*` headers against the allowlists.
pub fn validate(req: &S3Request, op: Op, owner_id: &str) -> S3Result<()> {
    let params = allowed_params(op);
    for (k, _) in &req.query {
        let ok = params.contains(&k.as_str())
            || k == "x-id"
            || PRESIGN_PARAMS.contains(&k.as_str())
            || (matches!(op, Op::GetObject | Op::HeadObject)
                && RESPONSE_OVERRIDES.contains(&k.as_str()));
        if !ok {
            return Err(S3Error::invalid_argument(format!(
                "Unsupported query parameter: {k}"
            )));
        }
    }
    if let Some(v) = req.q("versionId")
        && v != "null"
    {
        return Err(S3Error::not_implemented(
            "Object versioning is not supported by this service.",
        ));
    }
    if req.has_q("partNumber") && matches!(op, Op::GetObject | Op::HeadObject) {
        return Err(S3Error::not_implemented(
            "Reading an object by partNumber is not supported by this service.",
        ));
    }
    let specific = op_headers(op);
    for name in req.headers.keys() {
        let n = name.as_str();
        if !n.starts_with("x-amz-") {
            continue;
        }
        if GLOBAL_HEADERS.contains(&n)
            || specific.contains(&n)
            || (takes_metadata(op) && n.starts_with("x-amz-meta-"))
        {
            continue;
        }
        if let Some(msg) = unsupported_header_message(n) {
            return Err(S3Error::not_implemented(msg));
        }
        return Err(S3Error::not_implemented(format!(
            "The {n} header is not supported for {}.",
            op.name()
        )));
    }
    if let Some(owner) = req.header("x-amz-expected-bucket-owner")?
        && owner != owner_id
    {
        return Err(S3Error::access_denied().with_detail("expected bucket owner mismatch"));
    }
    if let Some(acl) = req.header("x-amz-acl")?
        && !matches!(acl, "private" | "bucket-owner-full-control")
    {
        return Err(S3Error::not_implemented(format!(
            "Canned ACL '{acl}' is not supported; this service only stores private objects."
        )));
    }
    if let Some(sc) = req.header("x-amz-storage-class")?
        && sc != "STANDARD"
    {
        return Err(S3Error::new(
            "InvalidStorageClass",
            http::StatusCode::BAD_REQUEST,
            "The storage class you specified is not valid",
        ));
    }
    if let Some(v) = req.header("x-amz-object-ownership")?
        && v != "BucketOwnerEnforced"
    {
        return Err(S3Error::not_implemented(
            "Only BucketOwnerEnforced object ownership is supported.",
        ));
    }
    if let Some(v) = req.header("x-amz-bucket-object-lock-enabled")?
        && !v.eq_ignore_ascii_case("false")
    {
        return Err(S3Error::not_implemented(
            "Object Lock is not supported by this service.",
        ));
    }
    if let Some(v) = req.header("x-amz-request-payer")?
        && v != "requester"
    {
        return Err(S3Error::invalid_argument("invalid x-amz-request-payer"));
    }
    // Conditional headers this service does not implement for writes.
    if matches!(
        op,
        Op::PutObject | Op::CopyObject | Op::CompleteMultipartUpload
    ) {
        for h in ["if-modified-since", "if-unmodified-since"] {
            if req.headers.contains_key(h) {
                return Err(S3Error::not_implemented(format!(
                    "{h} is not supported for {}.",
                    op.name()
                )));
            }
        }
        if let Some(v) = req.header("if-none-match")?
            && v.trim() != "*"
        {
            return Err(S3Error::not_implemented(
                "If-None-Match only supports '*' for writes.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, uri: &str, headers: &[(&str, &str)]) -> S3Request {
        let mut h = http::HeaderMap::new();
        for (k, v) in headers {
            h.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        S3Request::parse(
            "id".into(),
            method.parse().unwrap(),
            &uri.parse().unwrap(),
            h,
        )
        .unwrap()
    }

    #[test]
    fn subresources_dispatch_before_ordinary_operations() {
        assert_eq!(
            resolve(&req("PUT", "/b/k?partNumber=1&uploadId=x", &[])).unwrap(),
            Op::UploadPart
        );
        assert_eq!(
            resolve(&req("PUT", "/b/k", &[("x-amz-copy-source", "/b/x")])).unwrap(),
            Op::CopyObject
        );
        assert_eq!(resolve(&req("PUT", "/b/k", &[])).unwrap(), Op::PutObject);
        assert_eq!(
            resolve(&req("GET", "/b?list-type=2", &[])).unwrap(),
            Op::ListObjectsV2
        );
        assert_eq!(
            resolve(&req("POST", "/b?delete", &[])).unwrap(),
            Op::DeleteObjects
        );
        assert_eq!(
            resolve(&req("POST", "/b/k?uploads", &[])).unwrap(),
            Op::CreateMultipartUpload
        );
        assert_eq!(
            resolve(&req("DELETE", "/b/k?uploadId=x", &[])).unwrap(),
            Op::AbortMultipartUpload
        );
        for uri in [
            "/b/k?acl",
            "/b?versioning",
            "/b/k?tagging",
            "/b/k?retention",
            "/b?policy",
        ] {
            let m = if uri.contains('?') { "PUT" } else { "GET" };
            assert_eq!(
                resolve(&req(m, uri, &[])).unwrap_err().code,
                "NotImplemented",
                "{uri}"
            );
        }
        assert_eq!(
            resolve(&req(
                "PUT",
                "/b/k?partNumber=1&uploadId=x",
                &[("x-amz-copy-source", "/b/x")]
            ))
            .unwrap_err()
            .code,
            "NotImplemented"
        );
        assert_eq!(
            resolve(&req("GET", "/b", &[])).unwrap_err().code,
            "NotImplemented"
        );
    }

    #[test]
    fn unsupported_protection_features_fail() {
        let o = "0".repeat(64);
        for (k, v) in [
            ("x-amz-server-side-encryption", "AES256"),
            ("x-amz-object-lock-mode", "GOVERNANCE"),
            ("x-amz-grant-read", "id=x"),
            ("x-amz-tagging", "a=b"),
            ("x-amz-acl", "public-read"),
            ("x-amz-storage-class", "GLACIER"),
            ("x-amz-something-new", "1"),
        ] {
            let r = req("PUT", "/b/k", &[(k, v)]);
            assert!(validate(&r, Op::PutObject, &o).is_err(), "{k}");
        }
        let ok = req(
            "PUT",
            "/b/k?x-id=PutObject",
            &[
                ("x-amz-acl", "private"),
                ("x-amz-meta-a", "1"),
                ("x-amz-storage-class", "STANDARD"),
            ],
        );
        validate(&ok, Op::PutObject, &o).unwrap();
        let bad = req("PUT", "/b/k?foo=1", &[]);
        assert!(validate(&bad, Op::PutObject, &o).is_err());
        let meta_on_get = req("GET", "/b/k", &[("x-amz-meta-a", "1")]);
        assert!(validate(&meta_on_get, Op::GetObject, &o).is_err());
        let v = req("GET", "/b/k?versionId=abc", &[]);
        assert!(validate(&v, Op::GetObject, &o).is_err());
        let v = req("GET", "/b/k?versionId=null", &[]);
        validate(&v, Op::GetObject, &o).unwrap();
        let owner = req("GET", "/b/k", &[("x-amz-expected-bucket-owner", "123")]);
        assert_eq!(
            validate(&owner, Op::GetObject, &o).unwrap_err().code,
            "AccessDenied"
        );
    }
}
