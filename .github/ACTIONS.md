# How CI works

The binary runs as root on every validator, so every pull request passes the same five gates before it merges, and every push to `main` runs them again.
They run on GitHub's hosted runners and on a maintainer's machine with the same commands; nothing here depends on a cache, a secret or a write token.

## Workflows

### `ci.yaml`

Six jobs, one command each, on `pull_request` and on every push to `main`:

| Job | Command | What it stops |
|---|---|---|
| `fmt` | `cargo fmt --check` | unformatted code (stable rustfmt, no `rustfmt.toml`) |
| `clippy` | `cargo clippy --all-targets --locked -- -D warnings` | any warning, and the deny lints and disallowed methods of `Cargo.toml` and `clippy.toml` |
| `test` | `cargo test --locked` | a failing unit, golden or integration test |
| `deny` | `cargo deny check` | an advisory, a duplicate crate version, a wildcard requirement, a license outside MIT, Apache-2.0 and Unicode-3.0, a source other than crates.io (`deny.toml`) |
| `audit` | `cargo audit` | a RustSec advisory against `Cargo.lock` |
| `release-dry-run` | `bash etc/tests/release_dry_run.sh` | a release step that would fail on this tree (`release.conf`, a hook, `cliff.toml` or `CHANGELOG.md` out of step with `release/release.sh`), a build that is not reproducible, and a program whose `--version` lines the release check refuses |

`RUSTFLAGS=-D warnings` is set for the whole workflow, so a compiler warning fails `test` as well as `clippy`.
The source never says `#![deny(warnings)]`: a new compiler release must not break a plain local build.
`--locked` everywhere means `Cargo.lock` is the dependency set; a change to it is a reviewed change in the pull request.
`deny` and `audit` fetch their advisory databases when they run, by design, so a newly published advisory turns them red on the next run with no change in this repository.

The five cargo jobs install the toolchain of `rust-toolchain.toml` first (`rustup show active-toolchain || rustup toolchain install`), so the runner image's own Rust is never used.
`release-dry-run` installs none: its build runs in `BUILD_IMAGE` (see "Release workflow"), and it needs only docker with buildx, gpg and the network for the images and crates.io.
`RELEASE_BUILD_STUB=1` swaps the docker build for a stub in a quick local run, and the script refuses it in CI.

To run the gates locally, from the repository root:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo deny check
cargo audit
bash etc/tests/release_dry_run.sh
cargo build --locked --release --target x86_64-unknown-linux-musl
```

The last line is not a CI job; it shows that the release triple still links statically, and `target/x86_64-unknown-linux-musl/release/tn-p2p-allow-sync --version` prints the five version lines.
`make ci` runs the five cargo gates with `RUSTFLAGS=-D warnings`, as CI does.

### `nft-real.yaml`

One job, `nft-real`, on a matrix of `ubuntu-24.04` and `ubuntu-22.04`, on `pull_request` and on every push to `main`.
The runners are pinned by release rather than `ubuntu-latest` because the question the job answers is about kernels and nft versions: 22.04 ships nft 1.0.2 and 24.04 ships nft 1.0.9.
It installs `nftables` and `iproute2`, builds the test binary as the runner user (`cargo test --locked --no-run --test real_nft`), runs only that binary under `sudo -E` with `--ignored`, and uploads `target/real-nft/` (the raw `nft -j` listing after each step and `summary.txt`) as the artifact `real-nft-<runner>`.

`tests/real_nft.rs` creates a network namespace and a veth pair, loads the rendered program into the runner's own namespace, and sends UDP from the far end of the pair:

| Step | What it asserts |
|---|---|
| traffic | the accept and drop counters count exactly the datagrams from the listed and the unlisted source, only the listed ones are delivered, and a port outside the program is left alone |
| re-apply | loading the same program, then one with one more element, keeps both counters' totals, and the rules go on counting into them; a reset fails the job, and the choice of program form goes back to the plan (open question 9) instead of the job switching forms |
| reload | after `nft delete table`, the program recreates the table with both counters at zero |
| top of the address space | how nft lists `0.0.0.0/0`, `::/0`, `128.0.0.0/1` and `8000::/1` in the static sets: a prefix or the equivalent range passes, the sync's idempotence compare matches either, and `summary.txt` records which one nft printed |
| load time | a program of 10000 IPv4 plus 10000 IPv6 addresses loads in under 10 s over the live table, again over itself, and from nothing |

The test refuses to run where table `inet tn_p2p_allow` already exists, and deletes the table and the namespace when it ends.
Without root, nft or ip it prints why and passes; the job sets `TN_REAL_NFT_REQUIRED=1`, which turns that skip into a failure.

It is required on every pull request that touches `render`, `nft`, `sync` or `addr`: such a pull request does not merge without a green run on both runners.
A `paths:` filter alone cannot carry that rule, because GitHub treats a required check that never reported as pending and the pull request waits for it forever.
The workflow therefore reports on every pull request, and its first step decides from `git diff --name-only HEAD^1 HEAD`, the merge commit against the base branch tip, whether the suite runs.
It runs when the pull request touches any of these:

- `src/render.rs`, `src/nft.rs`, `src/sync.rs`, and `src/addr.rs`, which parses and masks the networks the program and the compare are built from;
- `tests/golden/`, `tests/fixtures/nft/`, `tests/support/`, `tests/real_nft.rs`;
- `.github/workflows/nft-real.yaml`;
- `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, since a dependency or a compiler changes the code of the four modules too.

