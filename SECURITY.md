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
- **Access-key secrets:** keys are generated from the OS CSPRNG (256-bit
  secrets; ids are `SL` + 18 random base32 characters) and stored in the
  metadata database. SigV4 needs the shared secret, so it cannot be hashed:
  by default each secret is encrypted with AES-256-GCM under a 32-byte master
  key kept in a file outside the data directory (owner-only permissions
  enforced), bound to the store id and access key id so records cannot be
  swapped. Secrets are held in memory while serving. A secret is returned only
  once, when a key is created or rotated; listings, the audit log, logs and
  metrics never contain it. `protection = "plaintext"` is available and
  stores secrets unencrypted.
- **Administration:** keys, grants and buckets are changed only through a
  local Unix socket (mode 0600) that serves peers whose effective uid is the
  server's or root, checked with the kernel's peer credentials. There is no
  network admin endpoint. Every change is recorded in an audit table with the
  caller's uid. A change that would leave no enabled admin key is refused.
- **Logs and metrics** never contain secrets, signatures, presigned URLs,
  request bodies, custom metadata, or (by default) object keys.

## What it does not protect

- Data at rest is stored in plaintext; use filesystem/volume encryption if
  required.
- Plaintext HTTP is allowed only on loopback or behind an explicitly
  allowlisted proxy. Use the built-in TLS or a trusted TLS-terminating proxy
  that preserves `Host`, path, query, and body.
- Backups contain object data, metadata (including access keys, encrypted
  unless plaintext protection is configured), and the listing-token key;
  protect and transport them as sensitive data. Keep the master key in a
  different place from the backups.
- Anyone who can read both the master key file and the database, or who can
  run code as the service user, can recover every secret.
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
