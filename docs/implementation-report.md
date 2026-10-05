# litebucket — implementation report

This report covers what was built against `docs/SPEC.md` 1.0, how, what was
verified, and **every assumption made where the specification left room**.
Supporting detail: `docs/test-evidence.md`, `docs/benchmarks.md`, `docs/compatibility.md`,
`docs/operations.md`, ADRs in `docs/architecture-decisions/`.

## 1. Outcome

All seven milestones (0–6) are implemented in one Cargo package at the repo
root, producing one executable, `litebucket`. Every acceptance ID in the plan has
executed evidence (60/60 pass). Real clients pass over HTTP and HTTPS: AWS CLI
v2, Boto3, the Ruby SDK, Rails Active Storage, and headless Chrome. The full
test suite, including a process crash matrix over every durable boundary,
passes on macOS (APFS) and on Linux (ext4, Docker). Not tested: power loss,
x86-64, and XFS.

Size: ≈ 13.4k lines of production Rust in `src/` (plus ≈ 1.8k lines of unit tests), ≈ 4k lines of integration tests, plus interop
suites in Ruby/Python/shell. Release binary 6.7 MB; container image 64.3 MB.

## 2. Architecture as built

```text
hyper http1 (+ rustls) ──► axum fallback ──► s3::handle
   limits → parse raw URI → resolve op (subresources first) → capability
   allowlist → SigV4 (header/presigned) → handler → S3 XML errors/CORS/log
                                   │
          ┌────────────────────────┼─────────────────────────┐
          ▼                        ▼                         ▼
   Store (typestate blob     Db: 1 writer + N reader     Capacity: permits +
   pipeline, key/upload      threads, bounded queues,     disk/temp reservations
   guards, supervision)      group commit (savepoints)
          │                        │
          ▼                        ▼
   data/objects|staging|multipart/aa/bb/<id>   data/metadata.sqlite3 (WAL, FULL)
```

- Durable write path: register WRITING row → exclusive staging file → stream
  + hash (MD5, SHA-256, requested S3 checksums) → verify → fsync → no-clobber
  rename + directory syncs → one short transaction (preconditions, quota,
  mapping, READY, old blob GARBAGE, counters). Publication+commit run as a
  supervised task so client disconnects can't interrupt them (ADR 0002).
- Uncertain commits are reconciled by re-reading blob state; an unresolvable
  outcome or any storage `EIO` halts mutations until restart.
- Group commit batches queued write transactions into one durable commit with
  a savepoint each; results are delivered only after that commit (ADR 0003).
- GC, multipart expiry, receipt expiry, WAL checkpoints, gauges, and
  periodic counter verification run as in-process tasks.
- Offline commands (`doctor`, `check --full`, `gc`, `bucket set-quota`,
  `backup`, `restore`) take the same lock.

## 3. Verification summary

| Area | Evidence |
|---|---|
| Unit + integration | 81 unit tests, 57 integration tests (objects 14, protocol 13, multipart 9, operations 13, crash 8); all pass on macOS and Linux/ext4 |
| SigV4 | Every AWS-published example signature (GET/PUT/list/lifecycle/presigned/chunked/trailer) reproduced exactly |
| Crash safety | SIGABRT at every durable boundary for new objects, overwrites, part replacement, multipart completion, delete, and GC, followed by recovery and offline `check --full` |
| Fault injection | fsync EIO, directory-sync EIO, write ENOSPC, uncertain commit, forced tracked/untracked storage-ID collisions |
| Real clients | AWS CLI 2.37.9, Boto3 1.43.108, aws-sdk-s3 (Ruby) 1.229.0, Rails 8.1.3.1, Chrome 154: all pass over HTTP and HTTPS |
| Resources | Peak server RSS < 8 MiB while streaming a 1 GiB object; ≈ 13 MiB with 8 concurrent 64 MiB transfers; small 1 KiB PUT 1406/s on Linux/ext4 (16 clients) (`docs/benchmarks.md`) |
| Container | Non-root, read-only root fs, `cap_drop: ALL`; TLS round trip, healthcheck, drained shutdown, clean `check --full` |

Defects found by the evidence work and fixed: Ruby `download_file` needed
`partNumber` reads; AWS CLI v2 sends `x-amz-object-annotation-directive`;
completion-request checksum headers were wrongly applied to the XML body;
completion validated part sizes before part existence; any storage `EIO` (not
only file fsync) now halts mutations; crash failpoints needed per-transition
names (the generic one had fired on the registration commit); small-write
throughput was bounded by one WAL flush per transaction (fixed by group
commit) and by an unnecessary staging-directory sync (removed).

## 4. Assumptions and decisions

These were made without asking, per the instruction to assume when in doubt.
Each is recorded where it applies; this list is the single overview.

### Project and dependencies

