# Single-Host S3-Compatible Object Storage
## Implementation specification for Codex

> **Note for readers:** this is the original design contract that litebucket was
> built against, kept for reference. `compact-s3` below is the placeholder
> name for `litebucket`. The implementation plan and agent prompts it mentions
> were build-time material and are not part of this repository. For how the
> implementation resolved open points, see
> [implementation-report.md](implementation-report.md); for user
> documentation, start at the [README](../README.md).

**Specification version:** 1.0  
**Prepared:** October 4, 2026  
**Working executable name:** `compact-s3` (a placeholder, not an approved product name)  
**Status:** Implementation contract; the application has not yet been implemented or benchmarked.

---

## 0. How to use this specification

Build the application described here. Do not merely produce a design, scaffold, or collection of success-returning endpoints. Implement it incrementally according to `IMPLEMENTATION_PLAN.md`, keeping tests and a compatibility report alongside the code.

`MUST` and `MUST NOT` identify requirements. `SHOULD` identifies a preferred implementation that can change with a documented reason. Defaults below are proposed engineering decisions that make the specification executable; they are not measured capacity claims or additional user-confirmed requirements.

The user's fixed requirements are:

1. A simple, compact, **single-host** S3-compatible object store.
2. **Rust and Axum** for the application.
3. **Rails Active Storage-style directory sharding**, not a flat object directory.
4. **Embedded SQLite for metadata**, with ordinary filesystem files for object contents.
5. One deployed application executable/process and one local persistent data directory. **No PostgreSQL, Redis, separate worker service, or mandatory external database.**

The complete v1 contract includes multipart uploads and current-client checksum handling. A development milestone that supports only ordinary PUT/GET is not the finished v1 release.

Keep `AGENTS.md` short and explicitly read the larger specification and plan; Codex has a bounded automatic instruction-loading budget. [R40]

Read `AGENTS.md` first, then this document and the implementation plan. If a third-party adapter cannot fulfill a mandatory behavior, document the failing test and fix or replace the adapter boundary. Do not quietly weaken security, durability, or checksums to obtain a green demo.

### Contents

1. Product scope and exclusions
2. Architecture and dependency policy
3. Storage and consistency invariants
4. Filesystem layout
5. Metadata schema and database rules
6. Durable write, read, delete, and copy protocols
7. Multipart upload state machine
8. S3 operations and compatibility contract
9. Listing and pagination
10. Checksums, ETags, and streaming bodies
11. Authentication, authorization, and secret handling
12. HTTP, browser access, and transport security
13. Resource controls and overload behavior
14. Startup, recovery, maintenance, and shutdown
15. Configuration and management commands
16. Backup and restore
17. Observability
18. Repository structure and implementation boundaries
19. Tests and release acceptance
20. Known limitations and deferred work
21. Source references

---

## 1. Product scope and exclusions

### 1.1 Intended use

A small application-storage service for attachments, images, documents, and other uploaded files. Clients use a normal S3 SDK with an explicit endpoint, a configured region, path-style addressing, and application credentials.

Compatibility targets for testing are **AWS CLI v2, Boto3, the Ruby AWS S3 SDK, and private-mode Rails Active Storage**. These are selected test targets, not evidence of compatibility before tests run. Record exact versions and settings in the implementation's compatibility report. A basic browser direct-upload test is also required.

### 1.2 v1 deployment model

| Area | Decision |
|---|---|
| Application | One Rust executable; one running service process per data directory |
| HTTP framework | Axum with Tokio; selective Tower middleware |
| S3 parsing/serialization | Prefer a tested `s3s` adapter behind our own boundary |
| Metadata | SQLite, accessed through `rusqlite` with bundled SQLite |
| File contents | Immutable, ordinary files in sharded local directories |
| Background work | Supervised tasks inside the same process |
| Production operating system | Linux, initially x86-64 and ARM64 |
| Development operating system | Linux; macOS supported where practical, with Linux durability tests authoritative |
| Filesystem | A local filesystem supporting the required locking, synchronization, and no-clobber publication operations; validate on ext4 and/or XFS |
| Network filesystem | Unsupported for the data directory: no NFS, SMB, or shared multi-host volume |
| Region | One immutable configured region; default `us-east-1` |
| Storage class | `STANDARD` only |
| Ownership | One local storage owner, multiple scoped application credentials |
| Administration | Local CLI plus a separate, restricted health/metrics listener; no web administration UI |

No application-managed replication or high availability is promised. RAID, filesystem encryption, off-host backups, and infrastructure redundancy are separate operator responsibilities.

### 1.3 Explicit exclusions

Do not implement clustering, distributed consensus, erasure coding, replication, a full IAM policy engine, STS, cross-account ownership, SigV2, SigV4a, object versioning, delete markers, Object Lock, legal holds, retention guarantees, storage tiers, archive restore, lifecycle transitions, notifications, static website hosting, image processing, antivirus scanning, a search engine, or a billing system.

Also defer virtual-hosted-style addressing, presigned POST forms, object tagging APIs, multipart server-side copy (`UploadPartCopy`), `GetObjectAttributes`, and legacy `ListObjects` v1 unless a mandatory target-client test demonstrates that a small, documented addition is necessary. Do not add them speculatively.

No server-side encryption API is implemented in v1. Files contain the uploaded bytes in plaintext unless the underlying volume or the client encrypts them. Requests that ask for unsupported encryption or retention **must fail**, not return success with fabricated protection headers.

---

## 2. Architecture and dependency policy

### 2.1 Logical architecture

```text
S3 clients
    |
    | HTTP(S), path-style addressing, SigV4
    v
Axum listener + bounded transport middleware
    |
    v
S3 protocol adapter
    |  raw request preservation, operation parsing,
    |  signature verification, XML and S3 response mapping
    v
Application operations + authorization
    |
    +-- Metadata workers --> bundled SQLite in WAL mode
    |
    +-- Blob store --------> sharded immutable local files
    |
    +-- Capacity manager --> bounded transfers, queues and staging space
    |
    +-- Maintenance ------> recovery, garbage collection, checkpointing

Separate restricted listener, same process:
    /livez   /readyz   /metrics

Same executable, offline subcommands:
    init, doctor, check, gc, backup, restore, credentials generation
```

Modules are not microservices. There is no second backend, client application, public management API, or repository-root package hierarchy to invent.

### 2.2 Required dependency decisions

Use stable Rust. During phase 0, choose compatible, maintained crate versions, pin the toolchain, and commit `Cargo.lock`. Do not invent exact crate APIs from memory, depend on an unpinned Git branch, or label a version "latest" without checking it.

Preferred dependencies:

- `axum`, `tokio`, `tower`, and compatible HTTP/body utilities.
- `s3s`, conditionally accepted after the protocol spike.
- `rusqlite` with `bundled`; plain parameterized SQL, not an ORM.
- A small bounded database-worker layer. `tokio-rusqlite` is acceptable only if compatible with the chosen bundled SQLite version and wrapped with explicit queue bounds.
- `serde`, JSON/TOML serialization, a CLI parser, structured tracing, established cryptographic/checksum libraries, and safe filesystem wrappers as needed.
- TLS support with a maintained Rust TLS implementation for deployments that do not use an existing reverse proxy.

Use one Cargo package initially. Optional test dependencies do not belong in the production runtime image. Do not add the AWS SDK as a production dependency merely to implement the S3 server; SDKs belong primarily in interoperability tests.

`rusqlite` can bundle SQLite, and ordinary Tokio filesystem operations currently use blocking execution internally. Keep database calls and blocking filesystem work away from unbounded execution on async request threads. [R03, R04, R05]

### 2.3 `s3s` acceptance gate

Treat `s3s` as a protocol adapter, not as the object-storage implementation or a complete security boundary. Its repository describes the project as experimental; its filesystem sample is intended for testing. The adapter requires deliberate authentication and resource protection. [R01, R02]

Before substantial application development, demonstrate:

1. Axum integration preserves the raw path, query, host, headers, body frames, and required trailers.
2. Signed and presigned requests are accepted only after valid SigV4 verification; unsigned requests fail.
3. Plain, unsigned-payload, signed-chunked, and checksum-trailer requests are either correctly handled or identified as explicit failing v1 requirements.
4. The application can stream data without whole-object buffering and receive trustworthy checksum validation results.
5. Unimplemented operations and headers cannot accidentally fall through to a supported write.
6. Runtime resource limits and authorization are still enforced when the adapter dispatches an operation.

Write an architecture decision record containing the tested versions, fixture results, discovered limitations, and the adoption decision. A small pinned patch/fork is permissible with justification and regression tests. Do not copy the sample filesystem backend and call it production-ready.

---

## 3. Storage and consistency invariants

These invariants take precedence over convenience and micro-optimizations.

| ID | Required invariant |
|---|---|
| INV-01 | A successful object mutation is acknowledged only after the object bytes and required filesystem namespace changes are durable, followed by a successful durable metadata transaction. |
| INV-02 | A public object row references exactly one complete, immutable object file; it never points to a partially received upload. |
| INV-03 | Replacing an object never truncates or edits its currently committed file. |
| INV-04 | Every file has a server-generated storage ID; no bucket name, object key, upload ID, or user filename becomes a physical path component. |
| INV-05 | Completed objects, temporary files, and multipart part files all use two-level directory sharding. |
| INV-06 | Object bytes are not stored as SQLite BLOB payloads. SQLite stores metadata, digests, identifiers, and bounded control records only. |
| INV-07 | No SQLite transaction remains open for the duration of a network transfer, large file copy, or multipart assembly. |
| INV-08 | Authorization is evaluated for every operation and every affected object; possession of an upload ID or pagination token is not permission. |
| INV-09 | Validated length, signature, and checksum failures cannot publish an object or replace a previously committed part. |
| INV-10 | Garbage collection cannot remove a live object, a committed multipart part, or a file being published by an active operation. |
| INV-11 | The process has bounded active transfers, file descriptors, metadata work queues, response-list sizes, and staging usage. |
| INV-12 | At most one application process owns a data directory, including offline maintenance commands. |
| INV-13 | A timeout or lost response is not proof that a mutation did not commit. Unknown commit outcomes are resolved before cleanup. |
| INV-14 | Unsupported protection features fail explicitly; never fake encryption, versioning, retention, ACL changes, or checksum validation. |

### 3.1 Consistency model

Within one running instance, object creation, overwrite, and deletion are linearized by their metadata commits. An operation begun after a successful mutation response must observe that mutation or a later committed mutation.

GET/HEAD select one committed generation. A GET overlapping an overwrite may return the old or new generation, but its headers and entire byte stream must describe the **same** generation. A download that has safely opened the old file may finish after the key is overwritten or deleted.

Listings use a short database snapshot for each response. Multi-page listings are **not** a single snapshot spanning client requests. Concurrent insertions/deletions can affect subsequent pages; document this instead of retaining a database transaction across pagination.

Conditional writes are tested and enforced against current committed state inside the final mutation transaction. A pre-upload check alone is not sufficient. An ETag is not a monotonic version counter: identical content can produce identical ETags despite intervening writes.

### 3.2 Durability boundary

The guarantee assumes the production filesystem and storage device honor successful synchronization operations. Process-kill tests do not by themselves prove resilience to sudden power loss or dishonest device caches. Report exactly which failure modes have been tested.

