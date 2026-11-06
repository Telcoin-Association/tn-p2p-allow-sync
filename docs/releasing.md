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
| `make release-sign TAG=vX.Y.Z` | `sign` | laptop | re-checks the draft's files, compares the `SHA256SUMS` hash with the one `build` printed, and appends your signature to `SHA256SUMS.asc`; it rebuilds the binary only with `TN_P2P_ALLOW_REBUILD=1`, since an amd64 build under emulation on a Mac is slow |
| `make release-verify TAG=vX.Y.Z` | `verify` | build server, CI, operators | re-checks the tag, the assets, the signatures and the `--version` lines, and rebuilds the binary from the tag, since the target sets `TN_P2P_ALLOW_REBUILD=1` unless you pass `TN_P2P_ALLOW_REBUILD=0` |
| `make release-publish TAG=vX.Y.Z` | `publish` | build server | verifies, then publishes the draft after you confirm |
| `make release-<step>-remote TAG=vX.Y.Z REMOTE=host[:dir]` | `prep`, `tag`, `build`, `sign`, `verify` or `publish` | laptop | runs `make release-<step>` over ssh in the build server's checkout; for `tag` and `sign` it forwards the laptop's gpg-agent, so gpg on the server signs through it while the YubiKey stays in the laptop |
| `make release-setup-remote REMOTE=host[:dir]` | | laptop | creates the build server's GnuPG home for the forwarded agent, imports the keys of `release/keys` into it and shows it; `release-tag-remote` and `release-sign-remote` do this first, so run it alone to check a server |

The Makefile also has `make ci`, the five gates of the README, and `make nft-real`, the real-kernel test.

## Cutting a release

1. `make release-prep TAG=vX.Y.Z` on the build server, then open the PR `release: vX.Y.Z` it prints and merge it alone.
2. `make release-tag TAG=vX.Y.Z` on the laptop, or with the `-remote` target, one touch of the YubiKey.
3. The tag push runs `.github/workflows/release.yaml`: `validate-tag` runs `check-tag` and `notes`, and `draft-release` runs `draft`, the only job with `contents: write`.
4. `make release-build TAG=vX.Y.Z` on the build server; note the `SHA256SUMS sha256:` line it prints last.
5. `make release-sign TAG=vX.Y.Z` on the laptop of each signer, or with the `-remote` target, with that hash at the prompt or in `RELEASE_SUMS_SHA256`, one touch each.
6. `make release-verify TAG=vX.Y.Z`, then `make release-publish TAG=vX.Y.Z`, on the build server.
7. The workflow's `verify-release` job runs `verify` on the published release, with `TN_P2P_ALLOW_REBUILD=1` and a read-only token, as a detective check.

