# Installing and running storlite

storlite is one binary. It needs a config file, a master key file (which
encrypts the access keys stored in the database), and a data directory on a
local disk. Access keys and buckets are managed at runtime with
`storlite admin`. This guide covers:

1. [Supported platforms](#supported-platforms)
2. [Installing the binary](#installing-the-binary)
3. [Running as a systemd service](#running-as-a-systemd-service) (standalone production)
4. [Docker](#docker)
5. [Managing access keys and buckets](#managing-access-keys-and-buckets)
6. [TLS and reverse proxies](#tls-and-reverse-proxies)
7. [Upgrading](#upgrading)
8. [Uninstalling](#uninstalling)

For configuration keys, health checks, failure handling and backup/restore,
see [operations.md](operations.md).

## Supported platforms

| Platform | Status |
|---|---|
| Linux x86_64 / aarch64, local ext4 or XFS | Production. Release binaries are static (musl), so they run on any distribution, including Alpine. |
| Docker on Linux (amd64 / arm64) | Production. The image is distroless and runs as non-root. |
| macOS (Apple Silicon / Intel) | Development and testing. Durability uses `F_FULLFSYNC`. |
| NFS, SMB, or any shared or multi-host volume | Not supported. |
| Windows | Not supported (use Docker or WSL2 with a Linux filesystem). |

Run one storlite process per data directory. A lock file stops a second
process from opening the same data directory.

## Installing the binary

### Install script

```sh
curl -fsSL https://raw.githubusercontent.com/tirtauntario/storlite/main/install.sh | sh
```

The script detects your OS and CPU, downloads the matching release archive,
verifies its SHA-256 checksum and installs `storlite` to `/usr/local/bin`
(using `sudo` if needed). Options:

```sh
# a specific version, into ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/tirtauntario/storlite/main/install.sh \
  | STORLITE_VERSION=v0.1.0 STORLITE_INSTALL_DIR="$HOME/.local/bin" sh
```

### Manual download

Each [release](https://github.com/tirtauntario/storlite/releases) has these
archives:

| Archive | Platform |
|---|---|
| `storlite-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64 |
| `storlite-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz` | Linux arm64 |
| `storlite-vX.Y.Z-aarch64-apple-darwin.tar.gz` | macOS Apple Silicon |
| `storlite-vX.Y.Z-x86_64-apple-darwin.tar.gz` | macOS Intel |

Each archive contains the binary, the example config file, the systemd unit,
the license and the changelog.

```sh
v=v0.1.0; t=x86_64-unknown-linux-musl
curl -fsSLO "https://github.com/tirtauntario/storlite/releases/download/$v/storlite-$v-$t.tar.gz"
curl -fsSLO "https://github.com/tirtauntario/storlite/releases/download/$v/storlite-$v-$t.tar.gz.sha256"
sha256sum -c "storlite-$v-$t.tar.gz.sha256"      # macOS: shasum -a 256 -c ...
tar -xzf "storlite-$v-$t.tar.gz"
sudo install -m 0755 "storlite-$v-$t/storlite" /usr/local/bin/storlite
storlite --version
```

On macOS, a binary downloaded with a browser is quarantined by Gatekeeper.
Clear the flag with `xattr -d com.apple.quarantine storlite`, or use the
install script, which downloads with `curl`.

### From source

Requires Rust 1.97 or newer and a C compiler (SQLite is compiled in).

```sh
# install the latest main branch into ~/.cargo/bin
cargo install --locked --git https://github.com/tirtauntario/storlite
# or a tagged version
cargo install --locked --git https://github.com/tirtauntario/storlite --tag v0.1.0

# or from a clone
git clone https://github.com/tirtauntario/storlite && cd storlite
cargo build --release --locked        # binary at target/release/storlite
```

## Running as a systemd service

This sets up storlite on a Linux host with this layout:

| Path | Owner / mode | Contents |
|---|---|---|
| `/usr/local/bin/storlite` | root, 0755 | binary |
| `/etc/storlite/config.toml` | root, 0644 | configuration |
| `/etc/storlite/master.key` | storlite, 0400 | encrypts the access keys in the database; back it up separately |
| `/etc/storlite/tls.crt`, `tls.key` | storlite, 0644 / 0600 | TLS certificate and key |
| `/var/lib/storlite/data` | storlite, 0700 | object store and metadata (including access keys) |
| `/run/storlite/admin.sock` | storlite, 0600 | admin API socket (created by systemd's `RuntimeDirectory`) |

**1. Create the service user and directories.**

```sh
sudo useradd --system --home-dir /var/lib/storlite --shell /usr/sbin/nologin storlite
sudo install -d -o root -g storlite -m 0750 /etc/storlite
sudo install -d -o storlite -g storlite -m 0750 /var/lib/storlite
```

**2. Write the configuration.** `storlite config template` prints a
commented file with every setting and its default
([`examples/config.example.toml`](examples/config.example.toml)). Generate it
and edit it, or write a minimal production file:

```sh
storlite config template | sudo tee /etc/storlite/config.toml >/dev/null
```

```toml
# /etc/storlite/config.toml
data_dir = "/var/lib/storlite/data"
region = "us-east-1"

[secrets]
master_key_file = "/etc/storlite/master.key"

[admin]
socket = "/run/storlite/admin.sock"

[http]
listen = "0.0.0.0:9000"
tls_certificate_file = "/etc/storlite/tls.crt"
tls_private_key_file = "/etc/storlite/tls.key"

[management]
listen = "127.0.0.1:9001"     # health and metrics; keep it private
```

storlite refuses plaintext HTTP on non-loopback addresses. Either configure
TLS as shown, or see [TLS and reverse proxies](#tls-and-reverse-proxies).

**3. Create the master key** and give it to the service user:

```sh
sudo storlite master-key generate --output /etc/storlite/master.key
sudo chown storlite:storlite /etc/storlite/master.key && sudo chmod 0400 /etc/storlite/master.key
```

Copy the master key to your secret store now. Without it, the access keys in
the database (and in every backup) cannot be used. You can always create new
keys offline with `storlite admin recover --reset-keys`; objects are never
affected.

**4. Install the TLS certificate and key** (see [TLS](#tls-and-reverse-proxies)),
then check the configuration:

```sh
sudo chown storlite:storlite /etc/storlite/tls.key && sudo chmod 0600 /etc/storlite/tls.key
sudo -u storlite storlite config check --config /etc/storlite/config.toml
```

**5. Initialize the store** as the service user, so that the data directory
has the right owner. This prints the first admin access key once; store it
in your password manager:

```sh
sudo -u storlite storlite init --config /etc/storlite/config.toml
```

**6. Install and start the unit.** The unit file is
[`deploy/storlite.service`](../deploy/storlite.service) (also in the release
archive).

```sh
sudo install -m 0644 storlite.service /etc/systemd/system/storlite.service
sudo systemctl daemon-reload
sudo systemctl enable --now storlite
storlite healthcheck                    # exit 0 when ready
journalctl -u storlite -f               # JSON logs
```

Day-to-day:

| Task | Command |
|---|---|
| Manage access keys and buckets | `sudo storlite admin --config /etc/storlite/config.toml ...` ([details](#managing-access-keys-and-buckets)) |
| Apply config or certificate changes | `sudo systemctl restart storlite` |
| Offline check, gc, backup | `sudo systemctl stop storlite`, then `sudo -u storlite storlite <command> --config /etc/storlite/config.toml` |

Tip: `alias storlite-admin='sudo storlite admin --config /etc/storlite/config.toml'`.

To listen on a port below 1024 (for example 443), add
`AmbientCapabilities=CAP_NET_BIND_SERVICE` and
`CapabilityBoundingSet=CAP_NET_BIND_SERVICE` to the unit with
`systemctl edit storlite`.

## Docker

Images are published to GitHub Container Registry for `linux/amd64` and
`linux/arm64`:

| Tag | Meaning |
|---|---|
| `ghcr.io/tirtauntario/storlite:latest` | Latest stable release |
| `ghcr.io/tirtauntario/storlite:X.Y.Z` | Exact version (recommended for production) |
| `ghcr.io/tirtauntario/storlite:X.Y` | Latest patch of a minor version |

The image is based on `gcr.io/distroless/cc-debian13:nonroot`. It has no
shell and runs as uid/gid `65532`. Its only writable path is the `/data`
volume, and the entrypoint is `storlite`, so any subcommand can be run with
`docker run ... ghcr.io/tirtauntario/storlite <subcommand>`. A health check
probes `/readyz` on the internal management listener.

### Docker Compose: one-step setup

```sh
mkdir storlite && cd storlite
curl -fsSL https://raw.githubusercontent.com/tirtauntario/storlite/main/deploy/setup.sh | sh
```

[`deploy/setup.sh`](../deploy/setup.sh) does the following, never overwriting a
file that already exists:

1. Downloads `compose.yaml`.
2. Writes `.env` with the image pinned to the exact version it pulled (for
   example `ghcr.io/tirtauntario/storlite:0.1.0`) and the host port.
3. Writes `config.toml` from `storlite config template --docker`. Every
   setting is listed, commented, with its default.
4. Creates `secrets/master.key`, which encrypts the access keys stored in the
   database.
5. Creates a self-signed certificate in `secrets/tls.crt` and `secrets/tls.key`,
   unless you put your own certificate there first.
6. Sets file modes, and on Linux gives the secrets to the container user
   (uid 65532, using `sudo`).
7. Validates `config.toml` and initializes the data volume. `init` creates
   the first admin access key, which the script saves to `secrets/admin.env`
   (mode 0600, readable by you).
8. Starts the container, waits until it is healthy, and prints test and
   admin commands.

Options are environment variables, set on the `sh` side of the pipe:

| Variable | Default | Purpose |
|---|---|---|
| `STORLITE_PORT` | `9000` | Host port |
| `STORLITE_HOSTNAMES` | none | Extra certificate names/IPs, comma-separated (`storage.example.com,10.0.0.5`) |
| `STORLITE_IMAGE` | `ghcr.io/tirtauntario/storlite:latest` | Image to use (pinned to its version in `.env`) |
| `STORLITE_START` | `1` | `0` creates the files only, so you can edit `config.toml` before the first start |

```sh
curl -fsSL https://raw.githubusercontent.com/tirtauntario/storlite/main/deploy/setup.sh \
  | STORLITE_PORT=443 STORLITE_HOSTNAMES=storage.example.com STORLITE_START=0 sh
```

The script is safe to re-run: it fills in whatever is missing and reports an
already initialized volume. Back up `secrets/master.key` separately: backups
of the data volume contain the access keys only in encrypted form.

### Editing the configuration

All server settings live in `config.toml` next to `compose.yaml`. It is
mounted read-only into the container. Every available setting is in the file
as a commented `# key = default` line. Remove the `# ` and change the value:

```toml
[limits]
max_object_bytes = 10737418240        # was: # max_object_bytes = 107374182400
```

Then validate and apply:

```sh
docker compose run --rm storlite config check
docker compose restart storlite
```

| File | What it controls | Apply with |
|---|---|---|
| `config.toml` | Server settings (limits, timeouts, logging, TLS file paths, proxy mode) | `docker compose restart storlite` |
| `.env` | `STORLITE_IMAGE` (version) and `STORLITE_PORT` | `docker compose up -d` |
| `secrets/tls.crt`, `secrets/tls.key` | TLS certificate chain and key | `docker compose restart storlite` |
| Access keys, grants, buckets, quotas, CORS | Stored in the database | `docker compose exec storlite storlite admin ...` (applies immediately; [details](#managing-access-keys-and-buckets)) |

On Linux the files in `secrets/` belong to uid 65532, so edit them with `sudo`
and keep them mode 0400. To get a fresh copy of the commented template, for
example after an upgrade adds settings, run
`docker run --rm ghcr.io/tirtauntario/storlite:<version> config template --docker`.

Do not change `data_dir`, `master_key_file`, the admin `socket` or the
`tls_*` paths in `config.toml`. They are paths inside the container, and
`compose.yaml` mounts the files there. `region` is fixed once the store is
initialized.

### Docker Compose: manual setup

These are the same steps the script runs, if you prefer to do them yourself:

```sh
mkdir storlite && cd storlite
curl -fsSL -O https://raw.githubusercontent.com/tirtauntario/storlite/main/deploy/compose.yaml
image=ghcr.io/tirtauntario/storlite:latest

# configuration
docker run --rm "$image" config template --docker > config.toml

# master key (written with mode 0600, owned by you)
mkdir -m 0700 secrets
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD/secrets:/out" "$image" \
  master-key generate --output /out/master.key

# TLS: copy a real certificate chain and key to secrets/tls.crt and
# secrets/tls.key, or create a self-signed pair:
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 365 \
  -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
  -keyout secrets/tls.key -out secrets/tls.crt

# Compose mounts secrets with their host owner and mode. storlite refuses a
# master key file that group or others can read.
chmod 0400 secrets/*
sudo chown 65532:65532 secrets/*     # Linux only; Docker Desktop maps access itself

docker compose run --rm storlite init   # prints the first admin key once; save it
docker compose up -d
docker compose ps                    # STATUS shows (healthy) when ready
```

Test it:

```sh
export AWS_ACCESS_KEY_ID=<access_key_id from init>
export AWS_SECRET_ACCESS_KEY=<secret_access_key from init>
export AWS_DEFAULT_REGION=us-east-1
aws --endpoint-url https://localhost:9000 --ca-bundle secrets/tls.crt s3 mb s3://documents
aws --endpoint-url https://localhost:9000 --ca-bundle secrets/tls.crt s3 ls
```

(`--ca-bundle` is only needed for a self-signed certificate.)

The compose file also sets a read-only root filesystem, drops all
capabilities, sets `no-new-privileges`, and sets a 40 s stop grace period so
in-flight writes drain. Data lives in the named volume `storlite-data`.

### Operating the container

| Task | Command |
|---|---|
| Manage keys and buckets | `docker compose exec storlite storlite admin ...` |
| Offline check | `docker compose stop && docker compose run --rm storlite check --full` |
| Lost every admin key | `docker compose stop && docker compose run --rm storlite admin recover`, then `docker compose start` |
| Backup | see below |

Backups are offline. Stop the service, then write the backup to a host
directory owned by uid 65532:

```sh
sudo install -d -o 65532 -g 65532 -m 0700 /srv/storlite-backups
docker compose stop
docker compose run --rm -v /srv/storlite-backups:/backup storlite \
  backup --destination /backup/$(date +%Y%m%d-%H%M%S)
docker compose start
```

### Building the image yourself

```sh
git clone https://github.com/tirtauntario/storlite && cd storlite
docker build -f deploy/Dockerfile -t storlite:local .
STORLITE_IMAGE=storlite:local sh deploy/setup.sh ~/storlite
```

## Managing access keys and buckets

Access keys, their grants, buckets, quotas and CORS rules live in the
metadata database. You manage them with `storlite admin`, which talks to the
running server over a local Unix socket (`[admin] socket` in the config).
Changes apply to the very next S3 request; no restart or reload is needed.
Only the server's own user and root may use the socket.

| Deployment | Prefix every command with |
|---|---|
| Docker Compose | `docker compose exec storlite storlite admin` (as the default container user; root is refused because the compose file drops all capabilities) |
| systemd | `sudo storlite admin --config /etc/storlite/config.toml` |
| Local trial | `storlite admin --config config.toml` |

### Access keys

```sh
# An application key limited to one bucket. The secret is shown once.
storlite admin key create --description "rails app" \
  --grant 'documents:read,list,write,delete'

# Read-only access to one customer's prefix, expiring next year
storlite admin key create --grant 'documents/customers/123/:read,list' \
  --expires-at 2027-01-01T00:00:00Z

# Print AWS_ACCESS_KEY_ID=/AWS_SECRET_ACCESS_KEY= lines, or write them to a 0600 file
storlite admin key create --grant 'documents:read' --format env
storlite admin key create --grant 'documents:read' --output app.env

storlite admin key list                      # never shows secrets
storlite admin key show SLABCDEFGHIJKLMNOPQR
storlite admin key disable SLABCDEFGHIJKLMNOPQR
storlite admin key enable SLABCDEFGHIJKLMNOPQR
storlite admin key update SLABCDEFGHIJKLMNOPQR --description "billing" --no-expiry
storlite admin key delete SLABCDEFGHIJKLMNOPQR
```

Generated access key ids look like `SL` followed by 18 letters and digits.
Secrets are 256-bit random values (43 characters) and are shown only when a
key is created or rotated. If one is lost, rotate the key.

**Rotation without downtime:** issue a new secret and keep the old one
working for a grace period while you redeploy the application:

```sh
storlite admin key rotate SLABCDEFGHIJKLMNOPQR --grace 24h
```

With `--grace 0` (the default) the old secret stops working immediately.

### Grants

A grant gives a key actions on a bucket, optionally limited to a key prefix:
`bucket[/prefix]:action[,action...]`.

| Action | Allows |
|---|---|
| `read` | GetObject, HeadObject, presigned GET |
| `list` | ListObjectsV2 under the prefix, multipart listings |
| `write` | PutObject, CopyObject destination, multipart uploads |
| `delete` | DeleteObject, DeleteObjects, AbortMultipartUpload |
| `manage_bucket` | DeleteBucket and CORS through the S3 API (whole bucket only) |

Global grants: `admin` (everything), `list_buckets` (ListBuckets shows the
buckets the key has grants on), `create_bucket` (S3 CreateBucket).

```sh
storlite admin grant add SLABCDEFGHIJKLMNOPQR 'reports:read,list'      # adds or replaces
storlite admin grant remove SLABCDEFGHIJKLMNOPQR reports
storlite admin global-grant add SLABCDEFGHIJKLMNOPQR list_buckets
```

storlite refuses any change that would leave no enabled admin key.

### Buckets

```sh
storlite admin bucket create documents --quota 10G --cors-file cors.json
storlite admin bucket list
storlite admin bucket show documents
storlite admin bucket set-quota documents --bytes 50G     # or --clear
storlite admin bucket set-cors documents --file cors.json
storlite admin bucket clear-cors documents
storlite admin bucket delete documents                    # only when empty
```

The CORS file is a JSON array of rules, or an S3 `CORSConfiguration` XML
document:

```json
[{"allowed_origins": ["https://app.example.com"],
  "allowed_methods": ["GET", "PUT"],
  "allowed_headers": ["*"],
  "expose_headers": ["ETag"],
  "max_age_seconds": 3600}]
```

S3 clients with an admin key (or the matching grants) can still create
buckets, delete them, and manage CORS through the S3 API.

### Audit log and status

```sh
storlite admin audit --limit 20     # who changed what (never secrets)
storlite admin status               # version, store id, secret protection, counts
```

Add `--json` to any `storlite admin` command for machine-readable output.

### Lost admin keys or master key

With the server stopped:

```sh
storlite admin recover --config config.toml               # adds a new admin key
storlite admin recover --config config.toml --reset-keys  # deletes all keys first
```

Use `--reset-keys` when the master key is lost: stored secrets can no longer
be decrypted, so every key must be reissued. Buckets and objects are not
touched. Point `master_key_file` at a new key (`storlite master-key
generate`) before running it.

### Secret protection

By default (`[secrets] protection = "encrypted"`) each secret is encrypted
with AES-256-GCM under the master key. With `protection = "plaintext"` the
secrets are stored as-is, and every backup contains usable secrets. Change
the setting and restart to convert all stored secrets; switching to plaintext
needs the master key one last time.

## TLS and reverse proxies

storlite only accepts plaintext HTTP from the loopback interface. For any
other network access, pick one of these options.

**Built-in TLS (recommended).** Set `http.tls_certificate_file` and
`http.tls_private_key_file` to PEM files. The certificate file should contain
the full chain. Certificates are loaded at startup, so restart after
renewal. For example, with certbot:

```sh
certbot certonly --standalone -d storage.example.com \
  --deploy-hook 'install -o storlite -m 0644 $RENEWED_LINEAGE/fullchain.pem /etc/storlite/tls.crt &&
                 install -o storlite -m 0600 $RENEWED_LINEAGE/privkey.pem /etc/storlite/tls.key &&
                 systemctl restart storlite'
```

**Reverse proxy on the same host.** Bind storlite to loopback and let the
proxy terminate TLS:

```toml
[http]
listen = "127.0.0.1:9000"
allow_insecure_loopback_http = true
```

The proxy must preserve `Host`, the raw path and query, and the body. It must
not normalize, decompress, buffer whole requests, or strip trailers, because
SigV4 signs all of these. For nginx:

```nginx
server {
    listen 443 ssl;
    server_name storage.example.com;
    # ssl_certificate / ssl_certificate_key ...

    client_max_body_size 0;          # storlite enforces its own limits
    proxy_request_buffering off;
    proxy_buffering off;
    proxy_http_version 1.1;

    location / {
        proxy_pass http://127.0.0.1:9000;   # no URI part: the request URI is passed unchanged
        proxy_set_header Host $http_host;
        proxy_read_timeout 3600s;           # large multipart completions answer only after commit
    }
}
```

The automated interoperability suite covers direct connections only. Test
your proxy with the clients you use before relying on it.

**Reverse proxy on another host.** Set `http.trusted_proxy_mode = true` and
list the proxy's IP addresses in `http.trusted_proxy_addresses`. Connections
from any other peer are dropped. Forwarded headers are never trusted. See
[operations.md](operations.md#configuration).

## Upgrading

Schema migrations run automatically at startup. A binary refuses a store
whose schema is newer than it understands. Always take a backup first.

**systemd:**

```sh
sudo systemctl stop storlite
sudo install -d -o storlite -g storlite -m 0700 /srv/storlite-backups
sudo -u storlite storlite backup --config /etc/storlite/config.toml --destination /srv/storlite-backups/pre-upgrade
curl -fsSL https://raw.githubusercontent.com/tirtauntario/storlite/main/install.sh | STORLITE_VERSION=vX.Y.Z sh
sudo -u storlite storlite doctor --config /etc/storlite/config.toml    # reports pending migrations
sudo systemctl start storlite
```

**Docker:** take a backup as shown above, set the new tag (or pull
`latest`), then:

```sh
docker compose pull
docker compose up -d        # stops the old container before starting the new one
```

Never run two containers against the same volume. Rolling updates with
overlapping writers are not supported.

Read [CHANGELOG.md](../CHANGELOG.md) before upgrading across minor versions.

## Uninstalling

```sh
# systemd
sudo systemctl disable --now storlite
sudo rm /etc/systemd/system/storlite.service /usr/local/bin/storlite
# data and configuration (irreversible):
sudo rm -rf /var/lib/storlite /etc/storlite && sudo userdel storlite

# Docker
docker compose down          # add -v to also delete the data volume (irreversible)
```
