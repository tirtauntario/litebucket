# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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
- Scoped credentials (bucket + prefix grants) with live reload on SIGHUP;
  `credentials generate --enable --global-grant` creates a directly usable key.
- Offline `doctor`, `check --full`, `gc`, bucket quotas, verified
  backup/restore; `/livez`, `/readyz`, Prometheus `/metrics`.
- Static Linux (musl) and macOS release binaries, a multi-arch distroless
  image on GHCR, an install script and a systemd unit.

[Unreleased]: https://github.com/tirtauntario/storlite/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/tirtauntario/storlite/releases/tag/v0.1.0
