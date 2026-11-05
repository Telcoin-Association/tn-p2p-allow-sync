# How CI works

The binary runs as root on every validator, so every pull request passes the same five gates before it merges, and every push to `main` runs them again.
They run on GitHub's hosted runners and on a maintainer's machine with the same commands; nothing here depends on a cache, a secret or a write token.

## Workflows

### `ci.yaml`

Five jobs, one command each, on `pull_request` and on every push to `main`:

| Job | Command | What it stops |
|---|---|---|
| `fmt` | `cargo fmt --check` | unformatted code (stable rustfmt, no `rustfmt.toml`) |
| `clippy` | `cargo clippy --all-targets --locked -- -D warnings` | any warning, and the deny lints and disallowed methods of `Cargo.toml` and `clippy.toml` |
| `test` | `cargo test --locked` | a failing unit, golden or integration test |
| `deny` | `cargo deny check` | an advisory, a duplicate crate version, a wildcard requirement, a license outside MIT, Apache-2.0 and Unicode-3.0, a source other than crates.io (`deny.toml`) |
| `audit` | `cargo audit` | a RustSec advisory against `Cargo.lock` |

`RUSTFLAGS=-D warnings` is set for the whole workflow, so a compiler warning fails `test` as well as `clippy`.
The source never says `#![deny(warnings)]`: a new compiler release must not break a plain local build.
`--locked` everywhere means `Cargo.lock` is the dependency set; a change to it is a reviewed change in the pull request.
`deny` and `audit` fetch their advisory databases when they run, by design, so a newly published advisory turns them red on the next run with no change in this repository.

Every job installs the toolchain of `rust-toolchain.toml` first (`rustup show active-toolchain || rustup toolchain install`), so the runner image's own Rust is never used.

To run the gates locally, from the repository root:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo deny check
cargo audit
cargo build --locked --release --target x86_64-unknown-linux-musl
```

The last line is not a CI job; it shows that the release triple still links statically, and `target/x86_64-unknown-linux-musl/release/tn-p2p-allow-sync --version` prints the five version lines.

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

Branch protection on `main` must require `fmt`, `clippy`, `test`, `deny` and `audit`, and the two checks of `nft-real.yaml`, `nft-real (ubuntu-24.04)` and `nft-real (ubuntu-22.04)`.
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

## Toolchain

`rust-toolchain.toml` pins `1.94.1` with `rustfmt`, `clippy` and the `x86_64-unknown-linux-musl` target, and `rustup` applies it to every cargo call in the repository.
To bump it, change `rust-toolchain.toml` and `rust-version` in `Cargo.toml` together, in one pull request, along with the release build image once that exists.
