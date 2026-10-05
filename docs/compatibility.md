# litebucket S3 compatibility report

litebucket implements a scoped subset of the Amazon S3 API for single-host
application storage. It is **not** a full S3 replacement. Everything below is
backed by executed tests (see `docs/test-evidence.md`); anything not listed is
unsupported and is rejected explicitly rather than ignored.

## Addressing, transport, and authentication

| Area | Behavior |
|---|---|
| Addressing | Path-style only: `https://host:port/bucket/key`. No virtual-hosted style, no `/api` prefix. |
| Region | One immutable region per store (default `us-east-1`); requests signed for another region get `AuthorizationHeaderMalformed` with a `<Region>` hint. |
| Signatures | AWS SigV4 via `Authorization` header or presigned query (`X-Amz-*`). SigV2, SigV4a/ECDSA, anonymous access, and `x-amz-security-token` (STS) are rejected. |
| Clock skew | 15 minutes for header-signed requests (configurable 60–3600 s). Presigned URLs are valid from their signed time until `X-Amz-Expires` (≤ 7 days); only future-dated URLs beyond the skew window are refused. |
| Signed headers | `host` must be signed; every `x-amz-*` header present (except `x-amz-user-agent`) must be signed. |
| Transport | HTTP/1.1. Built-in TLS (rustls) or plaintext on loopback / behind an explicitly trusted proxy (peer allowlist). No HTTP/2 or HTTP/3. No response compression; stored `Content-Encoding` is returned as stored. |

## Operations

| Operation | Status | Notes |
|---|---|---|
| ListBuckets | supported | `max-buckets`, `continuation-token` (HMAC-authenticated), `prefix`, `bucket-region`. Non-admin credentials see only buckets they hold grants on (requires `list_buckets`). |
| CreateBucket | supported | Admin or `create_bucket` grant. S3 general-purpose naming rules. Existing bucket → `BucketAlreadyOwnedByYou` (never resets it). `LocationConstraint` must be absent for `us-east-1`, or equal the configured region otherwise. |
| HeadBucket / GetBucketLocation | supported | `x-amz-bucket-region` header; `us-east-1` returns an empty `LocationConstraint`. |
| DeleteBucket | supported | Only when empty **and** without open/completing multipart uploads (local restriction). |
| PutObject | supported | Streaming, metadata, checksums, `If-None-Match: *`, `If-Match`. Max 5 GiB (configurable lower). |
| GetObject / HeadObject | supported | One byte range (`a-b`, `a-`, `-n`), conditional reads with S3 precedence, `response-*` overrides, `x-amz-checksum-mode`, `partNumber` (see deviations). |
| DeleteObject | supported | Idempotent; `versionId=null` only. |
| DeleteObjects | supported | ≤ 1000 keys, quiet mode, per-key authorization and results. Requires `Content-MD5` or an `x-amz-checksum-*` header; the whole request is validated before any deletion. |
| CopyObject | supported | Same-instance sources only; `COPY`/`REPLACE` metadata directives; four `x-amz-copy-source-if-*` conditions; ≤ 5 GiB; source read and destination write authorized independently. |
| ListObjectsV2 | supported | `prefix`, `delimiter`, `max-keys` (≤ 1000), `start-after`, `continuation-token`, `encoding-type=url`, `fetch-owner`. |
| CreateMultipartUpload / UploadPart / CompleteMultipartUpload / AbortMultipartUpload / ListParts / ListMultipartUploads | supported | 1–10,000 parts, 5 MiB minimum for non-final parts, 5 GiB maximum part, 100 GiB assembled object (configurable). |
| PutBucketCors / GetBucketCors / DeleteBucketCors / CORS preflight | supported | ≤ 100 rules, 64 KiB body; preflight is unsigned and never mutates. |
| ListObjects (v1) | **rejected** (`NotImplemented`) | Deferred by the spec; no tested client needed it (Rails `delete_prefixed` uses ListObjectsV2). |
| UploadPartCopy, GetObjectAttributes, object tagging, ACL APIs, versioning, Object Lock, retention, legal hold, lifecycle, website, policy, encryption, replication, notifications, presigned POST | **rejected** | `NotImplemented` (or a more specific error) before any mutation. |

## Integrity: payload modes and checksums

| Mechanism | Status |
|---|---|
| `x-amz-content-sha256` hex (signed payload) | verified while streaming; mismatch → `XAmzContentSHA256Mismatch` |
| `UNSIGNED-PAYLOAD` | accepted (transport is TLS, loopback, or an allowlisted proxy) |
| `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` | chunk-signature chain verified |
| `STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER` | chunk chain + trailer signature verified |
| `STREAMING-UNSIGNED-PAYLOAD-TRAILER` | framing, decoded length, and declared trailer verified |
| `Content-MD5` | verified (`InvalidDigest` / `BadDigest`) |
| CRC32, CRC32C, CRC64NVME, SHA1, SHA256 (`x-amz-checksum-*` header or trailer) | verified; stored and returned with `x-amz-checksum-mode: ENABLED` |
| Default when the client selects nothing | CRC64NVME FULL_OBJECT computed and stored |
| Multipart checksum types | CRC32/CRC32C: FULL_OBJECT or COMPOSITE; CRC64NVME: FULL_OBJECT; SHA1/SHA256: COMPOSITE (consecutive part numbers from 1 required) |
| SHA512, XXHash variants, `x-amz-checksum-md5` algorithm | not supported |
| HTTP/1.1 trailer fields (outside aws-chunked) | rejected (never silently dropped) |