---

## 4. Filesystem layout

### 4.1 Required layout

```text
data/
├── store.lock
├── metadata.sqlite3
├── metadata.sqlite3-wal        # SQLite-managed, when present
├── metadata.sqlite3-shm        # SQLite-managed, when present
├── objects/
│   └── a1/
│       └── b2/
│           └── a1b2c3d4e5f60718293a4b5c6d7e8f90
├── staging/
│   └── c3/
│       └── d4/
│           └── c3d4e5f60718293a4b5c6d7e8f9012ab.tmp
└── multipart/
    └── e5/
        └── f6/
            └── e5f60718293a4b5c6d7e8f9012ab34cd
```

A `StorageId` is 128 cryptographically random bits, rendered as 32 lowercase hexadecimal characters. Generate a new ID for each received object body, replacement, copied object body, multipart part replacement, and assembled multipart result.

```text
path(area, id) = data_root / area / id[0:2] / id[2:4] / filename(id)
```

`filename(id)` is the ID for immutable final files and the ID plus `.tmp` for staging. The shard components are derived from the internal ID, never from the S3 key. Rails Active Storage's disk service uses the same first-two/next-two directory pattern; the identifier generation and metadata system here are our own. [R06]

There are 256 × 256 = 65,536 possible leaf directories in each area. Create them lazily. This limits expected directory density; it does not impose a hard maximum number of files in a leaf and does not eliminate inode or open-file limits.

### 4.2 Path and file safety

Use exclusive file creation. Publication must not replace an existing storage-ID file, even during an injected ID collision. Before registering an allocated ID, check for an existing database record and any existing staging/object/part path without following symlinks; retry on a collision without claiming or deleting the existing file. The database unique constraint, exclusive creation, and no-clobber publication remain mandatory atomic safeguards—preflight checks do not replace them. All supported writes to this private directory come from the single locked service. An unexpected late path collision is an integrity incident: stop new mutations and automatic cleanup for the affected allocation, preserve the preexisting path, and resolve ownership before recovery deletes anything. Never mark a preexisting unowned file as ordinary garbage merely because a new request collided with its name.

Store validated binary IDs in SQLite and derive paths in one module. Reject malformed internal IDs before any filesystem access. Use directory-relative, no-follow operations through safe wrappers where available; do not follow symlinks during opening, maintenance, or backup traversal.

The data root must be owned by the service account and not writable by untrusted users. Default directories to mode `0700` and files to `0600`, subject to documented operator group-access configuration. A validated root path is not permission to follow arbitrary descendant symlinks.

All three areas must be on the same local filesystem. Startup must reject separate-device staging/final directories rather than falling back to a non-atomic copy-and-delete publication.

### 4.3 Immutable-file rule

A published file is never modified in place. No deduplication, hard-link-based object sharing, refcounting optimization, compression, or packing small objects into archive files in v1. Ordinary file copying for `CopyObject` is intentional: prefer straightforward ownership and recovery over space-saving complexity.

### 4.4 Object keys are not directories

For example:

```text
S3 bucket:   documents
S3 key:      customers/123/invoices/2026/october.pdf
Storage ID:  a1b2c3d4e5f60718293a4b5c6d7e8f90
Disk path:   objects/a1/b2/a1b2c3d4e5f60718293a4b5c6d7e8f90
```

An object ending in `/` is an ordinary object, including a zero-byte folder marker. There is no directory-creation operation associated with an S3 prefix. S3 object names form a flat logical namespace. [R16]

---

## 5. Metadata schema and database rules

### 5.1 SQLite configuration

Initialize WAL mode and verify that the returned journal mode is `wal`. On each appropriate connection set:

```sql
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = FULL;
```

Use one dedicated writer connection and a default of two reader connections. Place a bounded queue before every worker; a wrapper's internal unbounded channel does not meet this requirement. Reader connections must be read-only at the application level. Run checkpoints through the controlled maintenance/writer path.

WAL supports concurrent readers and a writer, but only one writer transaction at a time. `FULL` is required for acknowledged transaction durability; do not substitute `NORMAL` to improve a benchmark. Keep WAL files under SQLite's control. [R07, R08]

Bundle a currently maintained, security-patched SQLite build. Specifically verify the WAL-reset corruption fix documented by SQLite, and avoid withdrawn releases. Record `sqlite_version()` and `sqlite_source_id()` in diagnostics and the release report. The WAL-reset fix exists in 3.51.3 and selected backports; that historical threshold alone is not a sufficient ongoing security policy. [R07, R09]

### 5.2 Data representation

- All persistent timestamps are UTC Unix milliseconds in signed 64-bit integers. Format API dates correctly; filesystem modification times are not authoritative metadata.
- Bucket IDs, storage IDs, and internal generation IDs are 16-byte values.
- Object keys are exact UTF-8 byte sequences stored as SQLite `BLOB`, with bytewise ordered indexes. Never normalize Unicode or use case-insensitive collation.
- User metadata and preserved HTTP headers are validated, bounded JSON objects. They are not arbitrary serialized Rust types.
- ETags are stored without surrounding quotes and quoted at the HTTP/XML boundary where required.
- Internal SHA-256 and MD5 digests are binary. S3 checksum representations are algorithm/type-aware; do not confuse binary, hexadecimal, and Base64 encodings.
- Credentials are not stored in these tables; see section 11.

### 5.3 Required logical tables

| Table | Purpose |
|---|---|
| `store_meta` | Storage-format version, store UUID, configured region, stable owner identity, pagination HMAC key |
| `schema_migrations` | Applied migration identifiers and checksums |
| `buckets` | Bucket identity, creation time, CORS configuration, logical usage, optional logical byte quota |
| `blobs` | Tracked file lifecycle: area, state, immutable file size/digests, checksums, cleanup eligibility |
| `objects` | Current `(bucket_id, object_key)` mapping to an object blob and object metadata |
| `multipart_uploads` | Upload identity, target, state, metadata, checksum mode, completion receipt |
| `multipart_parts` | Current part-number mappings and returned part ETags |

`reference-schema.sql` contains an executable starting schema. It is included later in the assembled document for standalone use. Codex may refine indexes and internal columns through migrations, but not remove the lifecycle tracking or weaken the invariants.

### 5.4 Blob lifecycle

```text
WRITING  --durable file + referencing metadata commit--> READY
WRITING  --definite abort/recovery----------------------> GARBAGE
READY    --reference removed by metadata commit--------> GARBAGE
GARBAGE  --safe physical deletion + bookkeeping--------> row removed
```

Register a `WRITING` row before creating the file. Keep an in-process active-operation guard from registration until the result is conclusively committed or aborted. A READY object blob is referenced by one `objects` row; a READY part blob is referenced by one `multipart_parts` row. Cross-table ownership is an application invariant checked by `doctor` and tests.

A `WRITING` row may have a fully published physical file after a crash. Its mere existence does not authorize publishing the object. Recovery may reclaim it only after confirming no committed reference exists.

Use indexed state/time scans for regular garbage collection. Do not create a second per-object JSON sidecar or store the object key in a filesystem extended attribute as the only recovery mechanism.

### 5.5 Database transaction rules

Bind all SQL parameters. Use short `BEGIN IMMEDIATE` writer transactions for state changes and final precondition checks. Do not build SQL with an object key, bucket name, prefix, or user-provided sort fragment.

Use the immutable bucket ID—not just a bucket name—throughout a write. If an empty bucket is deleted and recreated during an upload, that upload must not accidentally commit into the new bucket.

Update bucket object counts and logical bytes in the same transaction as the object mapping. For overwrite, apply `new_size - previous_size`. Multipart part bytes are physical staging usage, not committed logical object usage. Periodically verify counters; do not recalculate all bucket contents on every request.

If a transaction or connection outcome is uncertain, poison or reconcile the connection before processing more writes. Never return a success on a database error and never delete a just-published file merely because the caller stopped awaiting a worker result.

---

## 6. Durable object protocols

### 6.1 PUT and overwrite

Implement this sequence:

1. Parse/authenticate the request, authorize the immutable target bucket/key, validate supported headers, and acquire bounded transfer/capacity permits.
2. Resolve the bucket ID. Optional preliminary precondition checks can reject obvious failures, but are not authoritative.
3. Allocate/register a new WRITING object blob and an active-operation guard.
4. Exclusively create its sharded staging file. Receive the body incrementally, validating framing, decoded length, signatures, requested checksums, and configured size limits. Compute internal SHA-256 and MD5 while streaming.
5. Complete all trailer/signature/checksum validation. Reject an incomplete or overlong body. Finish buffered writes and synchronize the file.
6. Create/verify the final shard chain, making newly created directory entries durable. Publish the file to its final object path with atomic **no-clobber** semantics. On Linux, a safe wrapper around `renameat2(RENAME_NOREPLACE)` is the preferred primitive.
7. Synchronize required destination and source directory entries, including newly created ancestor entries. A successful rename alone is not the durability boundary. [R10, R11]
8. Acquire the short per-object commit guard. In one writer transaction, confirm the original bucket still exists, reevaluate write preconditions, enforce logical quota, install the new mapping and metadata, mark the new blob READY, update usage, and mark the old blob GARBAGE if replaced.
9. Wait for a successful transaction commit. Release the guard and active-operation ownership, then return the appropriate S3 success response.
10. Reclaim replaced or rejected files later through the lifecycle collector.

No global object-store lock is held during reception or synchronization. If final preconditions fail after file publication, the new file is an unreferenced tracked garbage candidate; the old mapping remains untouched.

Directory creation must have an explicit durability algorithm. Synchronizing a leaf without making a newly created parent entry durable is insufficient. Filesystem `fsync()` and directory `fsync()` have different responsibilities. [R10]

Document and enforce one lock order: upload finalization guard (when applicable), then destination per-key commit guard, then a short metadata writer transaction. Never wait for a key/upload guard from inside the database writer transaction. COPY safely opens/releases the source guard before acquiring the destination guard; it must not hold source and destination guards in arbitrary order. A small bounded/sharded lock registry is preferable to a map that permanently retains every key ever seen.

### 6.2 Cancellation and ambiguous outcomes

Before entering the publication/commit critical phase, a disconnected uploader can be cancelled after its outstanding filesystem work has been drained or safely supervised. Then mark the WRITING record GARBAGE.

Once publication/commit has begun, use a supervised server-owned operation that reaches a known outcome even if the HTTP future disappears. Do not use an untracked fire-and-forget task. The operation's internal ID/storage ID allows a fresh database query to distinguish committed, aborted, and uncertain results.

A response-write failure after commit leaves the committed object valid. A database worker task can still finish after its receiving future is dropped; cleanup must not race that task. On an unreconciled commit/synchronization error, stop new mutations and recover conservatively.

### 6.3 GET and HEAD

Under a short per-key coordination guard, obtain current metadata and, for GET, open the corresponding immutable file safely before releasing the guard. Overwrite/delete commits must use the same coordination scheme. Release database transactions and the key guard before streaming.

A Linux open file descriptor continues to reference an unlinked file until the final reference is closed; nevertheless, the application must close the metadata-lookup/open race rather than assuming the file was already open. [R12]