Every step a maintainer runs can also be run from the laptop with the `-remote` targets, which run it on the build server over ssh ([Running from the laptop](#running-from-the-laptop)).
`tag` and `sign` need the maintainer's YubiKey, so a maintainer runs them by hand, at the laptop that holds it.
A final version must be higher than every other final, and a release candidate's higher than every final.
After publishing, the deployment scripts pick the release up only when a `chore` commit of `tn-node-deployment` moves `TN_P2P_ALLOW_VERSION` and `TN_P2P_ALLOW_TARBALL_SHA256` in `lib/common.sh` to the new tag and the tarball hash ([installing.md](installing.md)).

## Running from the laptop

`make release-<step>-remote TAG=vX.Y.Z REMOTE=host[:dir]` is run on the laptop that holds the YubiKey, for the steps `prep`, `tag`, `build`, `sign`, `verify` and `publish`.
It opens an ssh connection to the build server and runs `make release-<step> TAG=vX.Y.Z` there, in the server's checkout of this repository.
All the work happens on the server: Docker, `gh`, `git push` and, for `tag` and `sign`, the signature itself.
For `tag` and `sign` the connection also forwards the laptop's gpg-agent, and gpg on the server signs through it.
The YubiKey never leaves the laptop, and the PIN and the touch happen there.
Each signature is one `PKSIGN` request over the forwarded socket.
The laptop forwards the agent's extra socket, which gpg-agent serves in restricted mode: signatures and decryption, no key export and no control of the agent.
On the server that socket belongs to a GnuPG home of its own, so the server's own gpg-agent is never involved, and nothing starts an agent in that home: its socket is the forwarded one or none.

### What the server needs

- A checkout of this repository on `main`, pulled and without local changes, with the submodule at the commit `main` records (`git submodule update --init -- release`).
- Docker with buildx, and `cargo` for `prep`.
- A `gh` login with write access to the repository, for `build`, `sign` and `publish`; `verify` needs it too, to see the draft.
- ssh push access to `origin`, for `tag`.
- A login shell that reads POSIX syntax, such as bash, zsh or sh, because the remote command is one `cd ... && ...` line; fish does not parse it.
- GnuPG installed, and no gpg configuration.
  The `-remote` targets create the home `~/.gnupg-tn-release`, or the absolute path in `RELEASE_REMOTE_GNUPGHOME`, the first time they need it, and it holds only the public keys of `release/keys` and `no-autostart`.

The server does not need to know the signing key: the laptop resolves it and passes its fingerprint on as `RELEASE_GPG_KEY`.

### What the laptop needs

- The YubiKey and GnuPG, set up as `release/docs/yubikey-setup.md` describes, or the signing key in the laptop's own GnuPG.
- gpg-agent with its extra socket, which GnuPG 2.1.13 and later create; `gpgconf --list-dirs agent-extra-socket` prints its path.
- A graphical pinentry such as pinentry-mac, which the YubiKey setup installs, because a curses pinentry cannot open the server's terminal.
- ssh access to the server, and a checkout of this repository on a `main` that has the `-remote` targets, to run `make` from.
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
make release-sign-remote TAG=vX.Y.Z REMOTE=buildbox
```

Between them, the tag push starts the workflow that drafts the release, and `make release-build-remote TAG=vX.Y.Z REMOTE=buildbox` builds it; keep the `SHA256SUMS sha256:` line it prints last, because `sign` asks for it.
Plug the YubiKey in before each command and leave it in until the command ends.
Each command prints a first line such as `release-sign-remote: vX.Y.Z on buildbox, the step runs there and the YubiKey stays here`, then four stages, each headed by a `==>` line:

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
`release.sh` then prints the tag, the commit and the signer and asks `Push vX.Y.Z to origin? Type yes to continue:`; type `yes`, and the server pushes the tag, which starts the workflow that drafts the release.

For `sign`, `release.sh` fetches the draft's files and re-checks them, then prints the tag, the commit, the signer, the signatures so far and the sha256 of the draft's `SHA256SUMS`.
Unless `RELEASE_SUMS_SHA256` is set, it asks `Paste the SHA256SUMS sha256 from the last line of the build step:`; paste the line `build` printed last, or its hex alone, and a value that does not match stops the step before anything is signed.
It then asks `Sign SHA256SUMS for vX.Y.Z? Type yes to continue:`, and after `yes` the PIN and the touch follow as for `tag`.
The server uploads the new `SHA256SUMS.asc` to the draft and prints `signed vX.Y.Z as @<handle>: <n>/<required> signatures`.
A line `gpg: problem with fast path key listing: Forbidden - ignored` from the server is gpg noting that the restricted socket refuses a key listing; it is harmless.

A run that succeeds ends with `==> done: release-<step> vX.Y.Z on buildbox finished`.

### The other steps

`prep`, `build`, `verify` and `publish` forward no agent, and after the same first line they print two stages: `==> 1/2 buildbox: connecting` on the laptop, then the server's `==> 2/2 <hostname>: make release-<step> TAG=vX.Y.Z`, the Makefile's line for the step and the step's own output.
`build` takes several minutes and ends with the `SHA256SUMS sha256:` line to keep for `sign`, and `publish` asks `Publish vX.Y.Z? Type yes to continue:`.
`prep` leaves its changes in the server's checkout, so run the `git switch`, `git commit` and `gh pr create` commands it prints there, in an ssh session of your own; the `tag` command it names last is the plain step, which `make release-tag-remote` runs from the laptop.
Once the release PR merges, switch that checkout back to `main` and pull it, then run `git submodule update --init -- release` as [Setup](#setup) says.

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

`etc/tests/release_dry_run.sh` rehearses this path with a stand-in for ssh, which runs the remote command on the same machine and links the fixture agent's extra socket where the forward would put it.
`RELEASE_REMOTE_SSH` names that stand-in, and the `-remote` targets refuse it unless `RELEASE_DRY_RUN=1` is set.

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
