# Releasing

Releases are cut, signed and published with the shared signing repository `tn-release-signing`, which this repository carries as a git submodule at `release/`.
The submodule's `release.sh` holds the tag grammar, the maintainer allowlist, the signature threshold and the GitHub release layer; this repository keeps its own values and build in `release.conf`.
The full reference for the steps, the environment variables and the exit codes is `release/docs/signing.md`, and the key table and threshold are in `release/KEYS.md`.

The release wiring lands in its own commit, `ci(release): signing submodule, reproducible build, workflow`, which adds `.gitmodules`, `release/`, `release.conf`, `etc/Dockerfile`, `.dockerignore`, `Makefile`, `cliff.toml`, `CHANGELOG.md`, `SECURITY.md`, `.github/workflows/release.yaml`, `.github/CODEOWNERS` and `etc/tests/release_dry_run.sh`.
This page describes that wiring; a checkout from before that commit has none of these files.

## Setup

Every machine that runs a step needs the submodule at the commit `main` records, so run this after every pull of `main`:

```sh
git submodule update --init -- release
```

The build server needs Docker with buildx; the laptop that signs needs the maintainer's YubiKey, set up as `release/docs/yubikey-setup.md` describes.

## release.conf

`release.conf` in the repository root is `release/examples/cargo.release.conf` with this repository's values:

| Key | Value |
|---|---|
| `PROJECT` | `tn-p2p-allow-sync` |
| `DEFAULT_REPO` | `Telcoin-Association/tn-p2p-allow-sync` |
| `TAG_RE` | `vX.Y.Z` and `vX.Y.Z-rcN`, one channel, no features |
| `TRIPLE` | `x86_64-unknown-linux-musl` |
| `TARBALL_FILES` | `tn-p2p-allow-sync LICENSE-APACHE LICENSE-MIT NOTICE` |
| `VERSION_FILE` | `Cargo.toml`, whose `[package]` version must equal the tag |
| `CHANGELOG_FILE`, `ANCHOR` | `CHANGELOG.md`, `<!-- ANCHOR: releases -->` |
| `MAIN_REF` | `refs/remotes/origin/main` |
| `KEYS_PATH`, `SELF_REPO_PATH` | `release/keys`, `release` |
| `MIN_SIGNATURES` | `1`, never below the `Signatures required` line of `release/KEYS.md`, which `release.sh` enforces |
| `BUILD_IMAGE` | `rust:1.94-alpine` pinned by digest |

Its hooks:

- `hook_build` runs two `--no-cache` builds of `etc/Dockerfile` in `BUILD_IMAGE` with `RUSTUP_TOOLCHAIN=1.94.1`, a fixed `WORKDIR` and the tag's commit passed as `TN_P2P_ALLOW_GIT_SHA`, exports the binary from a `FROM scratch` stage with `buildx --output type=local`, requires the two binaries to have the same sha256, and packs the tarball with the shared deterministic `pack_tarball`.
- `hook_check_release_files` runs the released binary's `--version` in `BUILD_IMAGE`, with no network and as an unprivileged user, and requires exactly five lines: `tn-p2p-allow-sync` and the version, `commit` and the tag's commit, `target x86_64-unknown-linux-musl`, a `snapshot-versions` line that lists 1, and `config-version 1`.
  With `TN_P2P_ALLOW_REBUILD=1` it also rebuilds the binary from the tag's commit, with that commit's `etc/Dockerfile` and `BUILD_IMAGE`, and requires the same file; `build`'s own check skips the rebuild, because `hook_build` has just built twice.
- `hook_changelog` writes the new `CHANGELOG.md` section from `cliff.toml`.

`build` and `publish` refuse a `release.conf` that differs from `main`'s copy.

## Make targets

Each target runs one step from the repository root as `./release/release.sh <step> "$(TAG)"`:

| Target | Step | Where | What it does |
|---|---|---|---|
| `make release-prep TAG=vX.Y.Z` | `prep` | build server | sets the version in `Cargo.toml`, runs `cargo update --workspace`, writes the changelog section and prints the commands for the release PR |
| `make release-tag TAG=vX.Y.Z` | `tag` | laptop | signs the tag on `main`'s tip with your allowlisted key and pushes it after you confirm |
| `make release-check-tag TAG=vX.Y.Z` | `check-tag` | CI, build server, laptop | validates the pushed tag: signed, annotated, by an allowlisted key, on `main`, with the version and changelog rules |
| `make release-notes TAG=vX.Y.Z` | `notes` | CI, build server | renders the release body |
| `make release-draft TAG=vX.Y.Z` | `draft` | CI only | creates the draft release |
| `make release-build TAG=vX.Y.Z` | `build` | build server, Linux x86_64 | builds the tarball, writes `SHA256SUMS`, uploads both to the draft and prints `SHA256SUMS sha256: <hex>` |
| `make release-sign TAG=vX.Y.Z` | `sign` | laptop | re-checks the draft's files, compares the `SHA256SUMS` hash with the one `build` printed, and appends your signature to `SHA256SUMS.asc`; it rebuilds the binary only with `TN_P2P_ALLOW_REBUILD=1`, since an amd64 build under emulation on a Mac is slow |
| `make release-verify TAG=vX.Y.Z` | `verify` | build server, CI, operators | re-checks the tag, the assets, the signatures and the `--version` lines, and rebuilds the binary from the tag, since the target sets `TN_P2P_ALLOW_REBUILD=1` unless you pass `TN_P2P_ALLOW_REBUILD=0` |
| `make release-publish TAG=vX.Y.Z` | `publish` | build server | verifies, then publishes the draft after you confirm |