Hold the download/file-descriptor permit in the response-body owner until EOF, error, or drop. Read only the selected range. Verify file type and expected length before returning headers. If a referenced file is missing or has an unexpected size, report an internal storage-integrity error and alert; do not translate it to `NoSuchKey` or remove the metadata automatically.

HEAD does not open/read the entire content and never emits an XML error body. It must return metadata from one committed generation.

### 6.4 DELETE and batch deletion

Authorize before revealing existence. Under the short per-key coordination guard, delete the object row, adjust counters, and mark its blob GARBAGE in the same durable transaction. Return success after commit; physical deletion can be delayed.

For a missing key in an existing, authorized bucket, ordinary `DeleteObject` returns success. Never make a recursive filesystem deletion from a client-supplied key or prefix. [R25]

For `DeleteObjects`, validate the full bounded XML request and request checksum before changing anything. Process per-key authorization and results. Batch atomicity across all keys is not promised; every reported successful deletion must have committed. Acquire multiple key locks only in a documented stable order, or process bounded individual transactions to avoid deadlocks.

### 6.5 COPY

Resolve and authorize the source independently of the destination. Under the source key guard, evaluate source conditions and open one committed source generation. Release that guard before reading the bytes.

Copy through a bounded buffer into a new tracked file and use the PUT publication protocol for the destination. The source may subsequently be overwritten without corrupting the copy. Do not hold source and destination key locks throughout the copy or share the source blob between keys.

Support `COPY` and `REPLACE` metadata directives with correct content headers and user metadata. Copying an object onto itself solely to replace metadata must work through a new generation. A same-key copy with no effective supported change should fail with the appropriate S3 error, not silently mutate timestamps. [R23]

---

## 7. Multipart upload state machine

### 7.1 Persistent states

```text
OPEN ------complete begins------> COMPLETING ------commit------> COMPLETED
 |                                    |
 | abort / expiration                 | known failure or restart
 v                                    v
ABORTED                              OPEN
```

Use a random opaque upload ID. Every operation verifies the upload's bucket ID and exact key; an upload ID is not a bearer credential. Metadata supplied at initiation is the eventual object's metadata unless the protocol explicitly permits a later value.

OPEN and COMPLETING uploads count toward configured active-upload limits. COMPLETED and ABORTED rows are bounded, expiring receipts/tombstones and do not appear as active uploads.

### 7.2 UploadPart

Accept part numbers 1 through 10,000. A retried part number replaces only that part's mapping after its new bytes have passed validation and become durable. The previous committed part remains valid until then. Parallel uploads of different parts are permitted. [R26, R27]

Each part receives its own storage ID and final path under `multipart/aa/bb/id`. Stream reception through `staging/aa/bb/id.tmp`. Do not create one flat directory containing all parts, and do not interpret an upload ID as a directory name.

A per-upload coordination guard is required for the final state check and part-mapping transaction, not the network reception. Recheck that the upload is still OPEN. A part request that was in flight when completion/abort won the race must not resurrect or alter the closed upload.

### 7.3 Completion

Acquire a bounded completion-worker permit and the upload's exclusive finalization guard. This guard may span local assembly, but must not block all uploads globally or hold a SQLite transaction open. Release it on every exit path.

1. Validate the bounded completion manifest: nonempty, no duplicate part numbers, correct ordering, matching ETags/checksums, valid supported checksum mode, and part-size constraints.
2. All selected non-final parts must be at least 5 MiB. Every selected part is at most 5 GiB. The last part may be smaller. Enforce the application's assembled-size limit with checked arithmetic. [R26, R28]
3. With a short writer transaction, move OPEN to COMPLETING and store a canonical manifest fingerprint and enough selected-manifest information for safe recovery. Allocate/register a new WRITING object blob.
4. Reserve capacity for the **additional assembled file** while the old destination and source parts still exist. Do not assume part files will be removed before assembly finishes.
5. Read selected committed part files sequentially, bounded to a small number of open part descriptors. Concatenate into the new staging file while computing the final internal digest and applicable checksums. The held upload guard prevents replacement/abort from changing the selected parts.
6. Validate final length and any supplied full-object checksum. Construct the multipart ETag from selected part MD5 digests; do not substitute the complete-file MD5.
7. Durably publish the assembled file as an ordinary object file.
8. Acquire the destination's short commit guard. In one transaction, reevaluate conditions and quota, replace the object mapping, mark its blob READY, update bucket counters, mark the upload COMPLETED, save a completion receipt, remove **all** part mappings, and mark their blobs and any replaced destination blob GARBAGE.
9. Commit before returning a completed-upload success. Release guards; cleanup is deferred.

Parts not listed in the successful completion manifest are also reclaimed. They are not silently appended to the object. Without an additional composite checksum, ascending nonconsecutive part numbers may be selected; with composite checksums, apply the S3 consecutive-number requirement and test the error behavior against the pinned adapter/client contract. [R29]

Do not emit an optimistic `200 OK` before validation/commit. v1 can wait and return a complete response rather than reproducing S3's early-whitespace response behavior. Configure proxy/client timeouts accordingly.

### 7.4 Completion retry receipts

Retain a completed-upload receipt for a default of 24 hours. It contains the canonical completion fingerprint, original result metadata, and completion time, not a live reference preventing the blob from being collected forever.

An authenticated and authorized repeat of the **same** completion request can return the recorded result without writing the key again. A different manifest/precondition/checksum set must not reuse the receipt. Never resurrect a deleted or subsequently overwritten object when replaying the receipt. Revalidate current authorization on every retry.

This idempotent receipt is a documented local extension; do not claim AWS promises identical receipt retention. After receipt expiry, the upload can return `NoSuchUpload`.

### 7.5 Abort, expiration, and recovery

Abort acquires the upload finalization guard and atomically marks the upload ABORTED, removes part mappings, and marks parts GARBAGE. In-flight part receptions later fail their OPEN-state recheck. A missing/expired upload returns `NoSuchUpload`; do not create a new upload from an abort request.

Expire abandoned OPEN uploads after the configured inactive interval, default seven days. Update activity when a part is committed, not on every received chunk. Skip uploads with active operations. COMPLETING uploads are owned by the supervised completion task and must not be expired by a timer.

After an unclean restart, no old task owns an upload. If a COMPLETING upload has no atomically committed completion receipt, restore it to OPEN and reclaim unreferenced output files; retain its committed parts for retry. If its completion transaction committed, retain the committed object and receipt and resume part cleanup. Recovery must never infer success from an assembled file alone.

---
## 8. S3 operations and compatibility contract

### 8.1 Required operations

All operations below are mandatory for the completed v1, unless a narrower field-level exception is explicitly documented here. Implement the protocol adapter's S3 types and XML, not a parallel JSON CRUD API.

| Operation | Request shape, path-style | Required behavior |
|---|---|---|
| `ListBuckets` | `GET /` | Authorized visible buckets; bounded listing and supported pagination fields |
| `CreateBucket` | `PUT /{bucket}` | Validate name/region; create atomically |
| `HeadBucket` | `HEAD /{bucket}` | Existence/access check; region hint; no response body |
| `DeleteBucket` | `DELETE /{bucket}` | Only an empty bucket with no active multipart uploads |
| `GetBucketLocation` | `GET /{bucket}?location` | Configured region, including the `us-east-1` empty-location convention |
| `PutObject` | `PUT /{bucket}/{key}` | Streaming create/overwrite, metadata, checksums, conditional writes |
| `GetObject` | `GET /{bucket}/{key}` | Streaming bytes, conditional reads, one byte range, response overrides |
| `HeadObject` | `HEAD /{bucket}/{key}` | Object metadata and conditions, no body |
| `DeleteObject` | `DELETE /{bucket}/{key}` | Idempotent deletion of an unversioned key |
| `ListObjectsV2` | `GET /{bucket}?list-type=2` | Ordered prefix/delimiter listing and opaque cursor pagination |
| `DeleteObjects` | `POST /{bucket}?delete` | Up to 1,000 requested keys, quiet mode, per-key results |
| `CopyObject` | `PUT /{bucket}/{key}` plus `x-amz-copy-source` | Authorized source snapshot; metadata directives; source conditions |
| `CreateMultipartUpload` | `POST /{bucket}/{key}?uploads` | Persistent upload and metadata |
| `UploadPart` | `PUT /{bucket}/{key}?partNumber=N&uploadId=...` | Durable, replaceable part with verified integrity |
| `CompleteMultipartUpload` | `POST /{bucket}/{key}?uploadId=...` | Validated manifest, assembly, atomic publication |
| `AbortMultipartUpload` | `DELETE /{bucket}/{key}?uploadId=...` | Remove logical upload and safely reclaim parts |
| `ListParts` | `GET /{bucket}/{key}?uploadId=...` | Ordered, paginated committed parts |
| `ListMultipartUploads` | `GET /{bucket}?uploads` | Authorized active uploads, prefix/delimiter and marker handling |
| `PutBucketCors` | `PUT /{bucket}?cors` | Authorized, validated, persisted CORS rules |
| `GetBucketCors` | `GET /{bucket}?cors` | Stored configuration or the appropriate missing-configuration error |
| `DeleteBucketCors` | `DELETE /{bucket}?cors` | Remove browser-access configuration |
| CORS preflight | `OPTIONS /{bucket}/{key}` | Evaluate stored CORS rules; never perform an object mutation |

Use operation-specific AWS documentation, including its error catalog and PutObject contract, as the wire-format reference. [R48, R50] Bucket APIs, object APIs, listing, and multipart operations have separate response and error semantics. [R17–R28, R30–R33]

### 8.2 Addressing, dispatch, and key validation

Path-style addressing is mandatory. Do not mount the S3 service under an extra `/api` or `/s3` path prefix. Use the raw request path and query for signature verification; remove the bucket component only at the proper object-key parsing stage.

Dispatch specific S3 subresources before ordinary operations. For example, a PUT with `uploadId`/`partNumber` is not an ordinary object PUT, and `?acl`, `?tagging`, `?retention`, or `?versioning` must not accidentally execute a supported operation.

Preserve repeated slashes, trailing slashes, percent escapes, `+`, spaces, and dot segments at the S3 boundary. Decode object-key bytes exactly once; never use a filesystem path normalizer. Signature canonicalization has S3-specific rules, including not normalizing paths. [R13]

v1 supports nonempty keys of at most 1,024 UTF-8 bytes. Preserve case and Unicode composition exactly. To guarantee valid XML responses without inventing escaping for XML-forbidden characters, this release deliberately rejects code points forbidden by XML 1.0, including NUL, at upload/request validation. Document this as a narrower key-character subset than the broad S3 namespace. Valid tabs, newlines, and carriage returns in keys must be correctly encoded/escaped, never inserted into response headers or logs unsafely.

Bucket names follow the general-purpose S3 bucket rules applicable to the selected API model: lowercase DNS-compatible syntax, appropriate length and reserved-name checks. Names are unique within this service, not globally across AWS. Validate from the official rules instead of an incomplete ad hoc regex. [R15]

### 8.3 Bucket semantics

Use a stable local owner identity generated at initialization. Store one configured region in `store_meta`; refuse startup with a conflicting region after initialization.

