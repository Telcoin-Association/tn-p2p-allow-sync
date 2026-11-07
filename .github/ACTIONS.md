# How CI works

The binary runs as root on every validator, so every pull request passes the same gates before it merges, and every push to `main` runs them again: the seven jobs of `ci.yaml` (`fmt`, `clippy`, `test`, `coverage`, `deny`, `audit` and `release-dry-run`) and the three legs of `nft-real.yaml` (`nft-real (ubuntu-24.04)`, `nft-real (ubuntu-22.04)` and `nft-real (debian:13)`), the ten checks of "Required checks".
They run on GitHub's hosted runners and on a maintainer's machine with the same commands; nothing here depends on a cache, a secret or a write token.
`scheduled.yaml` adds weekly mutation testing, fuzzing, `cargo deny` and `cargo audit` on `main`, which report and gate nothing.

## Workflows

### `ci.yaml`

Seven jobs, one command each (`coverage` adds its gate), on `pull_request` and on every push to `main`:

| Job | Command | What it stops |
|---|---|---|
| `fmt` | `cargo fmt --check` | unformatted code (stable rustfmt, no `rustfmt.toml`) |
| `clippy` | `cargo clippy --all-targets --locked -- -D warnings` | any warning, and the deny lints and disallowed methods of `Cargo.toml` and `clippy.toml` |
| `test` | `cargo test --locked` | a failing unit, golden or integration test |
| `coverage` | `cargo llvm-cov --locked --summary-only --json --output-path target/coverage.json`, then `bash etc/coverage-gate.sh target/coverage.json etc/coverage-baseline` | a total line coverage more than 0.5 points below `etc/coverage-baseline` |
| `deny` | `cargo deny check` | an advisory, a duplicate crate version, a wildcard requirement, a license outside MIT, Apache-2.0 and Unicode-3.0, a source other than crates.io (`deny.toml`) |
| `audit` | `cargo audit` | a RustSec advisory against `Cargo.lock` |
| `release-dry-run` | `bash etc/tests/release_dry_run.sh` | a release step that would fail on this tree (`release.conf`, a hook, `cliff.toml` or `CHANGELOG.md` out of step with `release/release.sh`), a build that is not reproducible, and a program whose `--version` lines the release check refuses |

`coverage` runs the tests of `test` again, instrumented by `cargo llvm-cov` (its `llvm-tools-preview` comes with the toolchain of `rust-toolchain.toml`), and `etc/coverage-gate.sh` compares the total line coverage with the one number in `etc/coverage-baseline`.
More than 0.5 points below it fails the job; more than 0.5 above it prints a reminder to raise the baseline, which moves up in the pull request that adds the tests and down only as a reviewed change to the gate.
The job uploads `target/coverage.json` (the summary the gate reads) and `target/lcov.info` (per line, for an editor or a coverage viewer) as the artifact `coverage`, also when the gate fails.

`RUSTFLAGS=-D warnings` is set for the whole workflow, so a compiler warning fails `test` as well as `clippy`.
The source never says `#![deny(warnings)]`: a new compiler release must not break a plain local build.
`--locked` everywhere means `Cargo.lock` is the dependency set; a change to it is a reviewed change in the pull request.
`deny` and `audit` fetch their advisory databases when they run, by design, so a newly published advisory turns them red on the next run with no change in this repository.
That next run comes within a week even with no pull request open: `scheduled.yaml` runs the same two commands every Monday on `main` as `deny-weekly` and `audit-weekly` (see "`scheduled.yaml`").
`ci.yaml` itself has no schedule and no job-level `if:`, so every job runs on every event and none is skipped: a skipped job leaves a `skipped` check run under a required check's name, which branch protection would take as the newest run of that name, and the release gate of `release.conf` counts a skipped job as none.
A dispatch (*Actions* -> *CI* -> *Run workflow*) runs every job on the branch it is started on.

A new push to a pull request cancels the runs of `ci.yaml` and `nft-real.yaml` for its previous head (`concurrency`, with `cancel-in-progress` on `pull_request` only).
Every other run is a concurrency group of its own, because GitHub cancels a waiting run when a newer one joins its group: a push to `main` and a dispatch never wait for, cancel or displace another run, so every push to `main` reports.

