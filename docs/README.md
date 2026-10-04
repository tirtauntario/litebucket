# Codex specification bundle — single-host S3 object storage

This bundle describes the application to build. **It does not contain an implemented storage server.** The working name `compact-s3` is a placeholder. Fixed requirements are Rust/Axum, embedded SQLite metadata, Rails-style two-level sharding, and one host/process with no external database services.

## Files

| File | Purpose |
|---|---|
| `SPEC.md` | Authoritative implementation contract: scope, API behavior, data model, durability, multipart, security, operations, and release gates |
| `AGENTS.md` | Short repository instructions directing Codex to the full specification and preserving core constraints |
| `IMPLEMENTATION_PLAN.md` | Seven incremental milestones and a concrete acceptance matrix |
| `CODEX_PROMPT.md` | Starter and continuation prompts |
| `reference-schema.sql` | Executable reference SQLite schema; application-level invariants are still required |
| `examples/config.example.toml` | Proposed non-secret configuration with documented units and defaults |
| `examples/credentials.example.toml` | Disabled placeholder credentials and example scoped grants; not usable access keys |
| `VALIDATION.md` | Checks performed on this documentation bundle, distinct from future application tests |

`SPEC.md` embeds the SQL and configuration examples for convenient standalone reading. The separate files are identical authoring inputs. Prefer keeping the whole bundle together when implementing; the plan and AGENTS instructions reference the main document.

## Use in a repository

Place the files at the target repository root and use the prompt in `CODEX_PROMPT.md`. In an existing repository, merge instructions carefully and preserve unrelated code and existing AGENTS requirements. Do not copy placeholder credentials into a running deployment as though they were valid secrets.

Begin with the protocol/dependency feasibility milestone, then implement a working vertical slice and the remaining gates. Do not interpret an early PUT/GET demonstration as complete S3 compatibility.

## Decisions worth noticing

The production baseline is Linux and a supported local filesystem. Defaults include path-style access, one configured region, private storage, a 100 GiB assembled-object cap, offline backup, and no versioning, application-level encryption API, or retention guarantees. These are explicit engineering defaults/limits rather than measured capacity claims. The server must reject unsupported protection requests.

The full v1 includes multipart uploads, current-client checksum/body modes, scoped credentials, CORS, safe publication/recovery, and bounded cleanup. Their complexity is intentionally visible rather than hidden behind an overly small happy-path specification.

Primary technical references and their review date appear in `SPEC.md`. Dependency and client versions still need to be resolved, pinned, and tested during implementation.
