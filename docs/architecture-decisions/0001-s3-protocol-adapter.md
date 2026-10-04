# ADR 0001 — S3 protocol adapter: in-house boundary instead of `s3s`

**Status:** accepted (milestone 0, 2026-10-04)

## Context

SPEC §2.2–2.3 prefers `s3s` behind our own boundary, *conditionally* after a
protocol spike, and permits replacing the adapter if it cannot meet mandatory
behavior. Requirements that weigh on the choice: raw URI/frame preservation,
fail-closed SigV4 (header + presigned), all five body modes including signed
and unsigned checksum trailers, bounded streaming without buffering, a
centralized capability validator that rejects unknown semantic headers/query
parameters *before* dispatch, and per-operation authorization that cannot be
bypassed.

## Evaluation of `s3s` 0.17.0 (source review)

Reviewed `s3s-0.17.0` and `s3s-sigv4-0.17.0` from crates.io:

| Question | Finding |
|---|---|
| Anonymous fallback | Unsigned requests reach `S3Access::check` with `credentials() == None`; access checks are skipped entirely when no auth provider is configured. Safe only if both are wired correctly. |
| SigV2 | Disabled by default (fail-closed). |
| Streaming modes | Signed chunks, signed trailer, and unsigned trailer are implemented in `aws_chunked_stream.rs`; trailing checksum values are exposed to the implementor for validation. |
| Checksum validation | Values are surfaced; verification against bytes is left to the backend. |
| Unknown operations | Generated router returns `NotImplemented` for trait methods left at default. |
| Unknown headers | Parsed into ~100 generated DTOs; unsupported semantic fields (SSE, Object Lock, tagging, …) arrive as optional DTO fields that every handler must inspect. |
| Project status | Self-described experimental; large generated surface (all S3 operations). |

`s3s` would likely work, but every mandatory guard (capability rejection,
checksum verification, trailer cross-checks, authorization order, bounded
XML) would still be ours, layered over a large generated surface whose
fall-through behavior must be audited per DTO field and re-audited on every
upgrade.

## Decision

Implement a focused in-house protocol layer in `src/s3/` and `src/sigv4.rs`:
path-style routing on the raw URI, a capability validator over raw headers and
query parameters, SigV4 header/presigned/streaming verification, a strict
bounded XML reader/writer, and S3 error serialization. Only the ~22 required
operations exist; anything else is rejected before any handler runs.

Consequences:

- One hashing pipeline computes MD5 (ETag), internal SHA-256, the payload hash,
  and requested S3 checksums in a single pass while streaming to disk.
- Trailers are parsed by our `aws-chunked` decoder from data frames; HTTP-level
  trailer frames are rejected rather than silently dropped (SPEC §10.4, R36).
- Protocol correctness evidence comes from AWS-published SigV4 vectors and real
  SDK suites (Ruby, Boto3, AWS CLI v2) instead of from an adapter's own tests.
- Revisit if the required operation set grows substantially.

## Resolved dependency versions (Cargo.lock)

| Crate | Version | Note |
|---|---|---|
| rustc | 1.97.1 | `rust-toolchain.toml` |
| axum / hyper / hyper-util / tokio | 0.8.9 / 1.11.1 / 0.1.21 / 1.53.2 | |
| rusqlite / libsqlite3-sys | 0.40.2 / 0.38.2 (`bundled`) | |
| **SQLite runtime** | **3.53.2**, source id `2026-06-03 19:12:13 d6e03d8c…1a24` | ≥ 3.51.3 (WAL-reset fix); enforced at startup by `metadata::check_sqlite_runtime` |
| rustix | 1.1.5 | `renameat2(RENAME_NOREPLACE)` on Linux, `renameatx_np(RENAME_EXCL)` on macOS |
| rustls / tokio-rustls | 0.23.45 / 0.26.6 (`ring`) | |
| sha2 / sha1 / md-5 / hmac | 0.10.x / 0.12.1 | the RustCrypto 0.10 line is used for its stable API; 0.11 exists |
| crc / crc32c / crc32fast | 3.4.0 / 0.6.8 / 1.5.2 | CRC-64/NVME from `crc-catalog` |
| toml / clap / tracing | 1.1.6 / 4.6.7 / 0.1.44 | |

No asynchronous SQLite wrapper is used: workers own `rusqlite` connections on
dedicated threads behind bounded `tokio::sync::mpsc` queues, so no wrapper can
pull in a different SQLite build.