For default `us-east-1`, accept the ordinary create request without a location constraint. For another configured region, require the corresponding supported constraint. Reject conflicting region requests with the proper S3 error and provide the configured region hint. `GetBucketLocation` must use the documented empty value for `us-east-1`, not blindly echo a nonempty string. [R17, R18]

Creating an already-owned bucket returns a documented non-destructive result, preferably `BucketAlreadyOwnedByYou`; never reset its metadata or permissions. This deliberately avoids emulating legacy AWS region-specific reset behavior.

DeleteBucket checks emptiness in the same writer transaction as deletion. Existing objects or OPEN/COMPLETING multipart uploads cause `BucketNotEmpty`. Requiring multipart cleanup for general-purpose buckets is a documented conservative local restriction. Terminal receipts may be removed with the bucket. A competing PUT using the old immutable bucket ID must fail rather than attach to a recreated name. [R19]

### 8.4 Object metadata and headers

Persist and return, where appropriate:

```text
Content-Type                     default application/octet-stream
Content-Disposition
Content-Encoding                 exclude transport-only aws-chunked coding
Content-Language
Cache-Control
Expires
x-amz-meta-*                     validated custom metadata
Content-Length                   authoritative decoded stored byte count
Last-Modified                    commit timestamp
ETag                             correctly quoted
x-amz-checksum-*                  section 10 rules
x-amz-checksum-type               where applicable
```

Do not preserve arbitrary incoming headers as object metadata. In particular, never persist authorization, cookies, proxy headers, upload-session headers, or secret values.

Limit custom metadata to 2 KiB of UTF-8 key/value bytes and preserve allowed semantic values with case-insensitive metadata names. [R43] Bound the total HTTP header block separately. Validate header values against CR/LF injection, including download filename overrides. Tests must cover non-ASCII filenames encoded with appropriate `Content-Disposition` parameters.

For authenticated GETs, support response overrides for content type, disposition, encoding, language, cache control, and expiry. These values remain part of the signed request where applicable. They change the response, not stored metadata. [R20]

### 8.5 Conditions and ranges

Required conditional writes: `If-None-Match: *` and `If-Match` for PUT and multipart completion. Enforce conditions in the final metadata transaction, with correct missing-object and mismatch responses. Do not accept an unsupported condition and ignore it. Preserve the old object on failure. [R22]

Required conditional reads: `If-Match`, `If-None-Match`, `If-Modified-Since`, and `If-Unmodified-Since`. Implement S3's documented precedence when more than one condition is supplied; do not evaluate all dates and ETags as an arbitrary conjunction. HEAD must obey the corresponding bodyless response rules. [R20, R21, R44]

Support exactly one byte range, including `bytes=start-end`, `bytes=start-`, and `bytes=-suffix`. Use checked integer parsing. A valid satisfiable GET range returns `206` with the correct `Content-Range`, byte count, and body; an unsatisfiable range returns `416` with appropriate metadata. Bound reads to the selected length. S3 does not support multiple ranges in one GET. [R20]

Explicitly reject multipart range requests instead of silently concatenating ranges. HEAD range behavior must be covered by compatibility fixtures. `GetObject?partNumber=...` is deferred and returns a documented unsupported-feature error rather than a misleading full-object response.

### 8.6 Copy and deletion details

For CopyObject, decode and authorize `x-amz-copy-source` correctly, including URL-encoded keys, and implement the four source condition headers. Reject unsupported source versions and cross-service sources. The source is another bucket/key in this instance, never an arbitrary URL that triggers an outbound fetch.

v1 CopyObject is limited to 5 GiB and the configured lower deployment cap. Larger objects remain retrievable and uploadable through multipart, but large server-side copy requires deferred `UploadPartCopy`. Preserve or replace allowed metadata explicitly; recompute appropriate checksums for the new bytes and response semantics. [R23]

For DeleteObjects, support quiet mode and emit per-key failures without disclosing unauthorized key existence. Validate required request-body integrity before executing the batch. Ordinary missing keys can be reported as deleted; malformed XML and invalid request checksums do not trigger partial execution. [R24]

No version IDs or delete markers are generated. A request explicitly asking for a non-null object version is unsupported. A literal `versionId=null` may be accepted only as the current unversioned object and must be tested; never advertise versioning support because of it.

### 8.7 Unsupported options and error mapping

Create a centralized capability validator. It must recognize unsupported semantic query parameters, XML elements, and `x-amz-*` options before mutation. Maintain a tested allowlist for benign SDK tracing/operation-identification headers; unknown semantic features are rejected, not broadly ignored.

Minimal canned-ACL compatibility may accept absent ACL, `private`, and `bucket-owner-full-control` only because this single-owner private service grants no additional public or cross-account access. All other ACL values, grant headers, and ACL-management APIs fail. Record this restricted behavior instead of claiming ACL support.

Only absent storage class or `STANDARD` is accepted. Explicit server-side encryption, retention, legal hold, Object Lock, tagging, and unsupported checksum requests fail before publication. Disable SigV2 even if the adapter supports it.

Use S3 XML errors with request IDs. Map at least:

| Situation | Expected error family |
|---|---|
| Missing authorized bucket/key/upload | `NoSuchBucket`, `NoSuchKey`, `NoSuchUpload` |
| Invalid or unauthorized credentials | `InvalidAccessKeyId`, `SignatureDoesNotMatch`, `AccessDenied` as appropriate |
| Invalid syntax/value | `InvalidArgument`, `InvalidRequest`, `MalformedXML` |
| Size/checksum failure | `EntityTooLarge`, `EntityTooSmall`, `InvalidDigest`, `BadDigest` |
| Failed conditions | `PreconditionFailed`; conflict-specific error only when applicable |
| Invalid part manifest | `InvalidPart`, `InvalidPartOrder` |
| Unsupported operation | `NotImplemented` or the operation's more specific S3 error |
| Bounded overload | Retryable `SlowDown`/`ServiceUnavailable` with HTTP 503 |
| Referenced file corruption or unexpected I/O fault | `InternalError` and an operator alert |

HEAD errors have no XML body. S3 routes must not leak Axum JSON/text errors, filesystem paths, SQL messages, stack traces, or credential details. Do not convert all internal failures into 404.

---

## 9. Listing and pagination

### 9.1 Object listing

Implement `ListObjectsV2` with `prefix`, `delimiter`, `max-keys`, `start-after`, `continuation-token`, `encoding-type=url`, and `fetch-owner`. Return `Contents`, `CommonPrefixes`, `KeyCount`, `IsTruncated`, and continuation fields correctly. The maximum returned page is 1,000 entries; rolled-up prefixes count against the page budget. [R30]

Use bytewise key order from the `(bucket_id, object_key)` primary key. Prefix queries must use a binary lower bound and, when available, an exclusive prefix-successor upper bound. Do not use unescaped SQL `LIKE`: `%` and `_` are literal characters in object keys, not wildcard operators.

For each grouped prefix, jump to the exclusive bytewise upper bound of that group instead of walking all descendants. Listing one folder containing millions of keys must not require loading/sorting all those keys in memory.

Preserve a distinction between the API-visible last item and the internal scan position. Pagination must not re-emit a `CommonPrefixes` group or skip an adjacent object when a page ends at a group boundary. Include property tests comparing the indexed implementation against a small, clear in-memory reference algorithm.

`max-keys=0` returns an empty bounded response with no unusable continuation loop. Encode only the fields specified by the API when `encoding-type=url` is requested; XML escaping remains a separate step.

### 9.2 Cursor format

Continuation tokens must be opaque to clients and authenticated with an established HMAC-SHA256 implementation. Store the random HMAC key in the metadata store at initialization; do not reuse an application secret key.

A versioned token payload contains the store identity, immutable bucket ID, exact prefix/delimiter/encoding/owner options, principal binding, and the next scan position including inclusive/exclusive semantics. `max-keys` may change on a later page. Only the first page applies `start-after`; reject contradictory attempts to change listing scope during continuation.

Bound token length before decoding; compare authentication tags in constant time. Reject tampering, another store/bucket, mismatched options, malformed bytes, or an unsupported cursor version. Reauthorize the request using current grants even when the token is valid. A bucket deleted and recreated under the same name must not accept an old token.

Tokens are not database snapshots. Persistence of the HMAC key allows ordinary restarts without invalidating tokens, but migration/token-version changes must be documented.

### 9.3 Other listings

ListParts uses its numeric part-number marker and a maximum of 1,000 parts per page. ListMultipartUploads implements its key/upload-ID markers, prefix/delimiter grouping, and stable ordering for the supported general-purpose-bucket contract. Do not reuse an object-list cursor where the S3 API requires a different marker shape. [R27, R31]

ListBuckets must not build an unbounded response. Enforce the configured bucket cap and implement the documented pagination/filter fields supported by the selected API model; global enumeration requires the corresponding application grant. [R32]

All listings read metadata indexes. A recursive filesystem scan is acceptable for an explicit offline verification tool, not an S3 list request.

---

## 10. Checksums, ETags, and streaming bodies

### 10.1 Separate integrity concepts

Implement four independent concepts correctly:

1. SigV4 authentication of the request and, for applicable modes, its payload/chunk chain.
2. An explicitly supplied upload checksum that must be verified.
3. S3 ETag representation and conditional-request comparison.
4. An internal whole-file SHA-256 used for offline integrity checks.

A multipart ETag is not the MD5 of the assembled file. For this unencrypted v1, use the conventional single-PUT MD5 ETag and multipart `hex(MD5(concatenated binary part MD5s))-part_count` ETag. Compute over binary digests, not their hexadecimal strings. [R29]

MD5 and CRC are compatibility/integrity mechanisms, not authentication primitives. Never use them for signing credentials or cursor authentication.

### 10.2 Required algorithm coverage

| Mechanism/algorithm | Single PUT / individual part | Completed multipart object |
|---|---|---|
| `Content-MD5` | Verify when supplied | Not a substitute for an explicit final multipart checksum |
| CRC32 | Verify/return as requested | FULL_OBJECT and COMPOSITE |
| CRC32C | Verify/return as requested | FULL_OBJECT and COMPOSITE |
| CRC64NVME | Verify/return as requested | FULL_OBJECT only |
| SHA1 | Verify/return as requested | COMPOSITE only |
| SHA256 | Verify/return as requested | COMPOSITE only for the S3 multipart checksum; keep an internal whole-file SHA-256 separately |

Follow the specified checksum type and algorithm from initiation through parts, completion, and response metadata. [R41, R49] Validate the request's algorithm declaration, encoded digest length, and any trailer declaration. For composite results, use S3's defined checksum representation and part-count suffix; do not label a full-file digest as composite. For unsupported combinations return an appropriate error. [R29, R42]

When no optional S3 algorithm is selected, compute/store a default CRC64NVME FULL_OBJECT result in addition to the internal digests. Computing full-object CRC during local multipart assembly is sufficient; optimized algebraic CRC combination is not required.

Newer checksum algorithms outside the table, including SHA512, XXHash variants, and an explicit `x-amz-checksum-md5` algorithm interface, are deferred. This does **not** remove `Content-MD5` support. Identify each unsupported algorithm in the compatibility report.

Default SDK checksums cannot simply be disabled in release tests. AWS documents default CRC integrity behavior for current SDKs and CLI, including differing defaults among clients. [R34]

