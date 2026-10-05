# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.2.0] - 2026-10-05

The project is renamed from storlite to litebucket. Stored data, encrypted
access keys and backups from 0.1.0 carry over unchanged; names around them
change.

### Changed

- **Breaking:** the crate, binary and CLI are `litebucket`; the image is
  `ghcr.io/tirtauntario/litebucket`; release archives are
  `litebucket-<tag>-<target>.tar.gz`.
- **Breaking:** environment variables use the `LITEBUCKET_` prefix
  (`LITEBUCKET_CONFIG`, `LITEBUCKET_HTTP_LISTEN`, `LITEBUCKET_MANAGEMENT_LISTEN`,
  `LITEBUCKET_LOG_LEVEL`, `LITEBUCKET_LOG_FORMAT`, and the installer's
  `LITEBUCKET_VERSION`/`LITEBUCKET_INSTALL_DIR`). `STORLITE_*` is no longer read.
- **Breaking:** Prometheus metrics are prefixed `litebucket_` instead of
  `storlite_`.
- The S3 `Server` header and owner `DisplayName` are `litebucket`.
- Packaged defaults use litebucket names: `/etc/litebucket`,
  `/var/lib/litebucket`, `/run/litebucket`, the `litebucket` user and
  `litebucket.service`; the Compose service, volume (`litebucket-data`) and
  secrets are renamed to match.

### Upgrading from 0.1.0

- The data directory, metadata database, master key and backups need no
  conversion. Schema migrations are unchanged, and the backup format stays
  `storlite-backup-v1`.
- systemd: install the new binary as `/usr/local/bin/litebucket`, then either
  keep your existing unit and paths and change only `ExecStart`, or move to
  `litebucket.service` (rename the user, `/etc/storlite`, `/var/lib/storlite`,
  and update `data_dir`, `master_key_file` and `[admin] socket` in the config).
- Docker Compose: keep your existing `compose.yaml` (and its data volume).
  Set `STORLITE_IMAGE=ghcr.io/tirtauntario/litebucket:0.2.0` in `.env`, and
  change the config mount target from `/etc/storlite/config.toml` to
  `/etc/litebucket/config.toml`; the image now reads `LITEBUCKET_CONFIG`.
  Use `docker compose exec storlite litebucket admin ...` for admin commands.
  Switching to the new `compose.yaml` instead creates a new, empty
  `litebucket-data` volume: copy the old volume's contents into it first, and
  update the `/run/secrets/...` and `/run/...` paths in `config.toml`.
- Rename `STORLITE_*` environment variables and `storlite_*` metric names in
  scripts, dashboards and alerts.

## [0.1.0] - 2026-10-04

First public release.

### Added

- S3-compatible API: path-style addressing, SigV4 header and presigned
  authentication, all SDK payload modes including checksum trailers,
  CRC32/CRC32C/CRC64NVME/SHA1/SHA256 and Content-MD5 verification.
- Operations: buckets, PutObject, GetObject/HeadObject (ranges, conditions,
  `partNumber`), DeleteObject(s), CopyObject, ListObjectsV2, multipart
  uploads, bucket CORS.
- Durable write pipeline (fsync, no-clobber publish, directory sync, SQLite
  commit before acknowledgement) with crash recovery at every boundary.
- Access keys stored in the metadata database, managed at runtime with
  `storlite admin` over a local Unix-socket admin API: create (generated ids
  and 256-bit secrets, shown once), list, enable/disable, expiry, delete,
  rotate with a grace period, bucket/prefix grants and global grants. Changes
  apply to the next request and are recorded in an audit log; the last admin
  key cannot be removed.
- Secrets encrypted at rest with AES-256-GCM under a master key file
  (default), or stored plaintext (`[secrets] protection`); switching modes
  converts stored secrets at startup.
- `init` creates the master key and a first admin key; `admin recover`
  (offline) restores admin access, with `--reset-keys` for a lost master key;
  `master-key generate`.
- Buckets, quotas and CORS manageable through the admin API as well as S3.
- `config template [--docker]` prints a commented configuration file with
  every setting and its default.
- Offline `doctor`, `check --full`, `gc`, verified backup/restore (access
  keys included, encrypted; restore checks them with `--master-key-file`);
  `/livez`, `/readyz`, Prometheus `/metrics`.
- Static Linux (musl) and macOS release binaries, a multi-arch distroless
  image on GHCR, an install script, a one-step Docker Compose setup script
  (`deploy/setup.sh`) and a systemd unit.

[Unreleased]: https://github.com/tirtauntario/litebucket/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/tirtauntario/litebucket/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tirtauntario/litebucket/releases/tag/v0.1.0
