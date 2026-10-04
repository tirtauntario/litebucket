# Implementation plan and acceptance matrix

**Contract:** `SPEC.md`, version 1.0.  
**Purpose:** Incremental implementation instructions, not evidence of completed work.  
**Initial status:** Every milestone and application test below is **not implemented / not run**.

## Working method

Implement one milestone at a time. Commit-sized changes should include tests and an accurate status update. Build runnable behavior early, but never present an intermediate subset as the full v1 product. Security, bounded streaming, and persistence are foundational requirements, not a final hardening phase.

Keep these implementation-generated documents:

| Document | Required contents |
|---|---|
| `docs/implementation-status.md` | Milestone status, requirement/test links, current blockers, commands and results |
| `docs/compatibility.md` | Supported operations/headers/body modes, exact client versions/settings, local deviations, unsupported features |
| `docs/operations.md` | Configuration, credential handling, deployment, quotas, recovery, backup/restore, upgrade procedure |
| `docs/architecture-decisions.md` | Dependency selection, protocol adapter evaluation, filesystem publication/locking design, justified changes |

At every milestone, execute formatting, static analysis, and relevant tests. Record commands and actual outcomes. Failed or unavailable tests remain visible. Production release requires all mandatory acceptance groups, not just unit tests.

---

## Milestone 0 — protocol and dependency feasibility

**Read:** SPEC sections 2, 8, 10, 11, and 19.

Create a small isolated proof of integration, not a production backend. Resolve stable Rust/Axum/Tokio dependencies and evaluate `s3s` with explicit authentication. Verify bundled SQLite is a patched release and that any async wrapper does not force an incompatible or vulnerable SQLite dependency.

Test plain authenticated requests, header-signed and presigned PUT/GET/HEAD, raw URI handling, error serialization, a default-checksum upload from AWS CLI v2/Boto3, and the complete set of required streaming/trailer modes using fixtures where a client does not generate them. Determine whether the adapter exposes the information needed for validation and bounded streaming. Do not assume an advertised feature has been tested.

**Deliverables:** dependency lockfile; adapter boundary proposal; executable smoke tests; evidence table for body modes; explicit gaps and their implementation path. Record the SQLite library version returned at runtime, not merely a crate version.

**Gate:** no silent fallback to anonymous access, disabled client checksums, whole-body buffering, or fabricated success. A protocol gap must have an explicit fix/replacement plan before production operations depend on that path.

## Milestone 1 — executable and storage foundation

**Read:** SPEC sections 3–6, 13–15, and Appendix A.

Implement command-line configuration, `init`, single-owner directory locking, schema migrations, bounded database workers, validated ID/key types, sharded path generation, capacity primitives, and Linux durable-file primitives. Implement validated credential loading; a missing/invalid configuration cannot result in an open service.

Build and test exclusive creation, no-clobber publication, file/directory synchronization, immutable blobs, transaction outcome handling, cancellation ownership, and conservative recovery. Use real temporary filesystem directories and persisted SQLite databases. Keep the skeleton S3 listener closed unless the intended authenticated routes are usable.

**Gate:** a second process/offline writer is refused; malformed paths and injected ID collisions do not damage files; failed publication/commit does not produce an object reference; crash fixtures recover a consistent database and known file lifecycle. Schema migrations are repeatable and newer-format stores are rejected.

## Milestone 2 — authenticated object-storage vertical slice

**Read:** SPEC sections 6, 8, 10–13.

Implement ListBuckets/CreateBucket/HeadBucket/GetBucketLocation/DeleteBucket and PutObject/GetObject/HeadObject/DeleteObject. Connect scoped authorization and validated request bodies to streaming storage. Preserve metadata/ETags, include applicable checksums, and implement required signature/body validation before success.

Add presigned GET/PUT/HEAD; exact key handling; private defaults; supported canned-ACL normalization; rejection of unsupported encryption/retention/subresources. Implement conditional PUT, conditional reads, and single ranges as part of the object path, with final-transaction condition checks.

**Gate:** an authenticated local SDK can upload, overwrite, HEAD, download, and delete; invalid signatures and checksums never publish; reads return one coherent generation during overwrite/delete; oversized or cancelled uploads preserve existing objects. No database transaction remains open across a network transfer.