### 10.3 Checksum responses

For a full GET with `x-amz-checksum-mode: ENABLED`, return the stored supported S3 checksum and type. HEAD with checksum mode returns metadata without rereading the whole object. Upload and part responses return required verified checksum fields.

For a range GET, never supply a whole-object checksum as though it authenticates only the returned range. The v1 policy is to omit optional whole-object checksum response headers on partial GETs, while still reporting ETag/range metadata; document and test this field-level behavior with target clients.

No checksum is invented for an existing file that has not actually been computed and validated under the required mode. Returned internal SHA-256 must not masquerade as a supported S3 FULL_OBJECT SHA256 multipart checksum.

### 10.4 Body formats

The finished v1 must pass wire fixtures for:

```text
Ordinary signed payload with hexadecimal x-amz-content-sha256
UNSIGNED-PAYLOAD over an approved secure transport
STREAMING-AWS4-HMAC-SHA256-PAYLOAD
STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER
STREAMING-UNSIGNED-PAYLOAD-TRAILER
```

Implement the valid decoded-content-length, chunk-signature, final-chunk, and checksum-trailer rules for the selected mode. Validate signed trailer chains where applicable. An unsigned payload does not mean an unauthenticated request; the header/query signature must still be valid. [R13, R14, R35]

Distinguish HTTP transfer framing, `aws-chunked` content coding, and S3's multipart-upload API. They are not interchangeable. Persist only decoded object bytes. Strip only the transport-specific `aws-chunked` content coding from stored metadata; do not decompress a legitimate stored `gzip` object.

Do not convert the incoming body to a data-only stream before the protocol layer has consumed required frames/trailers. Axum documents that `Body::into_data_stream()` discards non-data frames. [R36]

Bound framing overhead, per-chunk headers, trailers, header counts, and parser work independently of the decoded object-size limit. Check all size arithmetic for overflow. Extra bytes, missing final chunks, undeclared/duplicate checksum trailers, invalid signatures, and short reads must not publish.

---

## 11. Authentication, authorization, and secret handling

### 11.1 SigV4

Require AWS Signature Version 4 through either the Authorization header or signed query parameters. Verify the algorithm, access key, credential scope/date, service `s3`, configured region, timestamp, signed headers, canonical request, and applicable payload integrity. Compare signatures without timing-sensitive string equality.

Default allowed clock skew for header-signed requests is 15 minutes. Presigned request expiry is evaluated using its signed timestamp and `X-Amz-Expires`, with an upper bound of seven days. Do not apply the 15-minute freshness rule to invalidate an otherwise valid longer-lived presigned URL; only the future-start skew check and its actual expiry apply. [R37]

Support presigned GET, PUT, and HEAD. Clients generate the URL using their SDK; the server validates it. A URL signed for GET cannot be reused as HEAD/PUT unless separately signed for that method.

Reject unsigned S3 requests by default, expired/revoked/unknown credentials, inconsistent authentication modes, temporary-security-token requests that cannot be validated, and unsupported signature algorithms. Do not fall back to anonymous access on a parse or configuration error.

### 11.2 Local permission model

Use a deliberately small allow-only model. No AWS IAM policy language or hidden implicit allow rules.

Each credential has an ID, secret, enabled flag, optional expiry, global grants, and zero or more exact bucket/prefix grants. Prefixes are literal UTF-8 byte prefixes, not glob expressions. An empty prefix grants the whole named bucket. An explicit operator credential may have a global `admin` grant.

| Local action | Scope |
|---|---|
| `read` | GET/HEAD an object; source side of COPY |
| `list` | ListObjectsV2 under an allowed requested prefix; corresponding part/upload listings |
| `write` | PUT; destination COPY; initiate/upload/complete multipart |
| `delete` | DeleteObject/DeleteObjects; abort multipart at the specified target key |
| `manage_bucket` | Inspect/manage that bucket's CORS and permitted bucket administration |
| Global `list_buckets` | Enumerate only permitted buckets, unless admin |
| Global `create_bucket` | Create buckets within an explicitly configured creation allowance, or admin |
| Global `admin` | Full local service operations; never an application default |

The initial implementation may restrict bucket creation to admin and omit a complex creation allowance. Do not accidentally let an ordinary object writer create/delete arbitrary buckets. Require an empty-prefix, whole-bucket `manage_bucket` grant or admin for DeleteBucket and CORS administration; reject nonempty-prefix `manage_bucket` grants as invalid configuration. HeadBucket/GetBucketLocation may disclose the existence/region of a bucket for which the credential has any valid grant, without granting listing or access to other prefixes. Make these operation-to-grant mappings explicit in tests.

For prefix-scoped listing, the requested listing prefix must be contained within a granted prefix. A grant for `customers/123/` does not permit `prefix=""`, `customers/`, or `customers/1234/`. Do not solve this by listing the whole bucket and filtering after pagination. Apply the same rule to common-prefix results and multipart-upload listings.

COPY requires source-read and destination-write independently. DeleteObjects authorizes each key. Multipart operations authorize their actual target even if the caller knows a valid upload ID. Multiple appropriately scoped keys may operate on the same upload; creator ID is audit information, not the only authorization mechanism, so key rotation can preserve access.

For a missing object, use the S3-compatible distinction between permitted listing/existence disclosure and restricted access. No unauthorized request should reveal whether a target exists.

### 11.3 Credentials and rotation

For compactness, load credentials from a separate, operator-managed TOML secret file rather than implementing credential-management APIs or a credential database. The config points to its path. An example is provided in `examples/credentials.example.toml` and must be replaced with generated secrets before use.

The service needs a recoverable secret or equivalent signing material to verify SigV4; storing only a password-style one-way hash is not sufficient. This v1 stores the configured secret in the protected file and memory, not under a false claim of encryption. Protect the file with owner-only permissions or an explicitly validated equivalent secret-mount arrangement. Never log it or include it in ordinary diagnostics.

Provide a local `credentials generate` command that writes a new credential fragment to an exclusively created mode-0600 output file. Generate at least 256 bits of secret entropy. Do not accept a secret as a command-line argument, print it by default, overwrite an existing secret file, or ship usable default credentials.

Reload the entire credential set on SIGHUP after complete validation. Replace the in-memory snapshot atomically. A failed reload keeps the last known-good configuration and emits a redacted alert. New requests use the new set immediately after reload; already-authorized in-flight requests may finish under their original snapshot. State this revocation boundary clearly.

Rotation means add new key, reload, update applications, disable/remove old key, reload. Never allow a disabled key to generate effective new access through an old presigned URL.

### 11.4 Information handling

Never log Authorization, signatures, secret keys, security tokens, raw presigned URLs, request bodies, or custom metadata by default. Treat object keys as potentially sensitive: omit them or log a keyed/redacted identifier unless an operator deliberately enables a protected audit mode.

No telemetry, phone-home call, or external network dependency is required to serve stored objects.

---

## 12. HTTP, CORS, and transport security

### 12.1 Transport

Support HTTPS through configured certificate/key files in the same executable, or operation behind an existing trusted TLS-terminating proxy. A reverse proxy is optional infrastructure, not a second service required by the application.

Development may use loopback HTTP with an explicit configuration setting. Refuse non-loopback plaintext startup unless the operator explicitly selects trusted-proxy/private-network mode. That mode must document firewall/source restrictions; a forwarded-proto header alone is not proof of a secure client connection.

Preserve the client's signed Host/authority and raw path. A proxy must not normalize slashes/dot segments, rewrite the public host or signed query, decompress upload content, strip checksum trailers, or buffer entire uploads. Trust forwarding headers only from explicitly configured proxy addresses, and never use an attacker-supplied forwarded host to bypass signature verification.

HTTP/1.1 is the initial required transport. HTTP/2 is optional only after equivalent raw-path, authority, framing, and checksum tests. HTTP/3 is out of scope.

Disable automatic response compression and transparent request decompression on object routes. Serve bytes and `Content-Encoding` as stored. Use a separate listener for health/metrics to avoid collisions with legal bucket names.

### 12.2 CORS

Buckets have no CORS rules by default. Implement S3-shaped CORS configuration with bounded rule count and body size. [R45–R47] Match origin, requested method, and requested headers; return only appropriate allowed/exposed headers and max-age. Preserve `Vary` semantics for origin-dependent responses. [R33]

Typical explicitly configured allowed methods are GET, HEAD, and PUT; DELETE/POST are allowed only when an operator's rule requires them. Support the documented wildcard matching behavior only in validated CORS fields, not as an authorization wildcard.

Expose ETag, relevant checksum headers, and request ID when configured. A permitted CORS preflight does not grant S3 access; the subsequent request still needs valid authentication and authorization. Preflight itself may be unsigned and must never create an object or disclose protected object metadata.

### 12.3 Request boundary safety

Use the HTTP stack's strict framing parser; reject ambiguous length/framing and malformed headers. Set request-header read timeouts, bounded header bytes/count, idle body progress timeouts, and operation-specific body limits. XML parsers must reject external entities/DTDs and have bounded nesting and collection sizes.

Whole-body buffering is allowed for bounded XML/configuration requests only. It is forbidden for object and part transfers. Do not assume an extractor default limit protects a manually streamed body.

---

## 13. Resource controls and overload behavior

### 13.1 Initial configurable defaults

These are conservative starting policies, not guaranteed performance figures. Validate them in benchmarks and document any changes.

| Setting | Initial default |
|---|---:|
| Active uploads, including part reception | 16 |
| Active downloads | 64 |
| Concurrent server-side copies | 2 |
| Concurrent multipart assemblies | 2 |
| Metadata writer queue capacity | 256 operations |
| Reader worker count | 2 |
| Per-reader queue capacity | 128 operations |
| Metadata busy timeout | 5,000 ms |
| Maximum single PUT | 5 GiB |
| Maximum individual part | 5 GiB |
| Maximum assembled object | 100 GiB |
| Maximum active multipart uploads | 1,024 |
| Multipart part-number bound | 10,000 |
| Maximum logical temporary bytes: committed parts plus active staging | 200 GiB |
| Minimum disk-free reserve | Greater of 1 GiB and 5% of filesystem capacity |
| Minimum free inode reserve, when meaningful | 10,000 |
| Maximum buckets | 1,000 |
| Object listing / part listing page | 1,000 entries |
| Maximum DeleteObjects entries | 1,000 |
| General XML control body | 4 MiB |
| CORS configuration body | 64 KiB |
| Custom metadata bytes | 2 KiB |
| HTTP header block | 32 KiB, 128 headers |
| Request-target length | 16 KiB |
| Transfer buffer target | 256 KiB; bounded small multiples allowed |
| Header read timeout | 10 seconds |
| Body idle-progress timeout | 60 seconds |
| Graceful shutdown allowance | 30 seconds |
| Inactive multipart expiration | 7 days |
| Completed/aborted receipt retention | 24 hours |
| Garbage cleanup batch | 100 blobs |
| Default superseded-file cleanup grace | 60 seconds |

The 100 GiB assembled-object cap is a local product limit, not a claim about AWS's current object-size limit. Configuration must not make limits contradictory: single-PUT/part caps cannot exceed implementation hard limits, and temporary-space policy must account for both parts and assembly output.

### 13.2 Capacity accounting