The cargo jobs install the toolchain of `rust-toolchain.toml` first (`rustup show active-toolchain || rustup toolchain install`), so the runner image's own Rust is never used.
`release-dry-run` installs none: its build runs in `BUILD_IMAGE` (see "Release workflow"), and it needs only docker with buildx, gpg and the network for the images and crates.io.
`RELEASE_BUILD_STUB=1` swaps the docker build for a stub in a quick local run, and the script refuses it in CI.

To run the gates locally, from the repository root:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo llvm-cov --locked --summary-only --json --output-path target/coverage.json
bash etc/coverage-gate.sh target/coverage.json etc/coverage-baseline
cargo deny check
cargo audit
bash etc/tests/release_dry_run.sh
cargo build --locked --release --target x86_64-unknown-linux-musl
```

The last line is not a CI job; it shows that the release triple still links statically, and `target/x86_64-unknown-linux-musl/release/tn-p2p-allow-sync --version` prints the five version lines.
`make ci` runs `fmt`, `clippy`, `test`, `deny` and `audit` with `RUSTFLAGS=-D warnings`, as CI does.

### `nft-real.yaml`

One job, `nft-real`, on `pull_request` and on every push to `main`, in three legs, each its own check:

| Check | Runner | Where the test runs | Artifact |
|---|---|---|---|
| `nft-real (ubuntu-24.04)` | `ubuntu-24.04` | on the runner, nft 1.0.9 | `real-nft-ubuntu-24.04` |
| `nft-real (ubuntu-22.04)` | `ubuntu-22.04` | on the runner, nft 1.0.2 | `real-nft-ubuntu-22.04` |
| `nft-real (debian:13)` | `ubuntu-24.04` | in a privileged `debian:13` container pinned by digest, with Debian's nft | `real-nft-debian-13` |

The runners are pinned by release rather than `ubuntu-latest` because the question the job answers is about kernels and nft versions.
The two Ubuntu legs install `nftables` and `iproute2`, build the test binary as the runner user (`cargo test --locked --no-run --test real_nft`), and run only that binary under `sudo -E` with `--ignored`.
The Debian leg runs `make nft-real-docker DISTRO=debian:13@sha256:<digest>`: it builds the test binary for `x86_64-unknown-linux-musl` as the runner user, so it starts whatever the image's glibc, and runs it with `docker run --privileged` in that image, with its `nftables` and `iproute2` and with `TN_REAL_NFT_REQUIRED=1`.
A container shares the runner's kernel, so that leg answers for Debian 13's nft and its JSON listing against Ubuntu 24.04's kernel, not for Debian's kernel.

The leg's image is the `distro` value of its matrix entry, `debian:13@sha256:9c6e381421d0f48d1b8133e573f33c185ee16cbf505eda9c4a8a4791b25ea8a6`, the digest of the multi-platform index, so a retagged `debian:13` changes nothing until a pull request moves the pin.
The pin covers the base image only: `nftables` and `iproute2` still come from Debian's archive through `apt-get` at run time, unpinned on purpose, as on the Ubuntu legs, so the leg tests the versions a Debian 13 validator gets.
To move it, pick an image at least 7 days old, resolve the index digest with `docker buildx imagetools inspect debian:13` (its `Digest:` line), replace the digest after `debian:13@` in `nft-real.yaml` and here, and let the pull request's run of the leg show that it still passes; the check name stays `nft-real (debian:13)`.
Every leg uploads `target/real-nft/` (the raw `nft -j` listing after each step and `summary.txt`) as its artifact; the Debian one is named `real-nft-debian-13` because an artifact name cannot hold a `:`.

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

It is required on every pull request that touches one of the paths below: such a pull request does not merge without a green run of all three legs.
A `paths:` filter alone cannot carry that rule, because GitHub treats a required check that never reported as pending and the pull request waits for it forever.
The workflow therefore reports on every pull request, and its first step decides from `git diff --name-only HEAD^1 HEAD`, the merge commit against the base branch tip, whether the suite runs.
It runs when the pull request touches any of these:

- `src/render.rs`, `src/nft.rs`, `src/sync.rs`, and `src/addr.rs`, which parses and masks the networks the program and the compare are built from;
- `src/config.rs` (the ports, the family split and the removal of covered networks that nft would refuse as overlapping), `src/status.rs` (the counters read from the listing), `src/check.rs` (the compare of `check`), `src/cli.rs` (the `--nft` path) and `src/fsutil.rs` (the staged file `nft -f` reads);
- `tests/golden/`, `tests/fixtures/nft/`, `tests/fixtures/state/`, `tests/support/`, `tests/real_nft.rs`;
- `.github/workflows/nft-real.yaml`, and `Makefile`, whose `nft-real-docker` target is the Debian leg;
- `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, since a dependency or a compiler changes the code of these modules too.