## Milestone 3 — listing, copy, batch deletion, and quota behavior

**Read:** SPEC sections 8–9 and 13.

Implement indexed ListObjectsV2 prefix/delimiter traversal, opaque scoped continuation tokens, page limits and encoding. Compare results to a simple in-memory reference model, including non-ASCII keys, CommonPrefixes boundaries, start-after, and pagination under concurrent changes.

Implement CopyObject with source/destination authorization, source conditions, metadata directives, and independent durable output. Implement DeleteObjects with request-wide integrity validation before mutation and per-object authorization/results. Add bucket logical counters and offline quota management in the same mutation transactions.

**Gate:** no filesystem scans for listing; tampered/cross-scope tokens fail; copy does not share mutable lifecycle ownership; batch deletion cannot bypass authorization; quotas/counters remain correct under concurrent overwrites and rollback.

## Milestone 4 — multipart state machine and retry behavior

**Read:** SPEC sections 7, 8, 10, and 13–14.

Implement all six mandatory multipart operations, including listings. Persist upload metadata/checksum mode, independently sharded part files, safe part replacement, final assembly, completion condition checks, and completion receipts.

Exercise OPEN/COMPLETING/COMPLETED/ABORTED transitions, the selected-part manifest, non-final part minimums, maximum part count/size, part-checksum validation, assembled limits, complete-request fingerprinting, and storage reservations for parts plus output. Keep assembly outside SQLite transactions.

**Gate:** parallel parts and repeated part numbers work; an in-flight part cannot revive an aborted/completing upload; complete/abort races have consistent outcomes; crash recovery does not expose partial output; successful completion atomically publishes one object and releases all obsolete parts. An identical completion retry returns a receipt without recreating an object that was later replaced/deleted.

## Milestone 5 — complete protocol coverage and target-client verification

**Read:** SPEC sections 8–12 and 19.

Finish and test every required checksum algorithm/type/body mode. Test signed chunk chains, checksum trailers, decoded lengths, multipart composite/full-object checksums, ETags, and default CRC behavior. Add bounded S3 CORS configuration/preflight and browser direct-upload tests.

Run the mandatory AWS CLI v2, Boto3, Ruby SDK, Rails Active Storage, and browser suites with exact recorded versions and explicit local endpoints. Tests must cover default client checksum settings and forced multipart transfers; do not pass by disabling integrity features. Fixtures may supplement but cannot replace actual mandatory client tests.

**Gate:** all mandatory API matrix entries, relevant headers, supported body modes, and compatibility workflows have passing evidence. Document local limits and intentionally unsupported features. Every adapter-specific workaround has regression tests.

## Milestone 6 — operations, stress, backup, and release

**Read:** SPEC sections 13–20.

Complete startup recovery, supervised cleanup/expiry, readiness/liveness, structured redacted logging, bounded metrics, graceful shutdown, doctor/check/GC commands, offline backup, restore validation, and upgrade documentation. Provide a non-root container image definition and deployment instructions without additional database containers.

Run failure injection, repeated crash/restart, resource exhaustion, queue overload, lost-client/cancelled-work, large-stream constrained-memory, and small-object contention tests. Measure memory, open files, metadata latency, filesystem behavior, binary/image size, and throughput with hardware/settings recorded.

**Gate:** the definition of done in SPEC section 19.5 is met. No “100% S3 compatible,” power-loss guarantee, or performance claim exceeds the evidence. Backup/restore is demonstrated with objects, metadata, multipart state, checksum verification, and external credentials/configuration handled as documented.

---

## Minimum acceptance matrix

The implementation should turn these IDs into test names or documented test-suite references. “Pass” requires actual executed evidence; add cases when design changes expose more failure modes.