A capacity manager coordinates admission for concurrent writes. A free-space check in each handler without reservation/accounting is insufficient: simultaneous uploads could all pass the same check.

Account for existing committed multipart parts, active staging, pending assembly output, retained old generations, and physical files awaiting deletion. Known-length operations reserve capacity before writing; unknown-length operations acquire additional bounded capacity while streaming. Conservative accounting is acceptable, but reservations must not leak after cancellation or commit.

Use decoded byte counts for application object limits and actual filesystem availability for disk safety. Also bound encoded/framing overhead. Reconcile capacity counters after restart from tracked metadata and the bounded set of unresolved files, not by scanning every committed object before readiness.

Do not write metadata on every chunk. In-progress counters and reservations may live in process memory; durable file registrations and final state transitions remain in SQLite. Release physical-space accounting only after deletion/closure actually permits reclaim, or refresh the filesystem availability conservatively.

When space falls below reserve, reject new PUT/part/copy/complete admissions with a documented retryable error. Keep reads and feasible delete/abort/maintenance operations available. `ENOSPC`, `EDQUOT`, `EIO`, or a failed synchronization still fail the operation safely even if admission previously succeeded.

### 13.3 Logical quotas

An optional bucket byte quota limits committed logical bytes. Enforce it in the final object transaction; overwriting an object counts the new logical size minus the old size. Quota does not replace temporary-space accounting or protect the disk by itself. Default is no bucket quota.

Quota adjustment may be a local offline CLI command; do not invent a nonstandard S3 quota API. A quota below current usage prevents growth but must not prevent reads or deletion.

### 13.4 Bounded execution and fairness

Permits cover the lifetime of actual work, especially response-body streaming and blocked filesystem work. Do not spawn one unbounded thread/task per request or rely on an unbounded `spawn_blocking` workload as backpressure.

Bound waiting as well as active work. On queue/admission timeout, emit retryable S3 errors. Protect small metadata/read operations from starvation by copy/assembly jobs. Per-credential transfer sublimits may be added using the same bounded manager, not a new rate-limit service.

Directory sharding solves directory density, not process file-descriptor exhaustion. The descriptor budget must include sockets, SQLite files, object/part handles, TLS files, directory handles, and margin. Provide diagnostics and an operator `nofile` example appropriate to the chosen defaults.

---
## 14. Startup, recovery, maintenance, and shutdown

### 14.1 Startup sequence

1. Parse and validate configuration without logging secrets. Resolve paths relative to the config file, not an unpredictable working directory.
2. Secure/open the data root and acquire an exclusive lifetime OS lock on `store.lock`. A PID file alone is insufficient. Do not unlink/recreate the lock file while holding it.
3. Validate directory ownership, symlink policy, same-filesystem requirements, and required filesystem operations. Reject incompatible deployment modes.
4. Open the metadata store with the required SQLite settings. Initialize only through the explicit `init` flow; a missing/corrupt database in a nonempty store must not silently create an empty service.
5. Verify the storage-format version and region, apply compatible migrations transactionally, and perform the configured database integrity checks. Reject a database newer than the executable supports.
6. Recover known WRITING/COMPLETING states conservatively, reconcile bounded in-flight accounting, and queue safe cleanup. Do not require a full scan of all object files for routine startup.
7. Load at least one valid credential configuration; never bind the S3 listener as an anonymously open fallback.
8. Start supervised database/maintenance workers and the restricted management listener.
9. Bind the S3 listener and become ready only after required recovery and checks succeed.

Initialization creates the database, storage roots, immutable store identity, cursor secret, and format metadata durably. It must refuse an existing nonempty/unrecognized target rather than overwrite it.

### 14.2 Recovery decision table

| Observed state | Recovery action |
|---|---|
| WRITING row, staging file only | No surviving active owner after restart; verify no reference, mark garbage |
| WRITING row, final file exists, no reference | Treat as uncommitted publication; reclaim conservatively |
| READY blob with a valid object/part reference | Preserve; its committed mapping is authoritative |
| Referenced file missing or wrong size | Report integrity failure; do not delete metadata or claim success |
| GARBAGE row, file exists | Resume safe physical cleanup |
| GARBAGE row, file already absent | Complete bookkeeping idempotently |
| COMPLETING upload, no completion commit | Reopen the upload, retain committed parts, reclaim abandoned output |
| COMPLETED upload with committed object/receipt | Preserve result; resume obsolete-part cleanup |
| Unknown untracked file | Do not import or delete automatically during normal startup; report through offline doctor/reconciliation |

A rare untracked file can result from manual intervention or an implementation bug. The full verification command must classify it without assuming the filename reveals its original key. Random storage IDs cannot reconstruct a lost bucket/key mapping.

### 14.3 Garbage collection

Select a bounded, indexed batch of GARBAGE rows whose eligibility time has passed and which have no active operation ownership. Revalidate that there is no live object/part reference and that the lifecycle state cannot transition back to READY.

Delete only paths derived from that row's validated ID/area, plus any corresponding staging path. Synchronize deletion metadata as required before removing the tracking row; replay after a crash must be harmless. Never do `remove_dir_all` on a request-derived path.

Existing downloads may keep deleted file descriptors open; capacity estimates must account for delayed physical reclamation. This is a reason to use actual free-space checks rather than assume unlink instantly frees bytes.

Do not remove shard directories during normal online cleanup. Keeping empty shards avoids parent-directory races and is bounded by the fixed sharding layout. Optional offline removal of empty shards can be added later.

A time-based grace period is extra protection, **not** the mechanism preventing deletion of active uploads. State ownership and reference checks are mandatory even after a file looks old.

### 14.4 SQLite maintenance

Use automatic checkpoints or bounded PASSIVE checkpoint work through controlled connections. Observe checkpoint progress and WAL growth. Do not hold long read snapshots or issue blocking TRUNCATE checkpoints during arbitrary uploads. Full checkpoint/truncation is appropriate during controlled offline operations or a clean shutdown after connections are drained.

Never manually delete `metadata.sqlite3-wal` or `metadata.sqlite3-shm` to "fix" startup. Handle unexpected database corruption as an integrity incident, not as permission to rebuild an empty database. [R07]

### 14.5 Shutdown

On SIGTERM/SIGINT, mark readiness false, stop accepting new work, stop maintenance admission, and drain in-flight requests and supervised commits within the configured grace period.

Cancel not-yet-published receptions safely. Do not abruptly abandon an in-progress metadata commit because its HTTP client disconnected. If the process must exit before all work finishes, the persistent state must support the recovery table above.

Close database connections and release the store lock last. Log whether shutdown was fully drained. Container deployment must stop the old process before starting another process against the same data directory; a rolling two-writer overlap is not supported.

---

## 15. Configuration and management commands

### 15.1 Configuration contract

Use TOML for non-secret configuration and a separate credentials TOML file. Validate all numeric ranges, overflow cases, unknown fields, conflicting limits, and incompatible transport settings at startup. Unknown configuration keys are errors, not silently ignored typos.

Precedence: built-in defaults, config file, documented non-secret environment overrides, explicit CLI overrides. Avoid supporting a huge arbitrary environment-to-config transformation. Secrets come from a file or a deliberately supported secret-file environment reference, not command-line flags.

`examples/config.example.toml` is the normative starting shape. Document units in names: `_bytes`, `_seconds`, `_ms`, and integer counts. Use integer byte values in the parsed format, not ambiguous `MB` strings.

The CLI may offer validated human-friendly sizes, but it must show their exact resulting byte count. The region and on-disk format cannot change just because a config value changed.

### 15.2 Required commands

The following names define the intended user interface; Codex must implement and test the commands rather than assume they already exist.

```sh
compact-s3 init --config ./config.toml
compact-s3 serve --config ./config.toml
compact-s3 config check --config ./config.toml
compact-s3 credentials generate --id app-key --output ./app-key.secret.toml
compact-s3 credentials check --file ./credentials.toml
compact-s3 healthcheck --url http://127.0.0.1:9001/readyz
compact-s3 doctor --config ./config.toml
compact-s3 check --config ./config.toml --full
compact-s3 gc --config ./config.toml --dry-run
compact-s3 gc --config ./config.toml --apply
compact-s3 bucket set-quota --config ./config.toml --name documents --bytes 10737418240
compact-s3 backup --config ./config.toml --destination /backup/snapshot-001
compact-s3 restore --source /backup/snapshot-001 --data-dir /srv/compact-s3-restored
```

Commands that inspect/mutate store files offline (`doctor`, `check`, `gc`, quota changes, backup, restore) must acquire the same exclusive lock and fail clearly if the server owns it. They must not bypass the service's transaction and file-lifecycle code.

`config check` performs structural validation without starting the listener. Secret-generation/check commands operate only on their explicit secret-file targets; they do not acquire the object-store lock or contact any remote service. The runtime healthcheck command performs only an HTTP request to the restricted management endpoint.

`doctor` reports schema/settings, reference consistency, counters, filesystem capabilities, and suspected anomalies without deleting data. `check --full` additionally enumerates stored files and verifies their size/internal SHA-256, including committed multipart parts. Its work can be streamed; it need not load a complete object inventory into memory.

`gc --dry-run` is the default inspection mode. `--apply` is explicit and deletes only tracked eligible garbage after revalidation. Unknown untracked files are reported, not deleted automatically. Restoring into a nonempty directory is an error; do not provide a casually destructive overwrite default.

### 15.3 Management listener

Bind to loopback by default. `GET /livez` reports that the process/event loop is alive. `GET /readyz` reports whether startup/recovery completed and mandatory subsystems are healthy. Include a bounded, redacted status distinguishing writable, read-only capacity pressure, and failed integrity state; unhealthy readiness uses a non-200 status.

`GET /metrics` exposes bounded-cardinality metrics without object names, secrets, or metadata. Keep the listener unpublished in the default container configuration. No mutation endpoints are provided here.

---

## 16. Backup and restore

### 16.1 Required v1 backup: offline, consistent, verifiable

An online object-store backup is out of scope. The baseline backup command requires the service to be stopped and takes the store lock. It must not silently perform a racy live copy of the database and object directories.

Create a new mode-0700 backup directory with an incomplete marker. Produce a consistent standalone SQLite snapshot and copy every live object and committed multipart part referenced by that snapshot to the corresponding sharded path. Include immutable store metadata, checksum records, upload state, and a versioned backup manifest.

SQLite's Backup API can produce a database snapshot, but it does not copy external object files; both components are required here. [R38]

Stream the file-copy/manifest work, validate copied lengths and hashes, and use no-follow path handling. Track progress in the backup directory so an interruption is clearly distinguishable from a complete backup. Only after required files, database, manifest, and directory changes are durable may the command publish the completed marker and exit successfully.

The snapshot can retain GARBAGE records whose files were not copied; restore/recovery must handle missing garbage files idempotently. It must contain every file required by a live reference. Normalize/recover interrupted operations before producing the snapshot, without inventing committed objects.

Do not store the backup inside the managed object, staging, or multipart roots. A destination on the same host may be convenient for export but is not protection against host loss. Require operator documentation for moving a verified copy outside that failure domain.

### 16.1a Later addition: online backup

