# Implementation status

Contract: `docs/SPEC.md` 1.0 and `docs/IMPLEMENTATION_PLAN.md`. A status of
**pass** means an executed test or measurement supports it (commands and
environments are in `docs/test-evidence.md`). Nothing here was marked passed
without running it.

## Milestones

| Milestone | Status | Notes |
|---|---|---|
| 0 — protocol/dependency feasibility | done | Dependencies pinned (`Cargo.lock`, `rust-toolchain.toml` 1.97.1); runtime SQLite 3.53.2 verified at startup; `s3s` 0.17 evaluated by source review and replaced by an in-house protocol boundary (ADR 0001). |
| 1 — executable and storage foundation | done | CLI, config, credentials, `init`, store lock, migrations, bounded DB workers, IDs/keys, sharded paths, capacity, durable file primitives, recovery. |
| 2 — authenticated object vertical slice | done | Buckets, Put/Get/Head/Delete, presign, conditions, ranges, capability validator. |
| 3 — listing, copy, batch deletion, quotas | done | Indexed ListObjectsV2 with HMAC tokens (property-tested vs. a reference model), CopyObject, DeleteObjects, logical quotas. |
| 4 — multipart | done | Six operations, state machine, receipts, expiry, composite/full-object checksums, partNumber reads. |
| 5 — protocol coverage and real clients | done | All body modes and checksum algorithms; CORS; AWS CLI v2, Boto3, Ruby SDK, Rails Active Storage, Chrome — over HTTP and HTTPS. |
| 6 — operations, stress, backup, release | done | Recovery, GC, expiry, readiness/metrics, graceful shutdown, doctor/check/gc/quota, offline backup/restore, crash/fault matrix, container image, benchmarks. |

## Acceptance matrix