| ID | Required assertion | Related specification |
|---|---|---|
| DEP-01 | Resolved protocol/dependency versions and runtime SQLite version are recorded and reviewed. | 2, 5 |
| DEP-02 | A missing adapter authenticator cannot start an anonymously open endpoint. | 2, 11 |
| FS-01 | Objects, staging files, and parts all use the two-level ID shard layout. | INV-05; 4 |
| FS-02 | User keys, encoded slashes, dot segments, and malformed IDs never become trusted disk paths. | INV-04; 4, 8 |
| FS-03 | Exclusive creation/publication and injected tracked/untracked ID collisions preserve existing files. | INV-01/03; 4, 6 |
| FS-04 | Synchronization failures at file and directory steps prevent a success acknowledgment. | INV-01; 6 |
| DB-01 | WAL/FULL/foreign-key settings apply correctly to all connections; work queues are bounded. | 5, 13 |
| DB-02 | No network transfer or assembly holds a metadata transaction open. | INV-07; 5–7 |
| DB-03 | Exact UTF-8 byte ordering and prefix bounds agree with a reference model. | 5, 9 |
| DB-04 | Failed migrations and unsupported newer schemas do not expose an empty or partial store. | 14 |
| PUT-01 | Empty, small, and large objects preserve exact bytes and metadata across restart. | INV-01/02; 6, 8 |
| PUT-02 | Invalid length/checksum/signature, oversized input, and cancellation cannot replace the old object. | INV-03/09; 6, 10 |
| PUT-03 | Concurrent If-None-Match creation has at most one successful commit. | INV-01/02; 6, 8 |
| PUT-04 | If-Match is checked in the final transaction, not just before reception. | INV-01/02; 6, 8 |
| GET-01 | GET overlapping overwrite/delete streams one generation with matching headers. | INV-02/03/10; 6 |
| GET-02 | Single ranges, suffix/open-ended ranges, unsatisfiable ranges, HEAD, and conditional precedence work. | 8 |
| GET-03 | A slow/disconnected reader releases handles and permits correctly; old open files remain safe. | INV-10/11; 6, 13 |
| LIST-01 | Prefix/delimiter, CommonPrefixes, max-keys, encoding, and start-after match the reference model. | 9 |
| LIST-02 | Tokens reject tampering, different bucket/principal/scope, and invalid requests. | INV-08; 9, 11 |
| LIST-03 | Paging uses bounded indexed queries, not full filesystem scans or all-key materialization. | INV-11; 9 |
| COPY-01 | Source and destination authorization/conditions are both enforced; metadata directives work. | 6, 8, 11 |
| COPY-02 | Copy survives source overwrite/delete after safe open and fails safely on capacity/commit errors. | INV-02/03/10; 6 |
| DEL-01 | DeleteObject is idempotent for an absent authorized key; deletion visibility precedes physical GC. | 6, 8 |
| DEL-02 | DeleteObjects validates the complete request before any mutation and reports per-object failures. | 8 |
| MPU-01 | Parallel part uploads and safe replacement of the same part number preserve correct mappings. | 7 |
| MPU-02 | Uploaded parts survive restart; ListParts and ListMultipartUploads paginate correctly. | 7–9 |
| MPU-03 | Invalid order, missing parts, wrong ETags/checksums, and illegal sizes cannot complete. | 7, 10 |
| MPU-04 | Complete/abort/part races cannot revive a terminal upload or expose a partial object. | INV-02/10; 7 |
| MPU-05 | Full-object and composite checksum modes produce independently verified expected results. | 10 |
| MPU-06 | Completion conditions and quota are evaluated atomically at final commit. | INV-01/02; 7, 13 |
| MPU-07 | An identical completion retry uses a receipt; a changed manifest fails; no later object is resurrected. | 7 |
| MPU-08 | Expiry and abort free tracked parts without deleting active reception/assembly data. | INV-10; 7, 14 |
| AUTH-01 | Header SigV4, presigned GET/PUT/HEAD, expiry, scope, region, and signed-header checks work. | INV-08; 11 |
| AUTH-02 | Wrong signatures, expired/revoked credentials, and wrong bucket/prefix permissions fail closed. | INV-08; 11 |
| AUTH-03 | Credential reload is atomic; invalid reload preserves the last valid configuration. | 11 |
| AUTH-04 | Copy, batch, listing tokens, and upload IDs cannot bypass scoped authorization. | INV-08; 8, 9, 11 |
| AUTH-05 | Unsupported SSE/retention/versioning/ACL requests fail without fabricated success. | INV-14; 8, 11 |
| BODY-01 | Ordinary signed and allowed unsigned payload modes produce exact stored bytes. | 10 |
| BODY-02 | All three specified streaming modes verify chunk signatures/trailers/decoded length as applicable. | 10 |
| BODY-03 | Bad final trailers/chunks prevent publication; no premature data-only conversion loses trailers. | 10 |
| BODY-04 | Content-MD5 and the five required explicit checksum algorithms validate independently. | 10 |
| HTTP-01 | S3 XML/status/header behavior, HEAD bodylessness, and unknown-subresource rejection are correct. | 8, 12 |
| HTTP-02 | Browser CORS allows configured origins only and never bypasses authentication. | 12 |
| HTTP-03 | Malformed framing, excessive headers/XML, slow bodies, and encoded overhead remain bounded. | INV-11; 12, 13 |
| CAP-01 | Concurrency/queues/buffers/FD counts remain bounded under overload and cancellation. | INV-11; 13 |
| CAP-02 | Concurrent unknown/known-length uploads cannot over-admit reserved disk or staging capacity. | INV-11; 13 |
| CAP-03 | Multipart assembly accounts for parts plus output and preserves the old object on ENOSPC. | INV-01/03; 7, 13 |
| CAP-04 | Logical usage/quota remains correct through overwrite/delete/rollback/restart. | 5, 13 |
| OPS-01 | A second server/offline writer cannot share the data directory; lock inode is never replaced. | INV-12; 14 |
| OPS-02 | Crash points across registration/write/sync/publication/commit/response recover consistent state. | INV-01/02/13; 6, 14, 19 |
| OPS-03 | Lost commit responses are resolved; cleanup never assumes timeout means rollback. | INV-13; 6, 14 |
| OPS-04 | GC deletes eligible tracked garbage only, respects owners, and does not recursively erase unknown files. | INV-10; 14 |
| OPS-05 | Offline backup/restore preserves exact metadata, live bodies, multipart state, and manifest hashes. | 16 |
| OPS-06 | Missing/corrupt referenced files trigger integrity diagnosis rather than silent data recreation. | 14, 16 |
| OPS-07 | Logs/metrics exclude secrets, presigned credentials, and unbounded object-key labels. | 11, 17 |
| SDK-01 | AWS CLI v2 default-checksum ordinary and forced-multipart flows pass against only a local endpoint. | 19 |
| SDK-02 | Boto3 and Ruby SDK default-checksum, metadata, listing, and multipart flows pass locally. | 19 |
| SDK-03 | Rails private attachments, direct upload, range/existence/prefix deletion workflows pass locally. | 19 |
| SDK-04 | A real browser permitted/forbidden-origin preflight and signed upload test passes locally. | 12, 19 |
| PERF-01 | Constrained-memory transfer proves no whole-object buffering; measurements include configuration. | INV-11; 19 |