Added after the v1 baseline, on request from an application that deploys the store as a long-running container. It is not a live copy: the running server pauses garbage collection (the only remover of committed files; a pass in progress finishes first), takes one consistent SQLite snapshot with `VACUUM INTO`, and then copies and verifies exactly the files that snapshot references, which therefore cannot disappear meanwhile. The output is the same format as 16.1 and restores the same way. Interrupted-operation recovery is not run (the server already did it at startup; restore runs it again). See ADR 0005.

### 16.2 Credentials and backup secrecy

The object-store backup contains object contents, metadata, and the cursor-authentication secret. It is sensitive. Protect its permissions and transport/storage appropriately.

Credentials/configuration are separate operator-managed files and are **not included implicitly**. Document how to back them up separately or restore with newly provisioned application credentials. Do not leak secrets into a general manifest or log. Store identity/region must be preserved even when credentials are replaced.

### 16.3 Restore

Restore only into a new/empty directory. Validate backup format, manifest, SQLite integrity, referenced file presence, hashes, and filesystem capabilities. Recreate protected permissions and a new process lock file; never restore an active lock as proof of ownership.

Run the same recovery/reference checks before making the restored service ready. A missing or corrupt required file fails restore; do not silently skip it. Verify a restored multipart upload can continue or be aborted as specified.

A release gate must back up a store, restore it into a different empty directory, and compare logical keys, metadata, checksums, bytes, listings, and multipart behavior. A successful database-only restore is insufficient.

---

## 17. Observability

### 17.1 Structured logs

Emit request ID, operation name, sanitized credential ID, outcome/error code, HTTP status, duration, and bounded byte counts. Use separate events for admission rejection, integrity failure, database errors, recovery actions, credential reload failure, and backup completion.

Avoid high-cardinality labels and sensitive key/body logging. Include build version, selected dependency/runtime versions, storage-format version, and effective non-secret limits in startup diagnostics.

### 17.2 Metrics

At minimum expose:

- Request totals/latency by operation and status family; bytes read/written.
- Active transfers/copies/assemblies, rejected admissions, queue depth and wait duration.
- SQLite transaction latency, busy/retry counts, checkpoint progress, WAL bytes.
- Available filesystem bytes/inodes, logical bucket totals, tracked temporary bytes and garbage backlog.
- Active multipart upload count, expiration/abort counts, integrity errors, and recovery actions.

No metric label contains an object key, upload ID, request ID, or unbounded credential identifier. Bucket labels are optional only with an explicit cardinality bound; aggregate defaults are preferred.

### 17.3 Error policy

Distinguish a client error, permission denial, configured capacity limit, temporary overload, and a storage-integrity fault. An integrity fault must not look like normal absence. A request failure must not claim data was definitely unchanged if its commit outcome is still unresolved.

---

## 18. Repository structure and implementation boundaries

### 18.1 Proposed repository

```text
.
├── AGENTS.md
├── SPEC.md
├── IMPLEMENTATION_PLAN.md
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── README.md
├── SECURITY.md
├── .gitignore
├── src/
│   ├── main.rs
│   ├── lib.rs
│   ├── config.rs
│   ├── cli.rs
│   ├── http.rs
│   ├── s3/
│   │   ├── mod.rs
│   │   ├── operations.rs
│   │   ├── capabilities.rs
│   │   └── errors.rs
│   ├── auth.rs
│   ├── metadata/
│   │   ├── mod.rs
│   │   ├── migrations.rs
│   │   └── queries.rs
│   ├── blob_store.rs
│   ├── multipart.rs
│   ├── listing.rs
│   ├── checksums.rs
│   ├── capacity.rs
│   ├── maintenance.rs
│   ├── backup.rs
│   └── telemetry.rs
├── migrations/
├── examples/
├── tests/
│   ├── protocol/
│   ├── storage/
│   ├── recovery/
│   ├── security/
│   └── interoperability/
├── scripts/
├── deploy/
│   ├── Dockerfile
│   └── compose.yaml
└── docs/
    ├── architecture-decisions/
    ├── compatibility.md
    ├── operations.md
    ├── test-evidence.md
    └── benchmarks.md
```

Split modules only when necessary; this tree is guidance, not a requirement to generate empty files. Keep one deployable package. No Loco/Rails application framework, frontend, GraphQL server, or generic multi-database persistence layer.

### 18.2 Internal boundaries

Use concrete modules with focused types rather than a large plugin abstraction. Useful internal contracts include:

```text
StorageId                 validated random binary ID, sole physical-path input
ObjectKey                 validated exact UTF-8 bytes
ObjectRecord              one immutable metadata snapshot/generation
BlobStore                 create staging, publish durable, open immutable, delete tracked
Metadata                  bounded reads and short serialized state transitions
AuthContext               credential snapshot plus evaluated target permissions
UploadSession             persistent multipart identity/state
CapacityPermit            explicit lifetime of admitted physical work
PreparedObject            validated, durably published file awaiting metadata commit
CommitOutcome             committed / definitely not committed / unresolved
```

Make it difficult to call a metadata publish operation with an unvalidated, unsynchronized file. This can be expressed by typed state transitions and private constructors. Types do not replace crash testing.

No application-authored `unsafe` without a narrowly documented need and review. Prefer safe, maintained system-call wrappers. Never implement a new cryptographic primitive or silently substitute a different CRC64 polynomial for CRC64NVME.

No panic/unwrap on network input, filesystem failures, or database results in production paths. Test assertions may use unwrap. Check arithmetic and bound allocations before parsing untrusted lengths.

### 18.3 Build and packaging

Provide reproducible locked builds, formatting/lint checks, tests, a release executable, and a multi-stage container build. Run the image as a non-root user with one writable persistent data mount. Keep secret mounts read-only and do not bake config secrets into image layers.

Target a small runtime image, but measure actual binary/image size instead of setting an invented guaranteed size. Do not remove durability checks or mandatory compatibility code to achieve a cosmetic size target.

The container must require no build toolchain, SQLite CLI, PostgreSQL, Redis, or worker container at runtime. Document any libc/TLS runtime requirements accurately; a "single binary" does not automatically mean a fully static binary.

Do not publish to a registry, deploy a host, change cloud resources, or push repository changes without an explicit user request. The implementation task is to create and test the local application artifacts.

---

## 19. Tests and release acceptance

### 19.1 Test categories

| Category | Mandatory examples |
|---|---|
| Unit/property tests | Key encoding, bytewise prefix bounds, cursor validation, delimiter pagination, ranges, checksum vectors, metadata limits |
| Protocol tests | Operation dispatch, XML shapes, S3 error codes, HTTP statuses, required headers, unsupported-feature rejection |
| Storage tests | Sharding, exclusive creation, same-filesystem publication, overwrite isolation, lookup/open race, copy snapshots |
| Concurrency tests | Conditional winners, simultaneous PUT/DELETE/GET, repeated part upload, complete/abort/part races, bounded queues |
| Crash tests | Forced process termination at every durable state boundary and before/after metadata commits |
| Failure injection | Disk-full, sync failure, short write/read, SQLite busy/error, missing/corrupt file, cancelled worker result |
| Security tests | Forged/expired/revoked signatures, cross-prefix access, parser limits, symlink escape, unsupported protection options |
| Interoperability | Pinned AWS CLI v2, Boto3, Ruby AWS SDK, private Rails Active Storage, browser direct upload |
| Operations | Startup lock, migrations, graceful stop, restore, credential reload, health/metrics, non-root container |
| Resource tests | Memory/FD bounds under large slow transfers; staging limits during multipart assembly |

`IMPLEMENTATION_PLAN.md` provides named acceptance cases and milestone exits. Test evidence must identify exact commands, runtime versions, fixture sizes, filesystem, and result. A skipped/unavailable test is not a pass.

### 19.2 Crash-point matrix

At a minimum, kill the process at these points for new objects, overwrites, parts, and completion where applicable:

```text
After WRITING metadata registration, before file creation
Midway through staging write
After the final byte, before checksum validation
After file synchronization, before publication
After no-clobber publication, before directory synchronization
After directory synchronization, before metadata transaction
During metadata transaction, before commit
Immediately after metadata commit, before HTTP response
After logical deletion, before file unlink
After unlink, before garbage-row deletion
During multipart assembly and during completion transaction
```

On restart, verify all acknowledged operations and all prior objects not successfully replaced/deleted. An unacknowledged operation may be absent or fully committed; a partial visible object is never permitted. SIGKILL fixtures validate process-crash handling; stronger power-loss tests must be reported separately.

### 19.3 Minimum compatibility exercises

For every mandatory SDK/CLI target, test small object upload/download, overwrite, HEAD, list pagination, delete, metadata, default checksums, and a forced multipart transfer. Do not disable checksums or switch to unsigned requests to make tests pass.

For Rails Active Storage specifically, exercise private service configuration, an attachment upload/download, signed direct upload with its required headers, a range download, existence check, and prefix deletion behavior. Set an explicit custom endpoint, matching region, and `force_path_style: true`. Active Storage delegates to the Ruby S3 SDK and uses direct-upload/checksum behavior that should be verified end to end. [R39]

Browser testing must prove a permitted preflight and signed PUT work, a forbidden origin receives no effective CORS permission, and CORS does not bypass authentication.

Use isolated local test credentials/data and an explicitly local endpoint. No tests may discover a real AWS account through environment/default credentials, access production buckets, or incur external storage charges. Clear inherited cloud profiles/credentials in the harness and fail closed on a non-allowlisted endpoint.

### 19.4 Performance/resource evidence

Record idle memory, peak memory/FD count for configured concurrent transfers, executable/image size, PUT/GET throughput, metadata transaction latency, listing latency at increasing key counts, WAL growth, and multipart assembly temporary-space use.

Include a large-stream fixture exceeding available test RAM or use a constrained-memory environment that proves whole-object buffering would fail. Memory growth must follow configured concurrency/buffer bounds rather than object size. Use a separate small-object load test for metadata contention.

Do not make a hard throughput claim without measured hardware/workload details. Do not disable `synchronous=FULL`, directory synchronization, checksums, or authorization in a result labeled production performance.

### 19.5 Definition of done

The v1 release is complete only when:

- Mandatory API operations and body/checksum modes pass their tests with pinned target clients.
- Every INV requirement has automated evidence, including crash and race tests.
- The service starts and restores from its documented one-process deployment without external databases.
- Object, staging, and part paths all use required sharding; no flat-directory fallback exists.
- Authentication defaults are closed, secret handling is documented, and unsupported protection requests fail.
- Backup/restore and integrity checks work on real persisted files, not mocks only.
- Resource bounds and admission failures are demonstrated under concurrent load.
- Compatibility limitations, measured results, deployment steps, and unresolved risks are written accurately.

A passing Rust compile or a happy-path PUT/GET demo is not the release gate.

---

## 20. Known limitations and deferred work

v1 is a scoped S3-compatible implementation, not an Amazon S3 replacement with equivalent availability, durability statistics, maximum object size, or full API coverage.

Known intentional limits include one process/host/volume, path-style addressing, one region, private single-owner storage, 100 GiB default assembled-object limit, offline backups, restricted key character support, no object versioning, no encryption/retention API, no large multipart server-side copy, no advanced checksum algorithms outside the matrix, and non-snapshot multi-page listings.

