# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project state

`storlite` is a freshly scaffolded Rust (edition 2024) HTTP server — currently a single file, `src/main.rs`, serving `GET /` → `"Hello, World!"`. No modules, tests, CI, or README exist yet. Update this file as real architecture lands.

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

## Architecture

- **Stack:** `axum` 0.8 on `tokio` (`features = ["full"]`), runtime started via `#[tokio::main]`.
- **Routing:** a single `axum::Router` built in `main`, bound with `tokio::net::TcpListener` and run with `axum::serve`. The bind address (`0.0.0.0:3000`) is hard-coded.
- **Errors:** startup failures currently `unwrap()`; there is no error type or config layer yet.

## Writing axum code

Before implementing a feature, consult the official axum docs (https://docs.rs/axum/latest/axum/) and follow the idiomatic pattern they show, rather than relying on memory, since axum's API changes between minor versions (this repo is on 0.8). The relevant sections are routing (`Router`, nesting, fallbacks), extractors (`extract::*`, `FromRequest`/`FromRequestParts`), responses (`IntoResponse`), error handling, shared state (`State`), and middleware (`tower` layers, `middleware::from_fn`).