Otherwise the job ends green with a notice.
A push to `main` always runs the suite, and so does a pull request whose diff cannot be computed.
The list lives in the `watched` pattern of the workflow's first step; change both together.

To run the test locally, as root on a Linux host:

```sh
cargo test --locked --no-run --test real_nft
sudo target/debug/deps/real_nft-<hash> --ignored --exact real_kernel_nft --nocapture
```

In a container, `ip netns add` has to mount `/run/netns`, so the container needs `--cap-add NET_ADMIN --cap-add NET_RAW --cap-add SYS_ADMIN --security-opt apparmor=unconfined`.
Build with `--target x86_64-unknown-linux-musl` there, so the static binary also starts in an image whose glibc is older than the host's, and mount the checkout at its own path, because the test writes to `target/real-nft/` under the path it was built in.

## Required checks

Branch protection on `main` must require `fmt`, `clippy`, `test`, `deny`, `audit` and `release-dry-run`, and the two checks of `nft-real.yaml`, `nft-real (ubuntu-24.04)` and `nft-real (ubuntu-22.04)`.
Those settings live on GitHub, not in this repository, so whoever creates or changes the repository sets them.
A change under `.github/` is a change to the gate and gets the same review as code.

## Action pins

Every `uses:` names the action by a full 40-character commit SHA, with the release it stands for in a trailing comment on the same line (`actions/checkout@<sha> # vX.Y.Z`).
A tag is a pointer its owner can move at any time; a moved tag would run new code in the gate with nothing for a reviewer to see.
A SHA cannot move, so the code that runs is the code that was reviewed when the pin was set.

| Action | Release | Commit | Runtime | Used by |
|---|---|---|---|---|
| `actions/checkout` | `v7.0.1` | `3d3c42e5aac5ba805825da76410c181273ba90b1` | node24 | every job |
| `taiki-e/install-action` | `v2.87.22` | `83ac0ad63c0167e6f06796fab0fce28db1bf3db0` | composite (shell steps only) | `deny`, `audit` |
| `actions/upload-artifact` | `v7.0.1` | `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` | node24 | `nft-real` |

These are the releases `telcoin-network` pins, so one review of an upstream release covers both repositories.
All three are lightweight tags, so the tag ref names the commit directly.

The tools `install-action` installs are pinned by an explicit version as well:

| Tool | Version | Source |
|---|---|---|
| `cargo-deny` | `0.20.2` | the manifest in the pinned `install-action` commit, with its sha256 |
| `cargo-audit` | `0.22.2` | the same |

`fallback: none` on both steps makes the step fail when the pinned manifest lacks the version, instead of fetching an unchecked build through `cargo-binstall`.
Run the same versions locally (`cargo install --locked cargo-deny@0.20.2 cargo-audit@0.22.2`) so a local pass means a CI pass.

### How a pin moves

There is no Dependabot here; pins move by hand, in a pull request of their own.