## Invariant coverage cross-check

| Invariant | Primary acceptance evidence |
|---|---|
| INV-01 Durable acknowledgment | FS-04, PUT-01/02, OPS-02/03 |
| INV-02 Complete immutable referenced file | PUT-01/02, GET-01, MPU-04 |
| INV-03 Never edit the old committed file | FS-03, PUT-02, GET-01, COPY-02 |
| INV-04 Internal IDs only in disk paths | FS-02/03 |
| INV-05 All file areas are sharded | FS-01 |
| INV-06 No object-body BLOB payloads | Schema review and large-stream DB-size assertion in PUT-01/PERF-01 |
| INV-07 Short metadata transactions | DB-02, MPU-04, PERF-01 |
| INV-08 Authorization everywhere | AUTH-01/02/04, LIST-02, COPY-01, DEL-02 |
| INV-09 Invalid integrity/authenticated body never publishes | PUT-02, BODY-02/03/04, MPU-03 |
| INV-10 Collector protects live/active files | GET-01/03, MPU-04/08, OPS-04 |
| INV-11 Bounded resources | CAP-01/02/03, LIST-03, PERF-01 |
| INV-12 One owner process | OPS-01 |
| INV-13 Reconcile unknown commit outcomes | OPS-02/03 |
| INV-14 Never fake protection support | AUTH-05, HTTP-01 |

## Release evidence template

For each test group, record: application revision, operating system/filesystem, Rust/dependency/client versions, configuration, command, outcome, artifacts/logs, and unresolved limitations. Distinguish unit tests, process-crash tests, power-loss tests, and benchmark evidence. Do not convert “not run” into “passed” in summaries.