The Makefile also has `make ci`, the five gates of the README, and `make nft-real`, the real-kernel test.

## Cutting a release

1. `make release-prep TAG=vX.Y.Z` on the build server, then open the PR `release: vX.Y.Z` it prints and merge it alone.
2. `make release-tag TAG=vX.Y.Z` on the laptop, one touch of the YubiKey.
3. The tag push runs `.github/workflows/release.yaml`: `validate-tag` runs `check-tag` and `notes`, and `draft-release` runs `draft`, the only job with `contents: write`.
4. `make release-build TAG=vX.Y.Z` on the build server; note the `SHA256SUMS sha256:` line it prints last.
5. `make release-sign TAG=vX.Y.Z` on the laptop of each signer, with that hash at the prompt or in `RELEASE_SUMS_SHA256`, one touch each.
6. `make release-verify TAG=vX.Y.Z`, then `make release-publish TAG=vX.Y.Z`, on the build server.
7. The workflow's `verify-release` job runs `verify` on the published release, with `TN_P2P_ALLOW_REBUILD=1` and a read-only token, as a detective check.

`tag` and `sign` need the maintainer's YubiKey, so a maintainer runs them by hand.
A final version must be higher than every other final, and a release candidate's higher than every final.
After publishing, the deployment scripts pick the release up only when a `chore` commit of `tn-node-deployment` moves `TN_P2P_ALLOW_VERSION` and `TN_P2P_ALLOW_TARBALL_SHA256` in `lib/common.sh` to the new tag and the tarball hash ([installing.md](installing.md)).

## Reproducibility

Unlike the node, this build is reproducible, and the release relies on it instead of an attestation.

- `build` uploads nothing unless two `--no-cache` builds of the tag gave the same binary, byte for byte.
- The build reads no git and no clock: the commit reaches the binary only as `TN_P2P_ALLOW_GIT_SHA` through `option_env!`, and `pack_tarball` sorts the members, fixes owner and group to 0 and the mtime to the commit time, and gzips without a name or a timestamp.
- `make release-verify` and the `verify-release` job rebuild the binary from the tag's commit and require the same file, so a release that was not built from its tag fails verification.
- Anyone can rebuild a tag with `etc/Dockerfile` and the pinned `BUILD_IMAGE` and compare the binary's sha256 with the one in the released tarball.

The full test suite, the real-kernel job included, is a required check on every pull request, so every commit a tag can name has passed it.

## Rehearsing

`bash etc/tests/release_dry_run.sh` runs every step on a copy of this repository with `RELEASE_DRY_RUN=1` and a throwaway key, so neither `gh` nor `git push` is called; CI runs it as the `release-dry-run` job.
The build in it is real: `build` builds the binary twice in `BUILD_IMAGE` and `verify` rebuilds it with `TN_P2P_ALLOW_REBUILD=1`, so the run also proves the build reproducible and needs Docker with buildx and the network; every check of the release files runs the five-line `--version` check in Docker as well.
`RELEASE_BUILD_STUB=1` replaces the Docker build with a stub that prints the five `--version` lines, for a quick local run; the script refuses it under GitHub Actions (`GITHUB_ACTIONS=true`), so CI always builds for real.
To rehearse by hand, follow "Rehearse with a dry run" in `release/docs/signing.md`.

## Moving the submodule pointer

A change to the keys, the threshold or the scripts in `tn-release-signing` reaches this repository only when `main` records a commit that carries it.
On a branch that starts at an up-to-date `main`:

```sh
git submodule update --init -- release &&
git -C release fetch --quiet origin main &&
git -C release checkout --quiet --detach FETCH_HEAD &&
git diff --submodule=log -- release &&
git -C release diff "$(git rev-parse HEAD:release)" HEAD -- keys KEYS.md release.sh &&
git add release
```

The two `diff` commands show the commits the move brings in and their changes to the keys, the key table and the script, which is what the reviewer checks.
Commit, open a PR, and have a maintainer who holds a release key review it, since the pointer decides which keys may sign this repository's releases.
`.github/CODEOWNERS` holds the rule `release/docs/signing.md` asks every consumer for, on `/release`, `/.gitmodules` and `/release.conf`, so such a PR needs a code owner's review once `main` requires one.
Once it merges, every machine that runs a step runs `git submodule update --init -- release` after its next pull of `main`.