Otherwise the job ends green with a notice.
A push to `main` always runs the suite, and so does a pull request whose diff cannot be computed.
The list lives in the `watched` pattern of the workflow's first step; change both together.

To run the test locally on a Linux host, as the Ubuntu legs do (the target builds as you and runs only the test binary under sudo):

```sh
make nft-real
```

or by hand:

```sh
cargo test --locked --no-run --test real_nft
sudo target/debug/deps/real_nft-<hash> --ignored --exact real_kernel_nft --nocapture
```

To run it in a container of another release, as the Debian leg does, with docker on the host:

```sh
make nft-real-docker DISTRO=debian:13
```

`DISTRO` names the image the test runs in (the `Makefile` target documents which it supports); the kernel is still the host's.
In a container, `ip netns add` has to mount `/run/netns`, so the container needs `--cap-add NET_ADMIN --cap-add NET_RAW --cap-add SYS_ADMIN --security-opt apparmor=unconfined`.
Build with `--target x86_64-unknown-linux-musl` there, so the static binary also starts in an image whose glibc is older than the host's, and mount the checkout at its own path, because the test writes to `target/real-nft/` under the path it was built in.

### `scheduled.yaml`

Four report-only jobs on `main`, never on a pull request: `mutants` and `fuzz` every Saturday at 03:41 UTC, `deny-weekly` and `audit-weekly` every Monday at 05:23 UTC, and all four on dispatch (*Actions* -> *Scheduled* -> *Run workflow*), on the branch it is started on.
None is a required check, and none may become one: `mutants` and `fuzz` take hours or depend on a nightly compiler, and what they find is work for a maintainer, not a reason to hold a pull request; `deny-weekly` and `audit-weekly` repeat `ci.yaml`'s `deny` and `audit`, which are the gates, under names of their own.
Each job's `if:` names the cron it does not run on, so every scheduled run also shows the other two jobs as skipped; no gate reads those runs, because no name here is a required check.
GitHub runs a schedule only on the default branch and disables it after 60 days without activity in the repository.
A disabled schedule shows under *Actions* -> *Scheduled* as disabled, and *Enable workflow* there turns it back on; check it after a quiet stretch, since nothing else reports it.

| Job | Command | What it reports |
|---|---|---|
| `mutants (<k>/4)`, for `k` 0 to 3 | `cargo mutants --cargo-arg=--locked --jobs 2 --shard <k>/4 --output target/mutants` | each change to the code (a mutant) that no test notices, in the step summary and the artifact `mutants-<k>` |
| `fuzz` | `cargo +nightly-2026-10-15 fuzz run <target> -- -max_total_time=300` in `fuzz/`, for `multiaddr`, `cidr_or_host`, `config`, `snapshot`, `nft_listing` and `http_response`, then `cargo deny --config deny.toml check` in `fuzz/` | an input that crashes a target, as the artifact `fuzz-artifacts`, and a dependency of the fuzz crate that `fuzz/deny.toml` refuses |
| `deny-weekly` | `cargo deny check` | what `ci.yaml`'s `deny` stops, here an advisory published since the last run on `main` |
| `audit-weekly` | `cargo audit` | a RustSec advisory against `Cargo.lock` published since the last run on `main` |

