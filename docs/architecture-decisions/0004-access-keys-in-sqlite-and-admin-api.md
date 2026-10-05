# ADR 0004 — Access keys in SQLite and a local admin API

**Status:** accepted (supersedes the credentials-file design of SPEC §11)

## Context

The specification keeps access keys in a separate operator-managed TOML file
(mode 0600) that the server reloads on SIGHUP, and excludes it from backups.
That works for a handful of static keys but makes routine work awkward:
creating a key for an application means generating a fragment, merging it by
hand, fixing permissions and signalling the process; in Docker the file is a
read-only secret mount that cannot be edited in place. Operators asked to
create keys, buckets, quotas and CORS through commands that apply immediately,
with the state in one place.

SigV4 verification needs the raw shared secret, so secrets cannot be stored
as one-way hashes.

## Decision

- **Storage.** Migration 0003 adds `credentials` (secret, optional previous
  secret with a deadline, enabled, description, expiry, timestamps),
  `credential_grants` (bucket, literal byte prefix, actions),
  `credential_global_grants`, and `admin_audit`. Grants refer to buckets by
  name, so access can be granted before a bucket exists (as before).
- **Secret protection** (`[secrets] protection`):
  - `encrypted` (default): AES-256-GCM (`ring`, already compiled in through
    rustls) under a 32-byte master key in `master_key_file`, a base64 file
    with owner-only permissions that must live outside `data_dir`. Each
    record has a fresh random 96-bit nonce; the associated data is a version
    tag, the store id and the access key id, so ciphertexts cannot be moved
    between rows or stores. Per-row `secret_scheme` (0 plaintext, 1 AES-GCM
    v1) leaves room for future key versions.
  - `plaintext`: secrets stored as-is, for operators who rely on disk
    encryption and file permissions.
  - At startup every stored secret is converted to the configured mode in one
    transaction, then all keys are decoded into the in-memory snapshot. Any
    undecryptable record (missing or wrong master key, tampering) refuses
    startup without changing anything.
- **Admin API.** A JSON API over HTTP/1.1 on a Unix socket (`[admin] socket`,
  mode 0600). The accept loop reads the peer's credentials and serves only
  the server's own effective uid or root. No network listener exists yet
  (remote administration over TLS with SigV4-signed admin requests is a
  possible later addition).
- **Consistency.** Each mutation is one metadata transaction (`write_tx`
  named `admin`, so the crash tests can abort before/after its commit) that
  also writes the audit record. While holding `Store::admin_lock`, the
  handler then rebuilds the credential snapshot from the database before
  responding, so a newer change can never be overwritten by an older
  refresh. Changes apply to the next request; requests already authorized
  keep their snapshot (the same boundary SIGHUP reload had).
- **Safety rails.** Changes that would leave no enabled, unexpired admin key
  are refused. `litebucket admin recover` (offline, under the store lock)
  creates an admin key directly; `--reset-keys` deletes all keys first for a
  lost master key, and without it recovery first proves the configured key
  opens every existing secret so keys sealed under different master keys are
  never mixed.
- **Rotation.** `rotate` issues a new secret and optionally keeps the old one
  valid for a grace period (≤ 30 days). Authentication tries the current
  secret, then the previous one until its deadline; the expiry task deletes
  expired previous secrets.
- **Bootstrap.** `init` creates the master key file when missing and a first
  admin key, which it prints once or writes to a new 0600 file.
- **Backups** now contain the keys (encrypted unless plaintext mode) but never
  the master key. `restore --master-key-file` verifies every secret decrypts
  before writing anything.

## Consequences

- Deviations from the specification: no credentials file or SIGHUP reload;
  credentials are inside backups; an admin interface exists; per-bucket quota
  is set through the admin API instead of an offline command. Recorded in
  `compatibility.md` and `implementation-report.md`.
- Operators must back up the master key separately from the data and keep it
  out of backups. Losing it loses the keys, not the data.
- A compromise of the service user (which can read both the master key and
  the database) exposes all secrets, as it did with the plaintext credentials
  file. Encryption protects copies of the database: backups, snapshots,
  leaked volumes.
- The admin API adds a privileged local surface, limited by file mode and
  peer-uid checks; there is no rate limiting because only local
  administrators can reach it.
