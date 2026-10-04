# Releasing storlite

Pushing a version tag publishes a release. No registry submissions are
needed: users install from GitHub Releases, GHCR, the install script, or
`cargo install --git` (see [installation.md](installation.md)).

## What a tag publishes

`.github/workflows/release.yml` runs on any `v*` tag:

| Job | Output |
|---|---|
| `check-version` | Fails unless the tag equals `v` + the `version` in `Cargo.toml`. |
| `binaries` | Release builds for `x86_64`/`aarch64-unknown-linux-musl` (static) and `aarch64`/`x86_64-apple-darwin`, packaged as `storlite-<tag>-<target>.tar.gz` with a `.sha256` file each. |
| `github-release` | A GitHub Release with generated notes, every archive, every checksum and a combined `SHA256SUMS`. Tags with a `-` (e.g. `v0.2.0-rc.1`) are marked pre-release. |
| `image`, `image-manifest` | `ghcr.io/tirtauntario/storlite` for `linux/amd64` + `linux/arm64`, built natively on each architecture. Tags: `X.Y.Z`, `X.Y`, `X` (from 1.0 on), and `latest` for stable releases only. |

`install.sh` always resolves "latest" through the GitHub API, so it picks up a
new release with no changes.

## One-time setup

1. **Make the repository public** (Settings → General → Danger Zone).
2. **Allow Actions to run** (Settings → Actions → General). The workflows only
   use the built-in `GITHUB_TOKEN`, so no secrets are needed.
3. **Make the container image public after the first release.** GHCR creates
   new packages as private. Go to your profile → Packages → `storlite` →
   Package settings → Change visibility → Public. The image is linked to the
   repository by its `org.opencontainers.image.source` label.
4. **Enable private vulnerability reporting** (Settings → Security →
   Private vulnerability reporting). `SECURITY.md` points reporters there.
5. Optional: protect `main` and require the `CI` checks to pass.

## Cutting a release

```sh
# 1. bump the version
$EDITOR Cargo.toml                 # version = "0.2.0"
cargo check --locked || cargo check   # refreshes Cargo.lock for the new version
# 2. move the "Unreleased" notes in CHANGELOG.md under "## [0.2.0] - YYYY-MM-DD"
# 3. commit, tag, push
git commit -am "Release v0.2.0"
git tag -a v0.2.0 -m "storlite v0.2.0"
git push origin main v0.2.0
```

Watch the run under Actions → Release. Then check the result:

```sh
gh release view v0.2.0
docker buildx imagetools inspect ghcr.io/tirtauntario/storlite:0.2.0   # two platforms
curl -fsSL https://raw.githubusercontent.com/tirtauntario/storlite/main/install.sh \
  | STORLITE_INSTALL_DIR=/tmp/storlite-check sh
```

If a job fails, fix the problem and re-run the failed jobs. If the fix needs a
code change, delete the release and tag (`gh release delete v0.2.0
--cleanup-tag`), then tag again.

Versioning follows [SemVer](https://semver.org). Before 1.0, a minor bump may
include breaking changes; record them, and any schema migration, in the
changelog.

## Optional package channels

None of these are required. Each one is a separate, opt-in step if users ask
for it.

| Channel | Effort | How |
|---|---|---|
| **crates.io** (`cargo install storlite`) | Low, once per release | Set `publish = true` in `Cargo.toml`, run `cargo publish --dry-run`, then `cargo login` and `cargo publish` after tagging. To automate it, add a job to `release.yml` that runs `cargo publish` with a `CARGO_REGISTRY_TOKEN` secret. |
| **Homebrew tap** (`brew install tirtauntario/tap/storlite`) | Low | Create a repo named `tirtauntario/homebrew-tap` with `Formula/storlite.rb`. The formula points at the release archives and their SHA-256 values; update them on each release (by hand or with a workflow). homebrew-core itself requires an established, notable project. |
| **Docker Hub** mirror | Low | Add a `docker/login-action` step for Docker Hub (with `DOCKERHUB_USERNAME`/`DOCKERHUB_TOKEN` secrets) and a second image name in both `metadata-action` steps. |
| **AUR, Nix, distro packages** | Medium, ongoing | Usually maintained by community packagers. Link to them from the README when they exist. |