ETags: single-part objects use `MD5(content)`; multipart objects use
`hex(MD5(concatenated binary part MD5s))-N`.

## Tested clients (exact versions)

All runs used only `127.0.0.1` endpoints with ambient AWS configuration
cleared (`scripts/interop.sh`), with default checksum settings left on.

| Client | Version | Result | Notes |
|---|---|---|---|
| AWS CLI v2 | 2.37.9 (Python 3.14.6, source install) | 12/12 checks, HTTP and HTTPS | `s3 cp` small/forced-multipart (5 MiB threshold), `sync`, `ls`, `presign`, `s3api` checksum/head/list, recursive `rm`, `rb`. |
| Boto3 / botocore | 1.43.108 | 7/7 tests, HTTP and HTTPS | Requires `Config(signature_version="s3v4")` for **presigned URLs** (botocore's legacy presigner default emits SigV2, which is refused). |
| Ruby `aws-sdk-s3` | 1.229.0 (aws-sdk-core 3.254.1, Ruby 3.4.10) | 9 tests / 31 assertions, HTTP and HTTPS | Explicit CRC32C/CRC64NVME need the optional `aws-crt` gem in the Ruby SDK (client-side limitation); CRC32/SHA1/SHA256 tested. |
| Rails Active Storage | Rails 8.1.3.1 | 12/12 checks, HTTP and HTTPS | Private attachments, signed direct upload (Content-Type/Content-MD5/Content-Disposition), range, existence, `delete_prefixed`, purge. `force_path_style: true`, explicit endpoint/region. |
| Browser | Google Chrome 154.0.8037.93 (headless) | 9/9 checks, HTTP and HTTPS | Allowed-origin preflight + presigned PUT/GET, exposed `ETag`, forbidden origin blocked, unsigned PUT from an allowed origin still `403`. |

Observed client payload modes: over plain HTTP the SDKs send signed
payloads; over HTTPS they send `STREAMING-UNSIGNED-PAYLOAD-TRAILER` with CRC
trailers (and `UNSIGNED-PAYLOAD` for some calls). The signed chunk/trailer
modes are covered by AWS-published signature vectors and wire fixtures.

## Deliberate local behaviors and deviations

| Behavior | Rationale |
|---|---|
| Object keys must contain only XML 1.0 characters (no NUL or other C0 controls except tab/LF/CR). | Every listing stays valid XML. Narrower than S3's key namespace. |
| `GetObject`/`HeadObject` with `partNumber` **is supported** (spec deferred it). | Ruby SDK `download_file` default mode needs it. Part layout of assembled objects is persisted (migration 0002). `x-amz-mp-parts-count` is returned only with `partNumber`, as S3 does. |
| `x-amz-object-annotation-directive: EXCLUDE` or `COPY` accepted on CopyObject; other values rejected. | Sent by AWS CLI v2 by default. Objects here carry no annotations, so both values are exact no-ops. |
| Canned ACL: only absent, `private`, `bucket-owner-full-control`. | Single-owner private store grants nothing else. |
| Storage class: only absent or `STANDARD`. | Single tier. |
| Completion receipts kept 24 h (configurable): an identical `CompleteMultipartUpload` retry returns the original result without rewriting; a changed manifest gets `NoSuchUpload`; a deleted/overwritten object is never resurrected. | Local idempotency extension; AWS makes no identical promise. |
| `CompleteMultipartUpload` responds only after commit (no early-whitespace keepalive). | Configure proxy/client timeouts for large assemblies. |
| Ranged and `partNumber` GETs omit whole-object checksum headers. | A whole-object checksum must not appear to authenticate a partial body. |
| Missing `Content-Length` without `Transfer-Encoding` means an empty body; chunked transfer without `Content-Length` or `x-amz-decoded-content-length` → `MissingContentLength` (411). | HTTP/1.1 semantics; S3 also requires a length. |
| Unknown `x-amz-*` headers and unknown query parameters are rejected (`NotImplemented` / `InvalidArgument`). | Fail closed rather than ignore semantics; `x-id` and SigV4 query parameters are allowlisted. |
| Logical byte quota per bucket (`QuotaExceeded`, 403) set through the local admin API (`litebucket admin bucket set-quota`); no S3 quota API. | Spec §13.3. |
| Access keys are created, scoped, rotated and revoked through the local admin API and stored in the metadata database (spec: a separate operator-managed credentials file). There is no IAM/STS API. | ADR 0004. |
| Disk-reserve or temporary-space exhaustion → `ServiceUnavailable` (503, retryable); admission/queue timeouts → `SlowDown` (503). | Retryable overload signals. |
| Multi-page listings are not a single snapshot. | Each page is one short read transaction; concurrent changes can affect later pages. |
| Bucket names are unique within this service only. | Not a global namespace. |
| `ListMultipartUploads` orders uploads of the same key by upload ID, not initiation time. | Stable, index-backed ordering. |

Never claim "100% S3 compatible": the table above is the contract.