1. Pick a release at least 7 days old, so one that is pulled or found compromised in its first days never lands, and read the release notes of every release between the old pin and the new one.
2. Resolve the tag to its commit:

   ```sh
   gh api repos/<owner>/<repo>/git/ref/tags/<tag> --jq '.object.type + " " + .object.sha'
   ```

   For `commit`, that SHA is the pin.
   For `tag` (an annotated tag), dereference it with `gh api repos/<owner>/<repo>/git/tags/<sha> --jq .object.sha` and pin that commit.
   `git ls-remote https://github.com/<owner>/<repo> 'refs/tags/<tag>' 'refs/tags/<tag>^{}'` gives the same answer without a token; the `^{}` line, when present, is the commit.
3. For `install-action`, check that the new commit's manifest still carries each pinned tool version:

   ```sh
   gh api "repos/taiki-e/install-action/contents/manifests/cargo-deny.json?ref=<sha>" --jq .content | base64 -d | jq '."0.20.2"'
   ```

   A `null` answer means that version is gone from the manifest; with `fallback: none` the job would fail, so bump the tool version too.
4. Rewrite the SHA and the `# vX.Y.Z` comment together on every line that uses the action, and update the tables above in the same commit.

A tool version moves the same way: pick the release, confirm the pinned `install-action` manifest has it (or move that pin first), change `tool:` in `ci.yaml` and the table above, and install the same version locally.

### What a pin does not cover

The runner image is a label (`ubuntu-24.04`, `ubuntu-22.04`), not a digest, so everything preinstalled on it, `rustup` included, is trusted and not pinned.
`nft-real` installs `nftables` and `iproute2` from the runner release's own archive, unpinned on purpose: the job tests the versions a validator on that release gets.
`rustup` downloads the toolchain of `rust-toolchain.toml` and checks it against the Rust release manifest; that pin is the channel `1.94.1`, not a digest.
The advisory databases `deny` and `audit` read are fetched fresh on every run, on purpose.

## Release workflow