A red `deny-weekly` or `audit-weekly` holds nothing back by itself, but every later run of `ci.yaml`'s `deny` or `audit`, on a pull request or as a re-run on the commit of a release tag, fails the same way, so update the dependency on `main` before the next release.

The mutants are split into four shards, one runner each, because the whole set on one runner could take longer than a job may run.
Each shard stays green whatever it finds: cargo-mutants exits non-zero for a missed or timed-out mutant, and the step has `continue-on-error`.
Read each shard's step summary, which counts caught, missed, timed-out and unviable mutants and lists the first 50 missed ones, and `mutants.out/` in its artifact, which has all of them with their diffs.
The step stops after 300 minutes so that the summary and the upload still run; a shard cut short reports what it reached.
`fuzz` runs every target even after one fails, then fails if any did, and uploads `fuzz/artifacts/<target>/` with the crashing inputs.
The fuzz crate is not a dependency of the program, so nothing of it reaches a release; its dependencies get their own `cargo deny` run because `ci.yaml`'s `deny` sees only the program's.
No job of `scheduled.yaml` sets `RUSTFLAGS=-D warnings`: a mutant that leaves a value unused has to build and be tested, not be counted as unviable.

To run them locally, from the repository root:

```sh
cargo install --locked cargo-mutants@27.1.0
cargo mutants --cargo-arg=--locked --output target/mutants

rustup toolchain install nightly-2026-10-15 --profile minimal
cargo +nightly-2026-10-15 install --locked cargo-fuzz@0.13.2
cd fuzz
cargo +nightly-2026-10-15 fuzz run <target> -- -max_total_time=300
cargo +nightly-2026-10-15 fuzz run <target> artifacts/<target>/<crash-file>   # replay one input
```

## Required checks

