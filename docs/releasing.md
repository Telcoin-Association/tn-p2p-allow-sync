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

The build server needs Docker with buildx; the laptop that signs needs the maintainer's YubiKey, set up as `release/docs/yubikey-setup.md` describes, and, because it runs `sign` and `publish` itself ([Sign and publish on the laptop](#sign-and-publish-on-the-laptop)), Docker and a `gh` login with write access to the repository.

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
- `hook_check_release_files` first reads the released binary's ELF headers and refuses it unless it is an x86-64 program with no `PT_INTERP` program header, that is, linked statically and asking for no loader.
  The `--version` check alone would not notice a dynamic build, because `BUILD_IMAGE` has musl's loader and a validator's host may not.
  It then runs the binary's `--version` in `BUILD_IMAGE`, with no network and as an unprivileged user, and requires exactly five lines: `tn-p2p-allow-sync` and the version, `commit` and the tag's commit, `target x86_64-unknown-linux-musl`, a `snapshot-versions` line that lists 1, and `config-version 1`.
  While the release is a draft it also requires the binary to have the sha256 that the release workflow's `rebuild` job recorded for the tag ([The CI rebuild](#the-ci-rebuild)); a dry run, `build`'s own check and a published release skip that comparison.
  With `TN_P2P_ALLOW_REBUILD=1` it also rebuilds the binary from the tag's commit, with that commit's `etc/Dockerfile` and `BUILD_IMAGE`, and requires the same file; `build`'s own check skips the rebuild, because `hook_build` has just built twice.
