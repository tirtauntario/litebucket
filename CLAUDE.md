# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project state

`storlite` is a single-host S3-compatible object store (Rust/Axum, embedded SQLite metadata, two-level sharded files). The code is still a scaffold — `src/main.rs` serves `GET /` → `"Hello, World!"` — and will be replaced as milestones land. Update this file as real architecture lands.

## Specification

The spec bundle lives in `docs/` and is the authoritative contract:

- `docs/SPEC.md` — full contract; invariants **INV-01..INV-14** are mandatory.
- `docs/IMPLEMENTATION_PLAN.md` — milestones 0–6 and the acceptance matrix (DEP-01, FS-01, …).
- `docs/AGENTS.md`, `docs/reference-schema.sql`, `docs/examples/*.toml`.

Read the relevant spec sections before each milestone. Conventions for applying the bundle to this repo:

- **Name:** the spec's placeholder `compact-s3` is `storlite` everywhere (binary, CLI, logs, metrics, image).
- **Layout:** one Cargo package at the repo root (`Cargo.toml`, `src/`, `migrations/`, `tests/`, `deploy/`). No workspace or subdirectory crate.
- **Paths:** spec references to `SPEC.md`, `IMPLEMENTATION_PLAN.md`, `examples/…` mean `docs/…`. Generated docs (`docs/implementation-status.md`, `docs/compatibility.md`, `docs/operations.md`, `docs/architecture-decisions/`) also go in `docs/`.
- **Platform:** dev host is macOS; Linux durability tests are authoritative.

## Commands

```bash
cargo run                    # start server on 0.0.0.0:3000
cargo build --release
cargo check                  # fast type-check
cargo clippy --all-targets   # lint
cargo fmt                    # format
cargo test                   # all tests
cargo test <name>            # tests whose path contains <name>
cargo test <name> -- --exact --nocapture   # one exact test, show stdout
```

Required checks once a lockfile is committed (from `docs/AGENTS.md`):

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

## Architecture

- **Stack:** `axum` 0.8 on `tokio` (`features = ["full"]`), runtime started via `#[tokio::main]`.
- **Routing:** a single `axum::Router` built in `main`, bound with `tokio::net::TcpListener` and run with `axum::serve`. The bind address (`0.0.0.0:3000`) is hard-coded.
- **Errors:** startup failures currently `unwrap()`; there is no error type or config layer yet.

## Writing axum code

Before implementing a feature, consult the official axum docs (https://docs.rs/axum/latest/axum/) and follow the idiomatic pattern they show, rather than relying on memory, since axum's API changes between minor versions (this repo is on 0.8). The relevant sections are routing (`Router`, nesting, fallbacks), extractors (`extract::*`, `FromRequest`/`FromRequestParts`), responses (`IntoResponse`), error handling, shared state (`State`), and middleware (`tower` layers, `middleware::from_fn`).