`.github/workflows/release.yaml` checks a release tag, opens a draft GitHub release for it, and checks the release again once a maintainer has published it.
It signs and publishes nothing: a maintainer builds, signs and publishes with the `make release-*` targets, which run `release/release.sh`, the release script of the [tn-release-signing](https://github.com/Telcoin-Association/tn-release-signing) submodule at `release/`, with this repository's values and hooks from `release.conf`.
The procedure is in [`release/docs/signing.md`](../release/docs/signing.md), and the threshold and the keys are in [`release/KEYS.md`](../release/KEYS.md).

| Job | Event | Runs | Token | Timeout |
|---|---|---|---|---|
| `validate-tag` | a pushed `v*` tag; a dispatch with `mode: validate` | `release/release.sh check-tag`, then `release/release.sh notes` into the step summary | `contents: read`, `actions: read` | 15 min |
| `draft-release` | a pushed `v*` tag, once `validate-tag` has passed | `release/release.sh draft` | `contents: write` | 5 min |
| `verify-release` | a published release (`release: published`); a dispatch with `mode: verify` | `release/release.sh verify` with `TN_P2P_ALLOW_REBUILD=1` | `contents: read` | 30 min |

The tag filter is broader than the tag grammar on purpose: a malformed `v` tag fails visibly in `validate-tag` instead of being ignored, and `TAG_RE` in `release.conf` decides what a release tag is.
A dispatch (*Release* -> *Run workflow*) takes a `tag` and a `mode` and is possible only once the workflow is on `main`; `verify` works on a published release only, because a read-only token cannot see drafts.
No job in `release.yaml` may share its name with a required check: GitHub matches a required check by name on a commit, whatever workflow reported it, and a tag can be pushed on any commit.

### Scripts and keys come from the pointer on `main`

Every job checks out `refs/heads/main`, never the tagged tree, with `fetch-depth: 0`, `submodules: true` and `persist-credentials: false`.
`release.conf` comes from that checkout, and `release.sh` and the maintainer keys from the tn-release-signing commit that `main` records for `release/`.
`release.sh` reads `keys/*.asc` and the `Signatures required` line of `KEYS.md` out of the submodule's object store at that commit, never from its checked-out files, and `make release-build` and `make release-publish` refuse a `release.sh` or `release.conf` that differs from `main`'s.
A tag on a commit that edits a hook, moves the pointer or adds a key would otherwise be judged by its own edit.
The job definitions are the exception: GitHub reads `release.yaml` from the tagged commit, so the tag ruleset below decides who can run it for a release tag.
`submodules: true` fetches tn-release-signing with this repository's token, so that repository has to be public, as the operator's check, which clones it, needs anyway.

### Reproducible instead of attested

No attestation stands behind a release here, unlike telcoin-network: the full suite, the real-kernel job included, is a required pull request check, and the program is reproducible.
`make release-build` builds the program twice from a pristine worktree of the tag, with `etc/Dockerfile` in `BUILD_IMAGE`, `--pull` and `--no-cache`, and uploads nothing unless both builds are the same file.
`make release-verify` (where `TN_P2P_ALLOW_REBUILD` defaults to 1) and `verify-release` rebuild it from the tagged commit and require the same file again; `make release-sign` rebuilds only with `TN_P2P_ALLOW_REBUILD=1`, since an amd64 build under emulation on a Mac is slow.
A rebuild takes `etc/Dockerfile` and the `BUILD_IMAGE` line of `release.conf` from the tagged commit, so an older release stays reproducible after `main` moves the image.
Every check also runs the program's `--version` in `BUILD_IMAGE`, with no network, and requires the five lines of the release: version, the tag's commit, the triple, a `snapshot-versions` list holding 1, and `config-version 1`.
The one job with a write token, `draft-release`, compiles nothing; the rebuild in `verify-release` runs the build code of every dependency, inside a container that receives no token, in a job whose token can only read.

`ci.yaml`'s `release-dry-run` job runs all of this on every pull request, the real build included, through `etc/tests/release_dry_run.sh`.

### The post-publish check is detective only

`verify-release` repeats what `make release-publish` checked: the tag, the asset set and `SHA256SUMS`, the tarball's files, the signatures on `SHA256SUMS` against the keys at the pointer on `main`, the `--version` lines, and the rebuild.
The release is public by the time it runs, so a failure is an alarm for a maintainer, not a gate.
GitHub starts no workflow for an event a `GITHUB_TOKEN` caused, so after every publish check that a `verify-release` run exists for the tag, and start one with a `verify` dispatch if none does.

### Settings the release workflow requires

None of these is visible to the workflow, and each closes a gap the workflow leaves open.

1. **Two tag rulesets on `refs/tags/v*`** (Settings -> Rules -> Rulesets): one with *Restrict creations* and the maintainers on its bypass list, and one with *Restrict updates* and *Restrict deletions* and an empty bypass list, so only a maintainer can create a `v*` tag and nobody can move or delete one.
   A ruleset's bypass list applies to every rule in it, hence two.
   The signature is not a GitHub rule: `validate-tag` and every later step refuse a tag without exactly one signature by an allowlisted key.
2. **Immutable releases** (Settings -> General -> Releases), so the files `verify-release` checked are the files operators download.
3. **A review from code owners on `main`** (*Require review from Code Owners*), so the rule in [`CODEOWNERS`](CODEOWNERS) on `/release`, `/.gitmodules` and `/release.conf` binds and every move of the pointer, change of its URL and change of `MIN_SIGNATURES` or a hook is in front of a maintainer ([`release/docs/signing.md`](../release/docs/signing.md#codeowners-for-the-pointer)).
4. **`release-dry-run` among the required checks of `main`**, next to the five gates of `ci.yaml`.
5. **The default workflow permission can stay read-only** (Settings -> Actions -> General -> Workflow permissions): `draft-release` asks for `contents: write` in its own `permissions:` block.

### Images

`BUILD_IMAGE` in `release.conf` (the build, the rebuild and the `--version` check) and `GIT_CLIFF_IMAGE` in `release/release.sh` (the changelog section `prep` writes) are pinned by digest.
The first moves with `RUSTUP_TOOLCHAIN` in `etc/Dockerfile` and `rust-toolchain.toml`, by the commands in the comment above it, and the second moves with the submodule pointer.
`docker` and `buildx` are the runner image's own, trusted and not pinned, like the rest of it.

## Toolchain

`rust-toolchain.toml` pins `1.94.1` with `rustfmt`, `clippy` and the `x86_64-unknown-linux-musl` target, and `rustup` applies it to every cargo call in the repository.
To bump it, change `rust-toolchain.toml`, `rust-version` in `Cargo.toml`, `RUSTUP_TOOLCHAIN` in `etc/Dockerfile` and `BUILD_IMAGE` in `release.conf` together, in one pull request.