- `hook_check_tag_extra` runs inside `check-tag`, and so in `tag` before it creates the tag and in `build`, `sign`, `verify` and `publish`, on the tag's commit.
  It refuses a commit whose `Cargo.toml`, `.cargo/config.toml`, `.cargo/config` or `etc/Dockerfile` has a line, outside a `#` comment, that names both `panic` and `abort` in any case: the release build keeps `panic = "unwind"`, so that a panic exits 101 and the deployment scripts read it as a bug.
  It also requires the newest check run of each name in `TN_REQUIRED_CHECKS` to have concluded `success` on that commit ([The required checks](#the-required-checks)); a dry run and a published release skip that, and `tag` and `sign` leave it to `check-tag` in CI on a host without `gh`.
- `hook_changelog` writes the new `CHANGELOG.md` section from `cliff.toml`.

`release-dry-run` runs the static and unwind checks on every pull request, so a change that breaks either fails before it merges.
Should one still stop a release, fix the cause on `main`.
The unwind check stops `tag` before the tag exists; the static check first runs in `build`'s own check, after the upload, when the tag already names the broken commit, so the fix ships as the next patch version.

`build` and `publish` refuse a `release.conf` that differs from `main`'s copy.

## Make targets

Each plain target prints one line, `==> release-<step> vX.Y.Z: ...`, that says what the step does and what it will ask of you, then runs the step from the repository root as `./release/release.sh <step> "$(TAG)"`.
A `-remote` target, run on the laptop, runs the same step on the build server over ssh ([Running from the laptop](#running-from-the-laptop)):

| Target | Step | Where | What it does |
|---|---|---|---|
| `make release-prep TAG=vX.Y.Z` | `prep` | build server | sets the version in `Cargo.toml`, runs `cargo update --workspace`, writes the changelog section and prints the commands for the release PR |
| `make release-tag TAG=vX.Y.Z` | `tag` | laptop | signs the tag on `main`'s tip with your allowlisted key and pushes it after you confirm |
| `make release-check-tag TAG=vX.Y.Z` | `check-tag` | CI, build server, laptop | validates the pushed tag: signed, annotated, by an allowlisted key, on `main`, with the version and changelog rules |
| `make release-notes TAG=vX.Y.Z` | `notes` | CI, build server | renders the release body |
| `make release-draft TAG=vX.Y.Z` | `draft` | CI only | creates the draft release |
| `make release-build TAG=vX.Y.Z` | `build` | build server, Linux x86_64 | builds the tarball, writes `SHA256SUMS`, uploads both to the draft and prints `SHA256SUMS sha256: <hex>` |
| `make release-sign TAG=vX.Y.Z` | `sign` | laptop | re-checks the draft's files, compares its program with the CI rebuild of the tag and the `SHA256SUMS` hash with the one `build` printed, and appends your signature to `SHA256SUMS.asc`; it rebuilds the binary only with `TN_P2P_ALLOW_REBUILD=1`, since an amd64 build under emulation on a Mac is slow |
| `make release-verify TAG=vX.Y.Z` | `verify` | build server, CI, operators | re-checks the tag, the assets, the signatures and the `--version` lines, on a draft the CI rebuild's sha256 too, and rebuilds the binary from the tag, since the target sets `TN_P2P_ALLOW_REBUILD=1` unless you pass `TN_P2P_ALLOW_REBUILD=0` |
| `make release-publish TAG=vX.Y.Z` | `publish` | laptop | verifies, then publishes the draft after you confirm |
| `make release-<step>-remote TAG=vX.Y.Z REMOTE=host[:dir]` | `prep`, `tag`, `build`, `sign`, `verify` or `publish` | laptop | runs `make release-<step>` over ssh in the build server's checkout, which must be clean and at `origin/main`'s tip; for `tag` and `sign` it forwards the laptop's gpg-agent, so gpg on the server signs through it while the YubiKey stays in the laptop; run `sign` and `publish` with the plain targets instead ([Sign and publish on the laptop](#sign-and-publish-on-the-laptop)) |
| `make release-setup-remote REMOTE=host[:dir]` | | laptop | creates the build server's GnuPG home for the forwarded agent, imports the keys of `release/keys` into it and shows it; `release-tag-remote` and `release-sign-remote` do this first, so run it alone to check a server |

The Makefile also has `make ci`, which runs `fmt`, `clippy`, `test`, `deny` and `audit` as CI does, and the real-kernel test as `make nft-real`, under sudo, and `make nft-real-docker DISTRO=<image>`, in a container.

## Cutting a release

1. `make release-prep TAG=vX.Y.Z` on the build server, then open the PR `release: vX.Y.Z` it prints and merge it alone.
2. Once the push of that merge to `main` has finished its required checks, all green ([The required checks](#the-required-checks)), `make release-tag TAG=vX.Y.Z` on the laptop, or with the `-remote` target, one touch of the YubiKey.
3. The tag push runs `.github/workflows/release.yaml`: `validate-tag` runs `check-tag` and `notes`, `draft-release` runs `draft`, the only job with `contents: write`, and `rebuild` builds the tag on a GitHub runner and uploads its program's sha256 as the artifact `rebuild-vX.Y.Z`.
4. `make release-build TAG=vX.Y.Z` on the build server; note the `SHA256SUMS sha256:` line it prints last.
5. Once `rebuild` has finished, `make release-sign TAG=vX.Y.Z` on the laptop of each signer, with that hash at the prompt or in `RELEASE_SUMS_SHA256`, one touch each; it refuses a draft whose program differs from the CI rebuild.
6. `make release-verify TAG=vX.Y.Z` on the build server, then `make release-publish TAG=vX.Y.Z` on the laptop.
7. The workflow's `verify-release` job runs `verify` on the published release, with `TN_P2P_ALLOW_REBUILD=1` and a read-only token, as a detective check; [After publish](#after-publish) says what to do with its result.

Every step a maintainer runs can also be run from the laptop with the `-remote` targets, which run it on the build server over ssh ([Running from the laptop](#running-from-the-laptop)).
`tag` and `sign` need the maintainer's YubiKey, so a maintainer runs them by hand, at the laptop that holds it.
Run `sign` and `publish` with the plain targets on the laptop, not with `-remote`, so that the comparison with the CI rebuild runs on a machine the build server does not control ([Sign and publish on the laptop](#sign-and-publish-on-the-laptop)).
A final version must be higher than every other final, and a release candidate's higher than every final.
After publishing, the deployment scripts pick the release up only when a `chore` commit of `tn-node-deployment` moves its pin, which waits for a green `verify-release` ([After publish](#after-publish)).

### The required checks

`TN_REQUIRED_CHECKS` in `release.conf` names the ten checks a tag's commit must have passed, the required checks of `main` ([`.github/ACTIONS.md`](../.github/ACTIONS.md#required-checks)): `fmt`, `clippy`, `test`, `coverage`, `deny`, `audit`, `release-dry-run`, `nft-real (ubuntu-24.04)`, `nft-real (ubuntu-22.04)` and `nft-real (debian:13)`.
For each name the newest check run on the commit counts, whatever started it, and it must have concluded `success`.
A run concluded `skipped` or `neutral` checked nothing, so it is passed over, and a name with only such runs counts as having none.
A name with no run, a run still queued or in progress, and a failed, cancelled or timed-out run all stop the step with exit 1, and the error lists each one with its state.
Branch protection does not ensure this alone: a pull request's checks ran on its head, not on the commit its merge leaves on `main`, an admin can merge past them, and a red run on a push to `main` gates nothing.

That makes the order matter on a release day.
`tag` signs `main`'s tip, and that commit has check runs only once its push to `main` has run `ci.yaml` and `nft-real.yaml`, which takes several minutes; `gh run list --commit "$(git rev-parse origin/main)"` shows those runs.
`tag` asks GitHub before it creates the tag and refuses while any of the ten is missing or unfinished, so wait until the push runs are done and green, then run it again.
On a host without `gh`, `tag` leaves the question to `validate-tag`, which then fails and drafts nothing, so wait in either case.
Merge nothing else between the release PR and the tag, since a new tip starts its checks over.
A job that failed for a transient cause, such as a download, is re-run on GitHub (`gh run rerun <run-id> --failed`), and its new run becomes the newest of its name.
`scheduled.yaml` runs `cargo deny` and `cargo audit` on `main`'s tip every Monday as `deny-weekly` and `audit-weekly`, names that are not required checks, so their runs neither pass nor stop a release step.
When one turns red, an advisory has been published against a dependency since the push run: a re-run of the tagged commit's `deny` or `audit` would fail as well and stop the steps that follow, and an advisory that stays needs a fix on `main` and the next patch version.

### The CI rebuild

`build`'s two builds and `verify`'s rebuild run on the build server, so without a check from elsewhere the build server alone would decide what is signed and published.
The release workflow's `rebuild` job adds that check: on the push of the tag it builds the tagged commit on a GitHub runner with `etc/ci-rebuild.sh`, the same build `verify` runs, with the tag's `etc/Dockerfile` and `BUILD_IMAGE`.
It shows the program's sha256 in its step summary and uploads it as `program.sha256`, the one line `<sha256>  tn-p2p-allow-sync`, in the artifact `rebuild-vX.Y.Z`.
While the release is a draft, `sign`, `verify` and `publish` take the newest run of `release.yaml` for the push of the tag at its commit, download that artifact and compare the draft's program with it:

- `GitHub has no run of release.yaml for the push of vX.Y.Z ...`, or `run <id> of release.yaml for vX.Y.Z is in_progress ...`, or `queued` (exit 2): the job has not finished yet; wait for it (`gh run watch <id>`) and run the step again.
- `run <id> of release.yaml for vX.Y.Z concluded failure ...` (exit 1): there is no rebuild to compare with.
  Read the run's log, fix the cause, which is usually a failed download, re-run the run's failed jobs (`gh run rerun <id> --failed`), and run the step again.
- `the draft's tn-p2p-allow-sync has sha256 <a>, but the CI rebuild of vX.Y.Z ... gave sha256 <b>` (exit 1): the program on the draft is not the one the tag builds.
  Sign nothing, publish nothing, and treat the build server as compromised until you know the cause.
  Build the tag on a machine you trust with `bash etc/ci-rebuild.sh vX.Y.Z <dir>`, which writes `<dir>/program.sha256` as the job does.
  If it agrees with the job, the server built something else: rebuild or replace the server, then run `build` again, which replaces the draft's files while nothing is signed.
  If it agrees with the server, the build gives different programs on different hosts; fix that on `main` and release the next patch version.

## After publish

`publish` makes the release public, and the release workflow's `verify-release` job then checks it once more on a GitHub runner, the rebuild from the tag included.
A failure there comes after the release is public, so it is an alarm, not a gate ([ACTIONS.md](../.github/ACTIONS.md#the-post-publish-check-is-detective-only)).
Operators get a release only once `tn-node-deployment` pins it, so the pin waits for a green `verify-release`:

1. Find the run: `gh run list --workflow release.yaml --event release` lists one for each published release, with its tag as the branch.
   A publish made with a `GITHUB_TOKEN` starts no workflow, so when the tag has no run, start one with `gh workflow run release.yaml --ref main -f tag=vX.Y.Z -f mode=verify` and find it with `--event workflow_dispatch` instead.
2. Wait for it to finish (`gh run watch <run-id>`).
   A run that failed on a download, exit 3 in its log, can be re-run with `gh run rerun <run-id>`; any other failure makes the release red, as below.
3. Once it is green, open the `tn-node-deployment` pull request whose `chore` commit moves `TN_P2P_ALLOW_VERSION` in `lib/common.sh` to the tag and `TN_P2P_ALLOW_TARBALL_SHA256` to the hash on the tarball's line of the signed `SHA256SUMS`, checked as [Verify a release by hand](installing.md#verify-a-release-by-hand) shows, and link the green run in it.

If `verify-release` is red, or `publish` ended with an error that sends you here because the release's assets or body changed while it was being published:

- do not pin the release, and close any `tn-node-deployment` pull request that already pins it;
- edit the release notes to say at the top that the release is untrusted and why (`gh release edit vX.Y.Z --notes-file <file>`), since an immutable release keeps its files;
- fix the cause and release the next patch version, which reaches operators through the usual pin;
- tell the operators which release is untrusted, as [Rotating or revoking a key](../release/docs/signing.md#rotating-or-revoking-a-key) asks after a compromise.

## Running from the laptop

`make release-<step>-remote TAG=vX.Y.Z REMOTE=host[:dir]` is run on the laptop that holds the YubiKey, for the steps `prep`, `tag`, `build`, `sign`, `verify` and `publish`.
It opens an ssh connection to the build server and runs `make release-<step> TAG=vX.Y.Z` there, in the server's checkout of this repository.
All the work happens on the server: Docker, `gh`, `git push` and, for `tag` and `sign`, the signature itself.
For `tag` and `sign` the connection also forwards the laptop's gpg-agent, and gpg on the server signs through it.
The YubiKey never leaves the laptop, and the PIN and the touch happen there.
Each signature is one `PKSIGN` request over the forwarded socket.
The laptop forwards the agent's extra socket, which gpg-agent serves in restricted mode: signatures and decryption, no key export and no control of the agent.
On the server that socket belongs to a GnuPG home of its own, so the server's own gpg-agent is never involved, and nothing starts an agent in that home: its socket is the forwarded one or none.

### Sign and publish on the laptop

A step run with a `-remote` target is the server's run: the server fetches the draft, runs every check and decides what reaches the forwarded agent.
The comparison with the CI rebuild ([The CI rebuild](#the-ci-rebuild)) protects against a compromised or broken build server only when it runs somewhere else, so run `sign` and `publish` with the plain targets on the laptop:

```sh
make release-sign TAG=vX.Y.Z
make release-publish TAG=vX.Y.Z
```

They fetch the draft and the `rebuild` artifact with the laptop's own `gh` login, from GitHub, not through the server.
Neither builds anything unless `TN_P2P_ALLOW_REBUILD=1` is set, but both run the program's `--version` in `BUILD_IMAGE`, so the laptop needs Docker as well as `gh`.
`publish` also refuses a `release.sh` or `release.conf` that differs from `main`'s, so pull `main` on the laptop and update the submodule first ([Setup](#setup)).
With `make release-sign-remote` or `make release-publish-remote` the server itself answers whether its program matches the CI rebuild, and for `sign` the forwarded agent signs whatever the server sends it: the touch confirms that a signature happens, not what it covers.
Keep the `-remote` targets for `prep`, `tag`, `build` and `verify`.

### What the server needs

- A checkout of this repository on `main`, pulled and without local changes, with the submodule at the commit `main` records (`git submodule update --init -- release`).
  Every step but `setup` checks this on the server before `make` runs: it fetches `main` from `origin` and stops with exit 2 when the checkout is on another branch or a detached HEAD, has changes to tracked files or a submodule at another commit, or is not at `origin/main`'s tip.
  The error names the fix, `git checkout main && git pull --ff-only`, `git stash`, or `git submodule update --init -- release`; untracked files do not count.
- Docker with buildx, and `cargo` for `prep`.
- A `gh` login with write access to the repository, for `build`, `sign` and `publish`; `verify` needs it too, to see the draft.
- ssh push access to `origin`, for `tag`.
- A login shell that reads POSIX syntax, such as bash, zsh or sh, because the remote command is one `cd ... && ...` line; fish does not parse it.
- The tools on the PATH of a command run over ssh, which reads no `.bashrc`: the step adds `~/.local/bin`, `~/.cargo/bin`, `/usr/local/bin`, `/snap/bin` and `/opt/homebrew/bin` when they exist, checks the step's tools before `make` and names a missing one with where it looked; `RELEASE_REMOTE_PATH=<dir>[:<dir>]`, given on the laptop, puts other directories first.
- GnuPG installed, and no gpg configuration.
  The `-remote` targets create the home `~/.gnupg-tn-release`, or the absolute path in `RELEASE_REMOTE_GNUPGHOME`, the first time they need it, and it holds only the public keys of `release/keys` and `no-autostart`.

The server does not need to know the signing key: the laptop resolves it and passes its fingerprint on as `RELEASE_GPG_KEY`.

### What the laptop needs

- The YubiKey and GnuPG, set up as `release/docs/yubikey-setup.md` describes, or the signing key in the laptop's own GnuPG.
- gpg-agent with its extra socket, which GnuPG 2.1.13 and later create; `gpgconf --list-dirs agent-extra-socket` prints its path.
- A graphical pinentry such as pinentry-mac, which the YubiKey setup installs, because a curses pinentry cannot open the server's terminal.
- ssh access to the server, and a checkout of this repository on a `main` that has the `-remote` targets, to run `make` from.
- For `sign` and `publish`, Docker and a `gh` login with write access to the repository ([Sign and publish on the laptop](#sign-and-publish-on-the-laptop)).
- The signing key in `RELEASE_GPG_KEY`, or in `git config user.signingkey` of that checkout.

### REMOTE

`REMOTE` is required, and there is no default host: a `-remote` target without it prints its usage and exits 2.
`REMOTE=host` names the ssh destination, an alias from `~/.ssh/config` or `user@host`.
A port or other ssh options belong in `~/.ssh/config` under that alias, since a `:` after the host starts the directory.
`REMOTE=host:dir` also names the checkout on the server, relative to the server's home unless it starts with `/`; a leading `~/` is accepted and means the same.
Without `:dir` the checkout is the directory under the server's home with the same name as the laptop's checkout, `tn-p2p-allow-sync` for a clone that kept the default name.
The host may hold only letters, digits and `_.@-`, and the directory only letters, digits and `_./-`; anything else stops the run with exit 2.

### Variables that travel to the server

Every `RELEASE_*` and `TN_P2P_ALLOW_*` variable exported on the laptop or given on the `make` command line is set for the step on the server, except `RELEASE_REMOTE_SSH`.
So `RELEASE_SUMS_SHA256=<hex> make release-sign-remote TAG=vX.Y.Z REMOTE=buildbox` and `make release-verify-remote TAG=vX.Y.Z REMOTE=buildbox TN_P2P_ALLOW_REBUILD=0` work as they do with the plain targets.
A value may hold only letters, digits and `_./:@%+=,-`; a variable with any other character stops the run with exit 2, and the error names it.
For `tag` and `sign`, `RELEASE_GPG_KEY` reaches the server as the fingerprint the laptop resolved, whatever form it had on the laptop.
`RELEASE_REMOTE_GNUPGHOME`, given on the laptop like the others, is an absolute path on the server, and its default, `$HOME/.gnupg-tn-release`, is under the server's home, not the laptop's.

### Checking a server

`make release-setup-remote REMOTE=buildbox` is the way to check a server before a release day.
It prints `==> 1/1 buildbox: the GnuPG home for the forwarded agent`, then the GnuPG home, the number of maintainer keys imported and the socket path gpg uses there, and ends with `==> done: buildbox is ready for release-tag-remote and release-sign-remote`.
It checks the ssh access, the checkout and the keys of its submodule, and it writes nothing on the server outside that GnuPG home and its socket directory.
It does not check Docker or the `gh` login; `gh auth status` on the server shows the login.

### The two signing commands

The YubiKey is needed twice on a release day, for the tag and for the signature over `SHA256SUMS`, and these are the two commands that use it, with `buildbox` standing for the server's ssh alias:

```sh
make release-tag-remote TAG=vX.Y.Z REMOTE=buildbox
make release-sign TAG=vX.Y.Z
```

Between them, the tag push starts the workflow that drafts and rebuilds the release, and `make release-build-remote TAG=vX.Y.Z REMOTE=buildbox` builds it; keep the `SHA256SUMS sha256:` line it prints last, because `sign` asks for it.
The second command is the plain `sign`, on the laptop ([Sign and publish on the laptop](#sign-and-publish-on-the-laptop)): it prints the Makefile's line for the step and goes straight to the `sign` prompts below.
`make release-sign-remote TAG=vX.Y.Z REMOTE=buildbox` would run the same step on the server through the forwarded agent, in the four stages below, as `tag` does.
Plug the YubiKey in before each command and leave it in until the command ends.
Each `-remote` command prints a first line such as `release-tag-remote: vX.Y.Z on buildbox, the step runs there and the YubiKey stays here`, then four stages, each headed by a `==>` line:

1. `==> 1/4 here: the signing key and gpg-agent`: the laptop finds the signing key, checks that its YubiKey answers when the key is on one, checks that gpg-agent answers and has its extra socket, and prints a line for each.
   There is nothing to do unless it stops, and then the error says what to fix, such as `plug the YubiKey in`.
2. `==> 2/4 buildbox: the GnuPG home for the forwarded agent`: a first ssh connection does what `make release-setup-remote` does and prints the same facts about the home.
   It stays open, idle, until the step is over, because the server keeps the socket directory under `/run/user/<uid>` only while a session is logged in there.
   There is nothing to do here either, apart from the ssh authentication.
3. `==> 3/4 buildbox: connecting with the agent forwarded to <socket>`: the laptop opens the second connection, with the agent forwarded to that socket and with a terminal, so the server's prompts appear in this terminal and you answer them here.
   Each connection authenticates on its own, with no `ControlMaster` sharing even when `~/.ssh/config` sets one, so a key passphrase or a second factor is asked twice per signing step and once per other step.
   Its reminder line is the rule for the rest of the run: enter the PIN when pinentry opens on the laptop, and touch the YubiKey only when it blinks, because a touch with nothing to sign types a one-time password and Enter into the focused window, and a prompt waiting in this terminal takes that as its answer and stops the step.
4. `==> 4/4 <hostname>: make release-<step> TAG=vX.Y.Z` comes from the server, under its own host name, which need not match the alias, once `etc/release-remote.sh` there has checked that the forwarded agent answers and holds the signing key.
   The server then runs that `make`, which prints its line for the step and runs `release.sh`; what `release.sh` asks of you depends on the step, as below.

For `tag`, `release.sh` checks the version and changelog rules and signs the tag on `main`'s tip.
On the laptop, pinentry asks for the PIN if this is the first signature since the YubiKey was plugged in, and the YubiKey blinks until you touch it.
`release.sh` then prints the tag, the commit and the signer and asks `Push vX.Y.Z to origin? Type yes to continue:`; type `yes`, and the server pushes the tag, which starts the workflow that drafts and rebuilds the release.

For `sign`, `release.sh` fetches the draft's files and re-checks them, the comparison with the CI rebuild included, then prints the tag, the commit, the signer, the signatures so far and the sha256 of the draft's `SHA256SUMS`.
Unless `RELEASE_SUMS_SHA256` is set, it asks `Paste the SHA256SUMS sha256 from the last line of the build step:`; paste the line `build` printed last, or its hex alone, and a value that does not match stops the step before anything is signed.
It then asks `Sign SHA256SUMS for vX.Y.Z? Type yes to continue:`, and after `yes` the PIN and the touch follow as for `tag`.
`release.sh` uploads the new `SHA256SUMS.asc` to the draft and prints `signed vX.Y.Z as @<handle>: <n>/<required> signatures`.
A line `gpg: problem with fast path key listing: Forbidden - ignored` from the server, in a `-remote` run, is gpg noting that the restricted socket refuses a key listing; it is harmless.

A `-remote` run that succeeds ends with `==> done: release-<step> vX.Y.Z on buildbox finished`.

### The other steps

`prep`, `build`, `verify` and `publish` forward no agent, and after the same first line they print two stages: `==> 1/2 buildbox: connecting` on the laptop, then the server's `==> 2/2 <hostname>: make release-<step> TAG=vX.Y.Z`, the Makefile's line for the step and the step's own output.
`build` takes several minutes and ends with the `SHA256SUMS sha256:` line to keep for `sign`, and `publish`, which belongs on the laptop, asks `Publish vX.Y.Z? Type yes to continue:`.
`prep` leaves its changes in the server's checkout, so run the `git switch`, `git commit` and `gh pr create` commands it prints there, in an ssh session of your own; the `tag` command it names last is the plain step, which `make release-tag-remote` runs from the laptop.
Once the release PR merges, switch that checkout back to `main` and pull it, then run `git submodule update --init -- release` as [Setup](#setup) says; until then every other step refuses that checkout.

### Exit codes

`make` exits 0 when the step finished and 2 whenever the run failed, as `make` does for any failed recipe; the code of what failed is in the last `error:` line.
`etc/release-remote.sh run` itself exits 255 when ssh failed, 2 for usage or a precondition on either machine, 1 when the forwarded agent does not hold the signing key, and otherwise with the step's own code from `release/docs/signing.md`, which the server's `make` has already turned into 2 when `release.sh` fails.
A failed step ends the run with `error: release-<step> vX.Y.Z on buildbox failed (exit N)`, a failed stage 2 with `error: setup on buildbox failed (exit N)`, and a failed ssh with `error: ssh to buildbox failed (exit 255)`.

### When a run fails

- ssh exits 255 with `remote port forwarding failed` when a live session holds the socket on the server, such as another `-remote` run on the same server account or an ssh session left open with the same forward; end it and run the command again.
  A socket nothing answers on does not get in the way, because stage 2 removes it, and the same error appears when the server's sshd forbids the forward (`AllowStreamLocalForwarding` in `sshd_config`).
- The socket directory under `/run/user/<uid>` belongs to the login session and is removed after the last one ends, so the socket path of an earlier day may not exist any more; stage 2 recreates it on every run and keeps its connection open until the step is over, and there is nothing to clean up by hand.
- A run interrupted with `^C` ends both connections and removes its temporary directory on the laptop; nothing is left to clean up.
- `gpg --card-status` on the server cannot see the YubiKey through the restricted socket, so the server notices a missing or locked YubiKey only when it asks for the signature; stage 1 checks the card on the laptop before anything connects.
- A missed touch fails the signature and stops the step; run the same command again.
- An error at the start of a connection says what to change, such as `pass REMOTE=buildbox:<path of the checkout>` when the directory is missing, or `pull main there first` when the checkout has no `etc/release-remote.sh`.
- `<host>:<dir> is on branch release/vX.Y.Z, not on main`, `is at <commit>, not at origin/main <commit>` or `has local changes to tracked files` (exit 2) means the server's checkout is not the clean `main` a step runs in ([What the server needs](#what-the-server-needs)); run the command the error names there and run the step again.

`etc/tests/release_dry_run.sh` rehearses this path with a stand-in for ssh, which runs the remote command on the same machine and links the fixture agent's extra socket where the forward would put it.
`RELEASE_REMOTE_SSH` names that stand-in, and the `-remote` targets refuse it unless `RELEASE_DRY_RUN=1` is set.

## Reproducibility

Unlike the node, this build is reproducible, and the release relies on it instead of an attestation.

- `build` uploads nothing unless two `--no-cache` builds of the tag gave the same binary, byte for byte.
- Before anything is signed, the `rebuild` job builds the tag once more on a GitHub runner, and `sign`, `verify` and `publish` refuse a draft whose binary differs from that build ([The CI rebuild](#the-ci-rebuild)).
- The build reads no git and no clock: the commit reaches the binary only as `TN_P2P_ALLOW_GIT_SHA` through `option_env!`, and `pack_tarball` sorts the members, fixes owner and group to 0 and the mtime to the commit time, and gzips without a name or a timestamp.
- `make release-verify` and the `verify-release` job rebuild the binary from the tag's commit and require the same file, so a release that was not built from its tag fails verification.
- Anyone can rebuild a tag with `etc/Dockerfile` and the pinned `BUILD_IMAGE` and compare the binary's sha256 with the one in the released tarball.

The full test suite, the real-kernel job included, is a required check on every pull request, and every step from `tag` to `publish` also requires it to have passed on the tagged commit itself ([The required checks](#the-required-checks)).

## Rehearsing

`bash etc/tests/release_dry_run.sh` runs every step on a copy of this repository with `RELEASE_DRY_RUN=1` and a throwaway key, so neither `gh` nor `git push` is called; CI runs it as the `release-dry-run` job.
The build in it is real: `build` builds the binary twice in `BUILD_IMAGE` and `verify` rebuilds it with `TN_P2P_ALLOW_REBUILD=1`, so the run also proves the build reproducible and needs Docker with buildx and the network; every check of the release files runs the static check and the five-line `--version` check in Docker as well.
A dry run calls no `gh`, so it skips the required checks and the comparison with the CI rebuild; the unwind check runs in it.
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
Operators' installs check signatures against `lib/release-keys/` of `tn-node-deployment`, not against this pointer, so a change to the keys reaches them only once that directory carries it too ([installing.md](installing.md#how-the-deployment-scripts-install-it)).
