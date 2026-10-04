# Security model

## What storlite protects

- **Authentication:** every S3 request except a CORS preflight must carry a
  valid AWS SigV4 signature (header or presigned query) from an enabled,
  unexpired credential. There is no anonymous mode and no fallback on parse or
  configuration errors. SigV2, SigV4a, and STS session tokens are refused.
  Signatures are compared in constant time; every `x-amz-*` header must be
  signed.
- **Authorization:** a small allow-only model (bucket + literal byte-prefix
  grants; `read`, `list`, `write`, `delete`, `manage_bucket`; global
  `list_buckets`, `create_bucket`, `admin`). It is checked for every operation
  and every affected key — upload IDs and listing tokens are never
  permissions. Listing tokens are HMAC-SHA256 authenticated and bound to the
  store, bucket ID, options, and principal. Unauthorized requests cannot
  learn whether a key exists.
- **Integrity:** payload hashes, chunk/trailer signatures, Content-MD5, and
  S3 checksums are verified before any object becomes visible; failures never
  replace committed data. Unsupported protection features (encryption,
  retention, Object Lock, versioning, ACL grants, tagging) fail explicitly —
  nothing returns fabricated protection headers.
- **Filesystem:** object keys never become paths; files are addressed only by
  validated random IDs, opened with `O_NOFOLLOW`, created exclusively, and
  published without overwriting. The data directory must be owned by the
  service user and not group/world-writable. Untracked files are never
  deleted automatically.
- **Resource bounds:** header size/count, request-target length, header and
  body-idle timeouts, XML size/depth/element limits (DTDs and entities are
  rejected), aws-chunked framing overhead, transfer/queue concurrency, disk
  and temporary-space reservations.
- **Secrets:** credentials live in a separate operator-managed file (mode
  0600). SigV4 needs the shared secret, so it is held in memory and in that
  file in plaintext — protect the file and host accordingly. Logs and metrics
  never contain secrets, signatures, presigned URLs, request bodies, custom
  metadata, or (by default) object keys.

## What it does not protect

- Data at rest is stored in plaintext; use filesystem/volume encryption if
  required.
- Plaintext HTTP is allowed only on loopback or behind an explicitly
  allowlisted proxy. Use the built-in TLS or a trusted TLS-terminating proxy
  that preserves `Host`, path, query, and body.
- Backups contain object data, metadata, and the listing-token key; protect
  and transport them as sensitive data.
- No replication or high availability; durability depends on the storage
  device honoring `fsync`.

## Supported versions

Security fixes go into the latest release. Upgrade to the newest version
before you report an issue, if you can.

## Reporting a vulnerability

Do not open a public issue. Report it privately through
[GitHub private vulnerability reporting](https://github.com/tirtauntario/storlite/security/advisories/new).
Include:

- the version (`storlite --version`) and platform;
- the relevant configuration, **without secrets**;
- steps to reproduce, and the impact you expect.

You should get an acknowledgement within a few days. Fixes are released with
a GitHub security advisory that credits the reporter, unless you ask not to
be credited.