| ID | Status | Evidence (test or artifact) |
|---|---|---|
| DEP-01 | pass | ADR 0001 version table; `metadata::tests::bundled_sqlite_is_patched`; startup log records `sqlite_version`/`sqlite_source_id`. |
| DEP-02 | pass | `protocol::dep_02_anonymous_and_malformed_auth_fail_closed`; `serve` refuses to start with zero enabled credentials (`cli::serve`). |
| FS-01 | pass | `objects::fs_01_object_files_are_two_level_sharded`; `fsutil::tests::paths_are_two_level_sharded`; multipart/staging layout in `multipart::mpu_08_*`. |
| FS-02 | pass | `ids::tests::rejects_malformed_ids`, `request::tests::path_style_split_preserves_key_bytes`, `objects::key_bytes_are_preserved_exactly`, `multipart::mpu_03_*` (`../../x` upload ID). |
| FS-03 | pass | `fsutil::tests::exclusive_create_and_noclobber_publish`; `crash::fs_03_injected_storage_id_collisions_preserve_existing_files` (tracked and untracked collisions via forced IDs). |
| FS-04 | pass | `crash::fs_04_injected_io_failures_never_acknowledge` (file `fsync` EIO, directory sync EIO, write ENOSPC, uncertain commit). |
| DB-01 | pass | `metadata::tests::durability_settings_apply_to_every_connection`, `metadata::tests::bounded_queue_rejects_when_full`. |
| DB-02 | pass | `operations::db_02_slow_upload_holds_no_metadata_transaction`. |
| DB-03 | pass | `queries::tests::byte_ordering_matches_reference_model`; `listing::tests::paging_matches_reference_model` (proptest, 400 cases). |
| DB-04 | pass | `migrations::tests::{newer_schema_is_rejected, tampered_migration_checksum_is_rejected, migrations_are_repeatable}`; `operations::db_04_missing_or_newer_metadata_never_opens_an_empty_store`. |
| PUT-01 | pass | `objects::put_01_bytes_and_metadata_survive_restart`. |
| PUT-02 | pass | `objects::put_02_invalid_checksum_or_signature_never_replaces`, `objects::put_02_oversized_and_short_bodies_preserve_old_object`, `operations::http_03_idle_body_times_out_and_preserves_object`. |
| PUT-03 | pass | `objects::put_03_concurrent_create_has_one_winner`. |
| PUT-04 | pass | `objects::put_03_04_conditional_writes`; `queries::tests::conditions_and_quota_are_checked_at_commit`. |
| GET-01 | pass | `objects::get_01_reads_one_generation_during_overwrites`. |
| GET-02 | pass | `objects::get_02_ranges_and_conditional_reads`. |
| GET-03 | pass | `operations::cap_01_get_03_overload_and_abandoned_downloads_are_bounded`. |
| LIST-01 | pass | `protocol::list_01_02_03_listing_contract`; listing proptest. |
| LIST-02 | pass | `protocol::list_01_02_03_listing_contract`, `protocol::list_02_recreated_bucket_rejects_old_tokens`, `protocol::auth_02_04_*`. |
| LIST-03 | pass | Index range scans with prefix-successor jumps (`listing::build_page`); page latency flat from 2k to 30k keys (`docs/benchmarks.md`). |
| COPY-01 | pass | `objects::copy_01_02_copy_semantics_and_isolation`. |
| COPY-02 | pass | Same test (source overwrite after copy, separate files, source digest re-verified). |
| DEL-01 | pass | `objects::del_01_delete_is_idempotent_and_hides_immediately`. |
| DEL-02 | pass | `objects::del_02_delete_objects_validates_then_reports_per_key`. |
| MPU-01 | pass | `multipart::mpu_01_parallel_parts_and_replacement`. |
| MPU-02 | pass | `multipart::mpu_02_parts_survive_restart_and_listings_paginate`. |
| MPU-03 | pass | `multipart::mpu_03_invalid_manifests_cannot_complete`. |
| MPU-04 | pass | `multipart::mpu_04_terminal_uploads_cannot_be_revived`; `crash::ops_02_crash_matrix_completion`. |
| MPU-05 | pass | `multipart::mpu_05_checksum_modes`; Boto3 composite SHA256 interop. |
| MPU-06 | pass | `multipart::mpu_06_completion_conditions_and_quota_at_commit`. |
| MPU-07 | pass | `multipart::mpu_07_completion_retry_uses_receipt_without_resurrection`. |
| MPU-08 | pass | `multipart::mpu_08_expiry_and_abort_release_parts`. |
| AUTH-01 | pass | `sigv4` AWS vectors; `protocol::auth_01_signature_checks`, `protocol::auth_01_presigned_get_put_head`; all SDK suites. |
| AUTH-02 | pass | `protocol::auth_01_*`, `protocol::auth_02_04_prefix_scoped_grants_fail_closed`, `protocol::auth_03_*` (revoked key, old presigned URL). |
| AUTH-03 | pass | `protocol::auth_03_credential_reload_is_atomic`. |
| AUTH-04 | pass | `protocol::auth_02_04_*`, `objects::copy_01_02_*`, `objects::del_02_*`. |
| AUTH-05 | pass | `protocol::auth_05_unsupported_protection_features_fail`; `capabilities::tests::*`; Boto3 SSE request → `NotImplemented`. |
| BODY-01 | pass | `objects::body_01_02_04_all_payload_modes_store_exact_bytes`. |
| BODY-02 | pass | Same test plus `payload::tests::aws_signed_trailer_example_decodes_at_any_split` and AWS chunk/trailer signature vectors. |
| BODY-03 | pass | `payload::tests::tampered_chunk_or_trailer_fails`, `undeclared_trailer_is_rejected`; `objects::body_03_*`. |
| BODY-04 | pass | `objects::body_01_02_04_*` (all five algorithms + Content-MD5), `objects::put_02_*`. |
| HTTP-01 | pass | `protocol::http_01_unknown_parameters_and_methods`, `protocol::bucket_operations_and_errors`, HEAD bodyless in `objects::get_02_*`. |
| HTTP-02 | pass | `protocol::http_02_cors_preflight_and_actual_requests`; Chrome suite. |
| HTTP-03 | pass | `protocol::http_03_request_bounds`, `xml::tests::*`, `payload::tests::framing_overhead_is_bounded`, `operations::http_03_idle_body_times_out_and_preserves_object`. |
| CAP-01 | pass | `operations::cap_01_get_03_*`, `capacity::tests::admission_times_out_with_overload`, `metadata::tests::bounded_queue_rejects_when_full`. |
| CAP-02 | pass | `operations::cap_02_concurrent_uploads_cannot_over_admit_temporary_space`; `capacity::tests::*`. |
| CAP-03 | pass | `multipart::cap_03_assembly_accounts_for_output_space`. |
| CAP-04 | pass | `operations::cap_04_counters_through_overwrite_delete_restart`; `queries::tests::overwrite_updates_counters_and_garbage`. |
| OPS-01 | pass | `operations::ops_01_second_owner_is_refused_and_lock_is_kept`; `fsutil::tests::second_owner_is_refused`. |
| OPS-02 | pass | `crash::ops_02_crash_matrix_{new_objects,overwrites,part_replacement,completion,delete_and_gc}` (SIGABRT at every durable boundary). |
| OPS-03 | pass | `crash::fs_04_*` (`commit:object=eio` → reconciled not-committed); recovery tests; ADR 0002. |
| OPS-04 | pass | `operations::ops_04_gc_reclaims_only_tracked_eligible_garbage`, `operations::doctor_reports_consistency_and_untracked_files`, crash GC cases. |
| OPS-05 | pass | `operations::ops_05_backup_and_restore_round_trip` (objects, metadata, multipart continuation, corruption and incomplete-backup refusal). |
| OPS-06 | pass | `operations::ops_06_missing_or_corrupt_files_are_integrity_failures`; `check --full` in crash tests. |
| OPS-07 | pass | `crash::ops_07_logs_exclude_secrets_and_keys`; `protocol::management_endpoints` (no key/credential labels). |
| SDK-01 | pass | AWS CLI 2.37.9: 12/12, HTTP and HTTPS. |
| SDK-02 | pass | Boto3 1.43.108: 7/7; Ruby aws-sdk-s3 1.229.0: 9 tests / 31 assertions; HTTP and HTTPS. |
| SDK-03 | pass | Rails 8.1.3.1 Active Storage: 12/12, HTTP and HTTPS. |
| SDK-04 | pass | Chrome 154 headless: 9/9, HTTP and HTTPS. |
| PERF-01 | pass | `scripts/bench.py`: 1 GiB single-stream upload/download with peak server RSS < 8 MiB (`docs/benchmarks.md`). |