Local multipart completion receipts, bucket deletion requiring multipart cleanup, and omitted optional full-object checksum headers on ranged GET are documented local behaviors. Never hide them behind a blanket "100% S3 compatible" claim.

Potential later work must be supported by a real requirement and measured bottleneck: virtual-hosted-style addressing, online consistent backup, versioning, object expiration, UploadPartCopy, extra checksum algorithms, GetObjectAttributes, or a different metadata architecture. PostgreSQL is not a planned prerequisite; only reconsider it after measured metadata contention or a deliberate multi-host redesign.

A future clustered service would require changes to object storage, coordination, recovery, and availability—not merely replacing SQLite with PostgreSQL.

---

## Appendix A. Executable reference schema

The accompanying SQL is a validated starting schema, not evidence that the Rust application exists. Invariants spanning tables, request authorization, file durability, and lifecycle transitions must also be enforced in application transactions and tests. The `blobs` table stores digests and file metadata, **not file contents**.

```sql
-- Reference schema for compact-s3 specification 1.0.
-- This is a starting migration, not a complete enforcement of cross-table invariants.
-- journal_mode=WAL and per-connection durability settings are initialization concerns.
PRAGMA foreign_keys = ON;

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

-- Application enforcement additionally required:
-- 1. A referenced blob is READY and has the correct area.
-- 2. A blob is referenced by one object OR one part, never both.
-- 3. A GARBAGE blob has no object/part reference and cannot be revived.
-- 4. Bucket counters/quota are updated with every object mutation.
-- 5. Part mutations only occur for an authorized OPEN upload.
-- 6. COMPLETING output WRITING blobs are associated with an operation for recovery;
--    add an explicit operation/output linkage column if the implementation uses it.
-- 7. All store IDs, owner identity, region, and format keys in store_meta are validated.
-- 8. Lowercase hexadecimal upload IDs and exact key UTF-8/XML policy are validated.
```

## Appendix B. Non-secret configuration example

This is the exact starting configuration from `examples/config.example.toml`. Units are explicit. The local development example binds only to loopback; production transport must follow section 12.

```toml
# Proposed configuration contract, not an implemented application's config.
# Paths are relative to this file. For a local trial, copy into the repository
# root as config.toml, then create a real protected credentials.toml.
data_dir = "./data"
region = "us-east-1"
credentials_file = "./credentials.toml"

[http]
listen = "127.0.0.1:9000"
allow_insecure_loopback_http = true
trusted_proxy_mode = false
max_header_bytes = 32768
max_header_count = 128
max_request_target_bytes = 16384
header_timeout_seconds = 10
body_idle_timeout_seconds = 60
# For direct non-loopback service, enable TLS and disable loopback-only HTTP.
# tls_certificate_file = "/run/secrets/server.crt"
# tls_private_key_file = "/run/secrets/server.key"
# trusted_proxy_mode requires a separately documented restricted-network policy;
# it must not treat arbitrary X-Forwarded-* headers as proof of secure transport.

[management]
listen = "127.0.0.1:9001"
metrics_enabled = true

[database]
reader_connections = 2
writer_queue_capacity = 256
reader_queue_capacity = 128
busy_timeout_ms = 5000
# WAL, FULL synchronization, and foreign keys are required, not speed toggles.

[limits]
active_uploads = 16
active_downloads = 64
active_copies = 2
active_multipart_assemblies = 2
max_single_put_bytes = 5368709120
max_part_bytes = 5368709120
max_object_bytes = 107374182400
max_temporary_bytes = 214748364800
min_disk_free_bytes = 1073741824
min_disk_free_percent = 5
min_free_inodes = 10000
max_buckets = 1000
max_listing_entries = 1000
max_delete_entries = 1000
max_xml_body_bytes = 4194304
max_cors_body_bytes = 65536
max_user_metadata_bytes = 2048
transfer_buffer_bytes = 262144
# No default bucket logical quota; set per-bucket quotas with the offline CLI.

[multipart]
max_active_uploads = 1024
max_parts = 10000
inactive_expiration_seconds = 604800
receipt_retention_seconds = 86400

[maintenance]
garbage_batch_size = 100
garbage_grace_seconds = 60
garbage_interval_seconds = 60
shutdown_grace_seconds = 30

[logging]
format = "json"
level = "info"
log_object_keys = false
```

## Appendix C. Credential-grant example

All entries are disabled and contain placeholders, not usable credentials. Generate real secrets locally and enable only the intended scopes.

```toml
# NONFUNCTIONAL EXAMPLES. There are no usable default credentials.
# Generate secrets locally with the implemented credentials generate command,
# merge the generated secret into an intended grant, enable it, and protect this
# file with mode 0600 (or a validated equivalent secret mount).
# Disabled entries are illustrative; the service refuses to start unless at
# least one usable, valid credential is enabled. Placeholder secrets must never
# be accepted for an enabled credential.

[[credentials]]
id = "local-operator"
secret_access_key = "REPLACE_WITH_GENERATED_SECRET"
enabled = false
global_grants = ["admin"]

[[credentials]]
id = "documents-app"
secret_access_key = "REPLACE_WITH_A_DIFFERENT_GENERATED_SECRET"
enabled = false
global_grants = ["list_buckets"]
# Optional RFC 3339 UTC expiration:
# expires_at = "2027-01-01T00:00:00Z"

[[credentials.grants]]
bucket = "documents"
prefix = ""
actions = ["read", "list", "write", "delete"]

[[credentials]]
id = "customer-123-reader"
secret_access_key = "REPLACE_WITH_ANOTHER_GENERATED_SECRET"
enabled = false
global_grants = []

[[credentials.grants]]
bucket = "documents"
prefix = "customers/123/"
actions = ["read", "list"]
```

## 21. Source references

Primary references checked while preparing this specification on October 4, 2026. The user requirements and normative implementation choices are original to this specification; citations identify protocol/library behavior rather than claims of tested application support. Recheck dependency versions and relevant API changes during implementation.

**[R01] s3s crate documentation — HTTP/S3 adapter, authentication and integration boundaries.**  
`https://docs.rs/s3s/latest/s3s/`

**[R02] s3s project — experimental status and sample-backend scope.**  
`https://github.com/s3s-project/s3s`

**[R03] rusqlite project — bundled SQLite and API.**  
`https://github.com/rusqlite/rusqlite`

**[R04] Tokio filesystem documentation — blocking filesystem execution, buffering and synchronization.**  
`https://docs.rs/tokio/latest/tokio/fs/index.html`

**[R05] tokio-rusqlite — database worker-thread execution model.**  
`https://docs.rs/tokio-rusqlite/latest/tokio_rusqlite/`

**[R06] Rails Active Storage DiskService — first-two/next-two shard pattern.**  
`https://github.com/rails/rails/blob/main/activestorage/lib/active_storage/service/disk_service.rb`

**[R07] SQLite WAL — concurrency, managed files, limitations and WAL-reset fix.**  
`https://sqlite.org/wal.html`

**[R08] SQLite PRAGMA reference — synchronous, foreign_keys and busy_timeout.**  
`https://sqlite.org/pragma.html`

**[R09] SQLite release news — verify patched bundled release during implementation.**  
`https://www.sqlite.org/news.html`

**[R10] Linux fsync — file and directory durability.**  
`https://man7.org/linux/man-pages/man2/fsync.2.html`

**[R11] Linux rename/renameat2 — no-clobber and filesystem boundary semantics.**  
`https://man7.org/linux/man-pages/man2/rename.2.html`

**[R12] Linux unlink — open file descriptor lifetime.**  
`https://man7.org/linux/man-pages/man2/unlink.2.html`

**[R13] S3 SigV4 header authentication and canonicalization.**  
`https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sig-v4-header-based-auth.html`

**[R14] S3 SigV4 streaming payloads.**  
`https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sigv4-streaming.html`

**[R15] S3 general-purpose bucket naming rules.**  
`https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucketnamingrules.html`

**[R16] S3 object keys and namespace behavior.**  
`https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html`

**[R17] S3 CreateBucket API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_CreateBucket.html`

**[R18] S3 GetBucketLocation API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetBucketLocation.html`

**[R19] S3 DeleteBucket API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteBucket.html`

**[R20] S3 GetObject API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html`

**[R21] S3 HeadObject API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_HeadObject.html`

**[R22] S3 conditional writes.**  
`https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html`

**[R23] S3 CopyObject API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_CopyObject.html`

**[R24] S3 DeleteObjects API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObjects.html`

**[R25] S3 DeleteObject API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObject.html`

**[R26] S3 multipart limits — distinguish AWS limits from local product limits.**  
`https://docs.aws.amazon.com/AmazonS3/latest/userguide/qfacts.html`

**[R27] S3 ListParts API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListParts.html`

**[R28] S3 CompleteMultipartUpload API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompleteMultipartUpload.html`

**[R29] S3 upload integrity — checksum types, algorithms and multipart behavior.**  
`https://docs.aws.amazon.com/AmazonS3/latest/userguide/checking-object-integrity-upload.html`

**[R30] S3 ListObjectsV2 API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectsV2.html`

**[R31] S3 ListMultipartUploads API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListMultipartUploads.html`

**[R32] S3 ListBuckets API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListBuckets.html`

**[R33] S3 CORS behavior.**  
`https://docs.aws.amazon.com/AmazonS3/latest/userguide/cors.html`

**[R34] AWS SDK and CLI data-integrity defaults.**  
`https://docs.aws.amazon.com/sdkref/latest/guide/feature-dataintegrity.html`

**[R35] S3 SigV4 streaming trailers.**  
`https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sigv4-streaming-trailers.html`

**[R36] Axum Body — frame and data-stream behavior.**  
`https://docs.rs/axum/latest/axum/body/struct.Body.html`

**[R37] S3 SigV4 query/presigned authentication.**  
`https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sigv4-query-string-auth.html`

**[R38] SQLite online backup API — database-only snapshot boundary.**  
`https://sqlite.org/backup.html`

**[R39] Rails Active Storage S3Service API.**  
`https://api.rubyonrails.org/classes/ActiveStorage/Service/S3Service.html`

**[R40] OpenAI Codex AGENTS.md guidance — repository instructions and loading budget.**  
`https://developers.openai.com/codex/guides/agents-md`

**[R41] S3 CreateMultipartUpload API — checksum selection and metadata.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_CreateMultipartUpload.html`

**[R42] S3 Checksum data type — representations and checksum type.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_Checksum.html`

**[R43] S3 object metadata — header and user-metadata semantics.**  
`https://docs.aws.amazon.com/AmazonS3/latest/userguide/UsingMetadata.html`

**[R44] RFC 9110 — HTTP semantics and conditional/range responses.**  
`https://www.rfc-editor.org/rfc/rfc9110.html`

**[R45] S3 PutBucketCors API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutBucketCors.html`

**[R46] S3 GetBucketCors API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetBucketCors.html`

**[R47] S3 DeleteBucketCors API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteBucketCors.html`

**[R48] S3 error responses and codes.**  
`https://docs.aws.amazon.com/AmazonS3/latest/developerguide/ErrorResponses.html`

**[R49] S3 UploadPart API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_UploadPart.html`

**[R50] S3 PutObject API.**  
`https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutObject.html`
