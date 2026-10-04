# Installing and running storlite

storlite is one binary. It needs a config file, a credentials file, and a
data directory on a local disk. This guide covers:

1. [Supported platforms](#supported-platforms)
2. [Installing the binary](#installing-the-binary)
3. [Running as a systemd service](#running-as-a-systemd-service) (standalone production)
4. [Docker](#docker)
5. [TLS and reverse proxies](#tls-and-reverse-proxies)
6. [Creating application credentials](#creating-application-credentials)
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

Each archive contains the binary, the example config and credentials files,
the systemd unit, the license and the changelog.

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
| `/etc/storlite/credentials.toml` | storlite, 0600 | access keys and grants |
| `/etc/storlite/tls.crt`, `tls.key` | storlite, 0644 / 0600 | TLS certificate and key |
| `/var/lib/storlite/data` | storlite, 0700 | object store |

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
credentials_file = "/etc/storlite/credentials.toml"

[http]
listen = "0.0.0.0:9000"
tls_certificate_file = "/etc/storlite/tls.crt"
tls_private_key_file = "/etc/storlite/tls.key"

[management]
listen = "127.0.0.1:9001"     # health and metrics; keep it private
```

storlite refuses plaintext HTTP on non-loopback addresses. Either configure
TLS as shown, or see [TLS and reverse proxies](#tls-and-reverse-proxies).

**3. Create the first credential.** This writes an enabled admin key with
mode 0600. The secret is inside the file and is never printed.

```sh
sudo storlite credentials generate --id admin --enable --global-grant admin \
  --output /etc/storlite/credentials.toml
sudo chown storlite:storlite /etc/storlite/credentials.toml
```

**4. Install the TLS certificate and key** (see [TLS](#tls-and-reverse-proxies)),
then check the configuration:

```sh
sudo chown storlite:storlite /etc/storlite/tls.key && sudo chmod 0600 /etc/storlite/tls.key
sudo -u storlite storlite config check --config /etc/storlite/config.toml
```

**5. Initialize the store**, as the service user, so that the data directory
has the right owner:

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
| Reload credentials after editing the file | `sudo systemctl reload storlite` (SIGHUP; an invalid file keeps the previous set) |
| Apply config or certificate changes | `sudo systemctl restart storlite` |
| Offline check, gc, backup | `sudo systemctl stop storlite`, then `sudo -u storlite storlite <command> --config /etc/storlite/config.toml` |

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
4. Creates `secrets/credentials.toml` with an enabled `admin` key.
5. Creates a self-signed certificate in `secrets/tls.crt` and `secrets/tls.key`,
   unless you put your own certificate there first.
6. Sets file modes, and on Linux gives the secrets to the container user
   (uid 65532, using `sudo`).
7. Validates `config.toml`, initializes the data volume, starts the
   container, waits until it is healthy, and prints a test command.

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
already initialized volume. Read the admin secret with
`cat secrets/credentials.toml` (`sudo cat` on Linux).

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
docker compose run --rm storlite config check --config /etc/storlite/config.toml
docker compose restart storlite
```

| File | What it controls | Apply with |
|---|---|---|
| `config.toml` | Server settings (limits, timeouts, logging, TLS file paths, proxy mode) | `docker compose restart storlite` |
| `.env` | `STORLITE_IMAGE` (version) and `STORLITE_PORT` | `docker compose up -d` |
| `secrets/credentials.toml` | Access keys and grants ([format](#creating-application-credentials)) | `docker compose kill -s HUP storlite` (no restart needed) |
| `secrets/tls.crt`, `secrets/tls.key` | TLS certificate chain and key | `docker compose restart storlite` |

On Linux the files in `secrets/` belong to uid 65532, so edit them with `sudo`
and keep them mode 0400. To get a fresh copy of the commented template, for
example after an upgrade adds settings, run
`docker run --rm ghcr.io/tirtauntario/storlite:<version> config template --docker`.

Do not change `data_dir`, `credentials_file` or the `tls_*` paths in
`config.toml`. They are paths inside the container, and `compose.yaml` mounts
the files there. `region` is fixed once the store is initialized.

### Docker Compose: manual setup

These are the same steps the script runs, if you prefer to do them yourself:

```sh
mkdir storlite && cd storlite
curl -fsSL -O https://raw.githubusercontent.com/tirtauntario/storlite/main/deploy/compose.yaml
image=ghcr.io/tirtauntario/storlite:latest

# configuration
docker run --rm "$image" config template --docker > config.toml

# admin credential (written with mode 0600, owned by you)
mkdir -m 0700 secrets
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD/secrets:/out" "$image" \
  credentials generate --id admin --enable --global-grant admin --output /out/credentials.toml

# TLS: copy a real certificate chain and key to secrets/tls.crt and
# secrets/tls.key, or create a self-signed pair:
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 365 \
  -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
  -keyout secrets/tls.key -out secrets/tls.crt

# Compose mounts secrets with their host owner and mode. storlite refuses a
# credentials file that group or others can read.
chmod 0400 secrets/*
sudo chown 65532:65532 secrets/*     # Linux only; Docker Desktop maps access itself

docker compose run --rm storlite init --config /etc/storlite/config.toml
docker compose up -d
docker compose ps                    # STATUS shows (healthy) when ready
```

Test it:

```sh
export AWS_ACCESS_KEY_ID=admin
export AWS_SECRET_ACCESS_KEY="$(sed -n 's/^secret_access_key = "\(.*\)"/\1/p' secrets/credentials.toml)"
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
| Offline check | `docker compose stop && docker compose run --rm storlite check --config /etc/storlite/config.toml --full` |
| Backup | see below |

Backups are offline. Stop the service, then write the backup to a host
directory owned by uid 65532:

```sh
sudo install -d -o 65532 -g 65532 -m 0700 /srv/storlite-backups
docker compose stop
docker compose run --rm -v /srv/storlite-backups:/backup storlite \
  backup --config /etc/storlite/config.toml --destination /backup/$(date +%Y%m%d-%H%M%S)
docker compose start
```

### Building the image yourself

```sh
git clone https://github.com/tirtauntario/storlite && cd storlite
docker build -f deploy/Dockerfile -t storlite:local .
STORLITE_IMAGE=storlite:local sh deploy/setup.sh ~/storlite
```

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

## Creating application credentials

Give each application its own key, scoped to what it needs. Generate a
disabled credential:

```sh
storlite credentials generate --id documents-app --output documents-app.secret.toml
```

Then merge its `[[credentials]]` block into the credentials file. Add grants
and set `enabled = true`:

```toml
[[credentials]]
id = "documents-app"
secret_access_key = "...generated..."
enabled = true
global_grants = []

[[credentials.grants]]
bucket = "documents"
prefix = ""                       # or e.g. "customers/123/"
actions = ["read", "list", "write", "delete"]
```

Validate the file, reload the server, and delete the fragment:

```sh
storlite credentials check --file /etc/storlite/credentials.toml
sudo systemctl reload storlite            # or: docker compose kill -s HUP storlite
rm documents-app.secret.toml
```

Actions: `read`, `list`, `write`, `delete`, `manage_bucket`. Global grants:
`list_buckets`, `create_bucket`, `admin`. See
[`examples/credentials.example.toml`](examples/credentials.example.toml) and
[operations.md](operations.md#credentials).

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
