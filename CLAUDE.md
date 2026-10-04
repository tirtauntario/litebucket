# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project state

`storlite` is a single-host S3-compatible object store (Rust/Axum, embedded SQLite metadata, two-level sharded files). All spec milestones 0–6 are implemented; `docs/implementation-status.md` tracks acceptance evidence and `docs/implementation-report.md` records decisions and assumptions. Keep both current when behavior changes.

## Specification

The spec bundle lives in `docs/` and is the authoritative contract:

- `docs/SPEC.md` — full contract; invariants **INV-01..INV-14** are mandatory.
- `docs/IMPLEMENTATION_PLAN.md` — milestones 0–6 and the acceptance matrix (DEP-01, FS-01, …).
- `docs/AGENTS.md`, `docs/reference-schema.sql`, `docs/examples/*.toml`.

Conventions for applying the bundle to this repo:

- **Name:** the spec's placeholder `compact-s3` is `storlite` everywhere (binary, CLI, logs, metrics, image).
- **Layout:** one Cargo package at the repo root (`Cargo.toml`, `src/`, `migrations/`, `tests/`, `deploy/`). No workspace or subdirectory crate.
- **Paths:** spec references to `SPEC.md`, `IMPLEMENTATION_PLAN.md`, `examples/…` mean `docs/…`. Generated docs (`implementation-status.md`, `compatibility.md`, `operations.md`, `test-evidence.md`, `benchmarks.md`, `architecture-decisions/`) also live in `docs/`.
- **Platform:** dev host is macOS; Linux durability tests are authoritative (run them in Docker, see `docs/test-evidence.md`).

## Commands

```bash
cargo build --release
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings      # also with --features failpoints
cargo test --locked                                      # unit + integration (in-process server)
cargo test --locked --features failpoints                # + tests/crash.rs (subprocess crash/fault matrix)
cargo test --test objects put_03 -- --nocapture          # one test file / filter
scripts/interop.sh [ruby boto3 awscli rails browser]     # real clients; INTEROP_TLS=1 for HTTPS
.interop/venv/bin/python scripts/bench.py                # resource/performance evidence
```

Releases: pushing tag `vX.Y.Z` (must equal `Cargo.toml` version) runs `.github/workflows/release.yml` → static musl/macOS binaries on GitHub Releases + multi-arch image on `ghcr.io/tirtauntario/storlite`. User install docs are `README.md` + `docs/installation.md`; maintainer steps in `docs/releasing.md`.

`.interop/` (git-ignored) holds the Python venv (boto3, AWS CLI v2) and the generated Rails app. Test configs pin a small disk reserve because the production default (5% free) can trip on a full dev disk.

## Architecture

- `src/store.rs` — the running `Store` (config, `DataDir`, `Db`, capacity, key/upload guards, task tracker, halt state) and the durable blob pipeline as typestate: `WriteTicket` → `ReceivedBlob` → `StagedBlob` (fsynced) → `PublishedBlob` (no-clobber rename + dir sync) → commit. Dropping any of them before commit abandons the WRITING row via a supervised task. Publication+commit run in `Store::supervise` so client disconnects can't interrupt them.
- `src/fsutil.rs` — store lock (`flock`), sharded paths (`area/aa/bb/<id>`), exclusive create, `fsync`/`F_FULLFSYNC`, `renameat2(NOREPLACE)`, no-follow opens.
- `src/metadata/` — connection policy (WAL, `synchronous=FULL`, verified), migrations (`migrations/*.sql`, checksummed), all SQL in `queries.rs`, bounded worker threads. Writes go through `Db::write_tx(name, f)`, which group-commits queued transactions with a savepoint each (ADR 0003); `name` labels test failpoints.
- `src/s3/` — protocol boundary: `mod.rs` (handler: limits → parse → resolve op → capability validation → SigV4 → dispatch → errors/CORS/logging/metrics), `auth.rs`, `payload.rs` (aws-chunked/trailer decoder), `integrity.rs`, `capabilities.rs` (allowlists), `xml.rs` (bounded parser/writer), and per-area handlers (`bucket`, `object`, `listing`, `multipart`, `cors`).
- `src/sigv4.rs` — SigV4 primitives, tested against AWS-published vectors.
- `src/maintenance.rs` (GC, multipart/receipt expiry, checkpoints, gauges), `src/server.rs` (hyper http1 + optional rustls, management listener, graceful shutdown), `src/doctor.rs`/`src/backup.rs` (offline commands), `src/cli.rs`.
- Lock order: upload guard → per-key guard → metadata transaction; never more than one key guard; never await a guard inside a transaction.
- No `unsafe`, no `unwrap` on network/fs/db results in production paths.

## Writing axum/hyper code

Axum is used only as the router shell (one fallback handler + management routes); connection handling uses `hyper::server::conn::http1` directly for header/timeout limits. Before changing either, check docs.rs for the pinned versions (axum 0.8, hyper 1.x, hyper-util 0.1) rather than relying on memory.