1. **Name and layout.** `compact-s3` → `litebucket` everywhere; one package at the
   repo root; the spec bundle stays in `docs/` and generated docs live there too.
2. **Protocol adapter.** `s3s` 0.17 was evaluated by source review and **not
   adopted**; litebucket has its own SigV4/XML/routing boundary so the
   capability validator, checksum pipeline, and fail-closed auth are fully
   under its control (ADR 0001). The spec prefers `s3s` but allows replacement.
3. **Toolchain.** Pinned to the installed rustc 1.97.1. RustCrypto hashes on
   the 0.10/0.12 API line (0.11 exists) for API stability.
4. **No async SQLite wrapper.** Dedicated threads own `rusqlite` connections
   behind bounded `tokio` channels.
5. **Group commit** (not in the spec) was added after measuring one-commit-per-
   transaction throughput; durability semantics are unchanged (ADR 0003).

### Configuration and credentials

6. Keys added beyond the example config (all optional with safe defaults):
   `[secrets]`, `[admin]` (see 8–9), `http.trusted_proxy_addresses`,
   `http.max_clock_skew_seconds`, `database.queue_wait_ms`,
   `limits.admission_timeout_ms`.
7. Environment overrides are limited to listen addresses and log level/format;
   CLI overrides to `--listen`, `--management-listen`, `--log-level`.
8. Access keys live in the metadata database and are managed through a local
   admin API (ADR 0004; a deliberate deviation from the spec's credentials
   file). Key ids: 3–128 chars of `[A-Za-z0-9._-]` (no `/`, which would break
   SigV4 scopes); generated ids are `SL` + 18 base32 characters. Secrets are
   generated server-side (256-bit, base64url), returned once on create/rotate,
   and never accepted from callers.
9. Secret protection is configurable: `encrypted` (default; AES-256-GCM under
   a master key file outside `data_dir`, AAD = store id + key id) or
   `plaintext`. Startup converts stored secrets to the configured mode in one
   transaction and refuses to start if any secret cannot be decrypted. The
   master key file must not be group/other-readable; symlinks (secret mounts)
   are followed.
10. Trusted-proxy mode means "only these peer IPs may connect"; forwarded
    headers are ignored entirely.

### Authentication and authorization

11. `UNSIGNED-PAYLOAD` is accepted on every transport the server accepts at all
    (TLS, loopback, allowlisted proxy).
12. Every `x-amz-*` header must be signed except `x-amz-user-agent`.
13. Canonical URI = AWS UriEncode of the once-decoded path; the raw path is
    tried as a second candidate for clients that over-encode. A literal `+` in
    the query means `+`.
14. Disabled, unknown, and expired credentials all return
    `InvalidAccessKeyId`.
15. Missing key: `NoSuchKey` only if the caller may list a prefix covering the
    key; otherwise `AccessDenied` (no existence disclosure).
16. `HeadBucket`/`GetBucketLocation` need any grant on the bucket; `ListParts`
    and `ListMultipartUploads` need `list`; creating buckets needs `admin` or
    `create_bucket`; `DeleteBucket` and CORS need whole-bucket `manage_bucket`.
17. `x-amz-expected-bucket-owner` must equal the store's owner ID;
    `x-amz-request-payer` accepts only `requester` (no-op).

### Protocol behavior

18. Unknown query parameters → `InvalidArgument`; unknown `x-amz-*` headers and
    recognized-but-unsupported subresources → `NotImplemented`. `x-id` is
    allowlisted.
19. Local error codes: logical quota → `QuotaExceeded` (403); disk/temporary
    capacity → `ServiceUnavailable` (503); permit/queue timeouts and the
    active-multipart cap → `SlowDown` (503); metadata > 2 KiB →
    `MetadataTooLarge`.
20. No `Content-Length` and no `Transfer-Encoding` = empty body; chunked
    transfer without a declared length → `MissingContentLength` (411). Unknown-
    length uploads are therefore not supported (S3 requires a length too).
21. HTTP trailer frames outside aws-chunked are rejected rather than dropped.
22. aws-chunked framing overhead is capped at 64 KiB + 1/8 of the decoded size.
23. Ranged and `partNumber` GETs omit whole-object checksum headers.
24. `encoding-type=url` uses S3's form-style encoding (space → `+`).
25. Listing tokens carry only a version, an inclusive flag, the scan position,
    and an HMAC over (store ID, bucket ID, prefix, delimiter, flags, principal,
    position). `start-after` is ignored when a token is present.
26. `ListMultipartUploads` orders uploads of one key by upload ID.
27. `x-amz-mp-parts-count` is returned only with `partNumber`.
28. CopyObject is limited to min(5 GiB, `max_single_put_bytes`); a same-key
    copy needs `REPLACE` or a new checksum algorithm; the copy's bytes are
    re-verified against the source's recorded SHA-256.