Branch protection on `main` must require `fmt`, `clippy`, `test`, `coverage`, `deny`, `audit` and `release-dry-run`, and the three checks of `nft-real.yaml`, `nft-real (ubuntu-24.04)`, `nft-real (ubuntu-22.04)` and `nft-real (debian:13)`.
`coverage` and `nft-real (debian:13)` are new in v0.1.1 and have to be added to the list by hand; `coverage` belongs there, because a gate that is not required lets a pull request merge while it is red.
Those settings live on GitHub, not in this repository, so whoever creates or changes the repository sets them.
No job of `release.yaml` or `scheduled.yaml` may share its name with one of these checks (today: `validate-tag`, `draft-release`, `rebuild`, `verify-release`, `mutants (<k>/4)`, `fuzz`, `deny-weekly` and `audit-weekly`): GitHub matches a required check by name on a commit, whatever workflow reported it.
It matches by name whatever posted it, too: any GitHub App installed on the repository with `checks: write` can post a check run under a required check's name, and so can any workflow whose token has `checks: write`, run from any branch, and GitHub credits that run to GitHub Actions like a job's own.
The release gate of `release.conf` (`tn_check_ci_green`) therefore reads no check runs.
It reads the jobs of the push-to-main runs of `ci.yaml` and `nft-real.yaml` on the tagged commit (`TN_REQUIRED_WORKFLOWS`), the newest completed run of each, and requires each of the ten names to be one of those jobs, concluded `success`; a job concluded `skipped` or `neutral` counts as none ([`docs/releasing.md`](../docs/releasing.md#the-required-checks)).
Those jobs are the ones the two files define at that commit, which the review of `main` covered, and a dispatch, a pull request or a push to another branch is not such a run, however green.
Branch protection should pin each required check's source to GitHub Actions (the app chosen beside each check's name in the rule), so a pull request cannot merge on another app's run.
A change under `.github/` is a change to the gate and gets the same review as code.

## Action pins

Every `uses:` names the action by a full 40-character commit SHA, with the release it stands for in a trailing comment on the same line (`actions/checkout@<sha> # vX.Y.Z`).
A tag is a pointer its owner can move at any time; a moved tag would run new code in the gate with nothing for a reviewer to see.
A SHA cannot move, so the code that runs is the code that was reviewed when the pin was set.

| Action | Release | Commit | Runtime | Used by |
|---|---|---|---|---|
| `actions/checkout` | `v7.0.1` | `3d3c42e5aac5ba805825da76410c181273ba90b1` | node24 | every job |
| `taiki-e/install-action` | `v2.87.22` | `83ac0ad63c0167e6f06796fab0fce28db1bf3db0` | composite (shell steps only) | `deny`, `audit`, `coverage`, `mutants`, `fuzz`, `deny-weekly`, `audit-weekly` |
| `actions/upload-artifact` | `v7.0.1` | `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` | node24 | `nft-real`, `coverage`, `rebuild`, `mutants`, `fuzz` |

These are the releases `telcoin-network` pins, so one review of an upstream release covers both repositories.
All three are lightweight tags, so the tag ref names the commit directly.

The tools `install-action` installs are pinned by an explicit version as well:

| Tool | Version | Source |
|---|---|---|
| `cargo-deny` | `0.20.2` | the manifest in the pinned `install-action` commit, with its sha256 |
| `cargo-audit` | `0.22.2` | the same |
| `cargo-llvm-cov` | `0.9.1` | the same |
| `cargo-mutants` | `27.1.0` | the same (`scheduled.yaml`) |

`fuzz` installs two things outside the manifest, each pinned in `scheduled.yaml`:

| Tool | Version | Source |
|---|---|---|
| nightly toolchain | `nightly-2026-10-15` (`FUZZ_TOOLCHAIN`) | `rustup toolchain install --profile minimal`, checked against the Rust release manifest of that date |
| `cargo-fuzz` | `0.13.2` | `cargo install --locked` from crates.io, built with the `Cargo.lock` it was published with; the pinned `install-action` manifest has no cargo-fuzz |

`fallback: none` on every such step makes the step fail when the pinned manifest lacks the version, instead of fetching an unchecked build through `cargo-binstall`.
Run the same versions locally (`cargo install --locked cargo-deny@0.20.2 cargo-audit@0.22.2 cargo-llvm-cov@0.9.1`) so a local pass means a CI pass.

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

A tool version moves the same way: pick the release, confirm the pinned `install-action` manifest has it (or move that pin first), change `tool:` in the workflow and the table above, and install the same version locally.
The nightly of `fuzz` moves by an edit of `FUZZ_TOOLCHAIN` in `scheduled.yaml` and of the table and commands above: pick a date at least 14 days old, so a nightly that breaks or is withdrawn in its first days never lands, and dispatch *Scheduled* on the branch before it merges.
`cargo-fuzz` moves by its version in the same step.

### What a pin does not cover

The runner image is a label (`ubuntu-24.04`, `ubuntu-22.04`), not a digest, so everything preinstalled on it, `rustup` included, is trusted and not pinned.
`nft-real` installs `nftables` and `iproute2` unpinned on purpose, from the runner release's own archive or, on the Debian leg, from Debian 13's inside the pinned image: the job tests the versions a validator on that release gets.
`rustup` downloads the toolchain of `rust-toolchain.toml` and checks it against the Rust release manifest; that pin is the channel `1.94.1`, not a digest.
The advisory databases `deny`, `audit`, `deny-weekly` and `audit-weekly` read are fetched fresh on every run, on purpose.

## Release workflow

`.github/workflows/release.yaml` checks a release tag, opens a draft GitHub release for it, rebuilds the program from the tag on a runner, and checks the release again once a maintainer has published it.
It signs and publishes nothing: a maintainer builds, signs and publishes with the `make release-*` targets, which run `release/release.sh`, the release script of the [tn-release-signing](https://github.com/Telcoin-Association/tn-release-signing) submodule at `release/`, with this repository's values and hooks from `release.conf`.
The procedure is in [`release/docs/signing.md`](../release/docs/signing.md), and the threshold and the keys are in [`release/KEYS.md`](../release/KEYS.md).

| Job | Event | Runs | Token | Timeout |
|---|---|---|---|---|
| `validate-tag` | a pushed `v*` tag; a dispatch with `mode: validate` | `release/release.sh check-tag`, then `release/release.sh notes` into the step summary | `contents: read`, `actions: read`, `checks: read` | 15 min |
| `draft-release` | a pushed `v*` tag, once `validate-tag` has passed | `release/release.sh draft` | `contents: write` | 5 min |
| `rebuild` | a pushed `v*` tag, once `validate-tag` has passed | `bash etc/ci-rebuild.sh "$RELEASE_TAG" "$RUNNER_TEMP/rebuild"`, then uploads its `program.sha256` as the artifact `rebuild-<tag>`, such as `rebuild-v0.1.1` | `contents: read` | 30 min |
| `verify-release` | a published release (`release: published`); a dispatch with `mode: verify` | `release/release.sh verify` with `TN_P2P_ALLOW_REBUILD=1` | `contents: read` | 30 min |

The tag filter is broader than the tag grammar on purpose: a malformed `v` tag fails visibly in `validate-tag` instead of being ignored, and `TAG_RE` in `release.conf` decides what a release tag is.
A dispatch (*Release* -> *Run workflow*) takes a `tag` and a `mode` and is possible only once the workflow is on `main`; `verify` works on a published release only, because a read-only token cannot see drafts.
`actions: read` lets `check-tag` read when GitHub created its run, and lets the tag gate of `release.conf` list the runs of `ci.yaml` and `nft-real.yaml` on the tagged commit and read their jobs; the gate reads no check runs, so the job's `checks: read` is unused.
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
The builds before publish run on the build server (`build`'s two and `verify`'s rebuild) and in the CI `rebuild` job, the one the server does not control: on the push of the tag, before anything is signed, a GitHub runner builds the tagged commit the same way with `etc/ci-rebuild.sh`.
It writes `program.sha256`, one line `<sha256>  tn-p2p-allow-sync`, to the step summary and uploads it as the artifact `rebuild-<tag>` (for v0.1.1, `rebuild-v0.1.1`).
Only the push of the tag runs `rebuild`, never a dispatch or a published release, so the run that holds `rebuild-<tag>` is the `push` run of that tag (`gh run list --workflow release.yaml --event push`, then `gh run download <run-id> -n rebuild-<tag>`).
While the release is a draft, `sign`, `verify` and `publish` each compare the draft's program with that sha256 and stop on a difference; `build`'s own check and a published release skip the comparison ([`docs/releasing.md`](../docs/releasing.md#the-ci-rebuild)).
A rebuild takes `etc/Dockerfile` and the `BUILD_IMAGE` line of `release.conf` from the tagged commit, so an older release stays reproducible after `main` moves the image.
Every check also runs the program's `--version` in `BUILD_IMAGE`, with no network, and requires the five lines of the release: version, the tag's commit, the triple, a `snapshot-versions` list holding 1, and `config-version 1`.
The one job with a write token, `draft-release`, compiles nothing; the builds in `rebuild` and `verify-release` run the build code of every dependency, inside a container that receives no token, in jobs whose token can only read.

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
4. **`release-dry-run` among the required checks of `main`**, next to the other jobs of `ci.yaml` and the three checks of `nft-real.yaml` ("Required checks").
5. **The default workflow permission can stay read-only** (Settings -> Actions -> General -> Workflow permissions): `draft-release` asks for `contents: write` in its own `permissions:` block.

### Images

`BUILD_IMAGE` in `release.conf` (the build, the rebuild and the `--version` check) and `GIT_CLIFF_IMAGE` in `release/release.sh` (the changelog section `prep` writes) are pinned by digest.
The first moves with `RUSTUP_TOOLCHAIN` in `etc/Dockerfile` and `rust-toolchain.toml`, by the commands in the comment above it, and the second moves with the submodule pointer.
`docker` and `buildx` are the runner image's own, trusted and not pinned, like the rest of it.

## Toolchain

`rust-toolchain.toml` pins `1.94.1` with `rustfmt`, `clippy`, `llvm-tools-preview` (for `coverage`) and the `x86_64-unknown-linux-musl` target, and `rustup` applies it to every cargo call in the repository.
To bump it, change `rust-toolchain.toml`, `rust-version` in `Cargo.toml`, `RUSTUP_TOOLCHAIN` in `etc/Dockerfile` and `BUILD_IMAGE` in `release.conf` together, in one pull request.