## Invariants

| Invariant | Primary evidence |
|---|---|
| INV-01 durable acknowledgment | FS-04, OPS-02 (`after_commit:*` vs `before_commit:*` cases), group commit delivers results only after the shared commit (ADR 0003). |
| INV-02 complete immutable referenced file | GET-01, OPS-02, MPU-04. |
| INV-03 never edit committed files | FS-03, PUT-02, COPY-02. |
| INV-04 internal IDs only in paths | FS-01/02. |
| INV-05 all areas sharded | FS-01. |
| INV-06 no object bytes in SQLite | Schema (`migrations/`); metadata DB 12 MB after 30k objects + 2 GiB data (`docs/benchmarks.md`). |
| INV-07 short metadata transactions | DB-02; bodies are SQL only (ADR 0003). |
| INV-08 authorization everywhere | AUTH-01..05, LIST-02, COPY-01, DEL-02. |
| INV-09 invalid bodies never publish | PUT-02, BODY-02..04, MPU-03. |
| INV-10 collector protects live/active files | GET-03, MPU-08, OPS-04, crash GC cases. |
| INV-11 bounded resources | CAP-01..03, HTTP-03, PERF-01. |
| INV-12 one owner | OPS-01. |
| INV-13 reconcile unknown outcomes | OPS-03. |
| INV-14 never fake protection | AUTH-05. |

## Known gaps and limits

- Power-loss durability is not tested; only process crashes (SIGABRT) and
  injected I/O failures. Durability depends on the device honoring `fsync`.
- Linux validation ran on ext4 inside Docker Desktop's VM (aarch64); x86-64
  and XFS were not run.
- Throughput numbers are from a developer laptop; see `docs/benchmarks.md`
  for conditions. They are not capacity claims.
- `UploadPartCopy`, ListObjects v1, virtual-hosted addressing, versioning,
  encryption APIs, and online backup remain out of scope.