29. `x-amz-object-annotation-directive` `EXCLUDE`/`COPY` is accepted (no-op);
    other values are rejected.
30. **`partNumber` reads are implemented** (spec deferred them) because the Ruby
    SDK's default `download_file` requires them; part layouts are stored in a
    new migration (0002).

### Multipart

31. Uploads with no algorithm default to CRC64NVME FULL_OBJECT (computed over
    the assembled bytes); parts always store a checksum of the upload's
    algorithm, computed server-side.
32. An explicit algorithm makes parts with a different algorithm an error; an
    explicit COMPOSITE upload requires a checksum for every listed part and
    part numbers 1..N with no gaps.
33. A full-object checksum header on completion is accepted only for explicit
    uploads of the same algorithm.
34. The receipt fingerprint covers the parts (numbers, ETags, checksums),
    conditions, checksum type, and `x-amz-mp-object-size`.
35. Assembly verifies every part's size and MD5 against metadata while
    copying; a mismatch is an integrity fault.

### Storage, recovery, and maintenance

36. The staging-shard entry is **not** synced at file creation (the WRITING
    row is already durable and recovery tolerates a missing staging file);
    durability is established at publication (file fsync, destination and
    source directory syncs). Shard directory chains are synced on first use
    per process.
37. Integrity faults (missing/short files) mark readiness unhealthy but do not
    halt writes; storage `EIO` does halt writes.
38. GC's grace period (60 s) applies online; offline `gc --apply` ignores it.
39. Counters are verified online one bucket per 6 hours (bounded work) and in
    full by `doctor`.
40. Startup runs `PRAGMA quick_check`; `doctor` runs the full
    `integrity_check`.
41. Startup probes `O_EXCL`, `RENAME_NOREPLACE`, and file/directory sync with
    throwaway names in the staging root and refuses unsupported filesystems.
42. Backups use `VACUUM INTO` for the SQLite snapshot (consistent and compact)
    and a JSON-lines file manifest; restore always creates a fresh lock file.

### Operations and packaging

43. Container base: distroless `cc-debian13:nonroot` (uid 65532); compose
    example uses TLS secrets, a read-only root filesystem, and `cap_drop: ALL`.
44. Test-only failpoints (`--features failpoints`) arm only once the server is
    ready; they never exist in release builds.
45. Request logs carry `request_id`, `operation`, `credential`, `status`,
    `code`, `detail`, `duration_ms`, `payload` mode; object keys only with
    `log_object_keys = true`.
46. Configuration templates: `litebucket config template [--docker]` prints the
    commented templates `docs/examples/config.example.toml` and
    `deploy/config.toml` (embedded at build time). A unit test uncomments every
    documented `# key = value` line and checks that it equals the built-in
    default, so the templates cannot drift from the code.
47. Docker setup: `deploy/setup.sh` creates `compose.yaml`, `.env` (image
    pinned to the exact version), `config.toml`, the master key and a
    self-signed certificate, runs the explicit `init` (the spec forbids
    implicit initialization), saves the first admin key from its output to
    `secrets/admin.env`, and starts the stack. It never overwrites existing
    files and sets ownership to uid 65532 on Linux. The admin socket lives on
    a tmpfs (`/run/litebucket`); `LITEBUCKET_CONFIG` in the image lets
    `docker compose exec litebucket litebucket admin ...` find the config.
48. Admin changes: one transaction per change with its audit record; the key
    snapshot is rebuilt under `admin_lock` before the response, so changes
    apply to the next request and an older refresh can never overwrite a newer
    one. A change leaving no enabled admin key is refused (409). Rotation keeps
    the previous secret for an optional grace period (≤ 30 days); the expiry
    task deletes it afterwards. `admin recover` (offline) creates an admin key
    and, with `--reset-keys`, deletes all keys first. Backups include the
    (encrypted) keys; `restore --master-key-file` verifies them first.
49. Interop tooling (Python venv with Boto3 and AWS CLI v2, generated Rails app)
    lives in git-ignored `.interop/`; nothing is installed system-wide.

## 5. Known limitations and follow-ups

- Not tested: power loss, x86-64, XFS. Durability assumes the device
  honors `fsync`/`F_FULLFSYNC`.
- macOS small-write throughput (~140 PUT/s) is dominated by `F_FULLFSYNC`
  drive-cache flushes; Linux/ext4 reaches ~1400 PUT/s on the same hardware.
- Not implemented (by spec or deferred): UploadPartCopy, ListObjects v1,
  virtual-hosted addressing, versioning, SSE, Object Lock, tagging, online
  backup, HTTP/2, per-credential rate limits, unknown-length uploads.
- The dev machine's disk was ~95% full; the default 5% free-space reserve
  will refuse writes on such a disk by design.
