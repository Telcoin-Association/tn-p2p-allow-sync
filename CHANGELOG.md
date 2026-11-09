# Changelog

Generated from commit messages by git-cliff when a release is cut (`make release-prep`); sections are never edited by hand.

## [Unreleased]

The commits on `main` since the newest release below; `make release-prep` writes them into the next release's section.

<!-- ANCHOR: releases -->
## [v0.1.1] - 2026-11-09
<!-- release-base: b7f1798357cc7d3d5f255ffbc6a80c9455c63a45 -->
### Features
- **check**: Report a dormant table as dormant, exit 1
### Bug fixes
- **release**: Name an expired or missing CI rebuild artifact
- **fsutil**: Check an input's type before opening it
- **sync**: Re-apply the program to a dormant table
- **nft**: Read a dormant table from its flags key
- **sync**: Re-apply the program when the table's listing cannot be read
- **nft**: Read the elements of the program's four sets only
- **log**: Route each run's closing line through Log::last
- **log**: Never drop a run's closing line under the 8-line cap
- **release-remote**: Refuse a step in a server checkout that is not main's clean tip
- **config**: Check a capture-floor input's type on the open file
- **config**: Read an IPv4-mapped network in P2P_ALLOW_FROM as IPv4
- **cli**: Strip trailing slashes from --state-dir and refuse . and ..
- **fsutil**: Refuse a state dir path that does not end in a name
- **fsutil**: Remove an empty directory left at a temporary name
- **fsutil**: Open state reads with O_NONBLOCK so a swapped-in FIFO cannot block
- **snapshot**: Require tracked_epoch, admission, admission.epoch and fallback keys
### Refactoring
- **fsutil**: Every std::fs open, rename and remove goes through fsutil
- **nft,rpc**: Expose the listing parser and the response reader for fuzzing
### Documentation
- **contract**: The status screen warns at once on a failed run or unit too
- The required checks are the jobs of the push-to-main runs
- **actions**: List rebuild among the upload-artifact users
- **contract**: The 65k load figure covers the measured range
- **sync**: Disambiguate the render link
- **contract**: The 65k-address program loads in about 0.3 s
- README names the weekly jobs of scheduled.yaml
- **make**: The make ci help line names its five gates and leaves coverage to CI
- Weekly deny and audit in scheduled.yaml, ten required checks, every draft step compares with the rebuild
- **contract**: Outputs the integration tests pinned: --version, epoch info, state.json, failed status write, live keys
- **contract**: Version examples at 0.1.1
- **contract**: Release layout, and the status and memory versions a newer build reads
- **contract**: IPv4-mapped networks in the config, trailing slash on --state-dir, non-blocking floor inputs
- **contract**: State dir path checks, opens by path, O_NONBLOCK reads, leftover directory
- **contract**: Stale path re-listing failure, closing log line never capped
- **contract**: Check reports a dormant table as dormant, exit 1
- **contract**: Dormant table re-applies, unreadable listing re-applies, unhealed hand edits, program size
- **contract**: Rpc-method-missing and --http.api, epoch-regressed clears by itself, check-node at-once list
- **contract**: Snapshot nullable keys, node prerequisites, caps and expected transients
- README lists the CI jobs as they are and the local recipes
- **installing**: The install's keyring is tn-node-deployment's lib/release-keys, and key changes update both
- **releasing**: After publish, the CI rebuild and required-check gates, sign and publish on the laptop
### Tests
- **status**: Unix_now reads the wall clock
- Kill the remaining mutation survivors in nft, rpc, snapshot and fsutil
- Kill the mutation survivors in fsutil, cli and log
- **release**: The server-checkout refusals of a remote step
- Point stale-path comments at the contract
- **status**: Compare --json keys in the binary's order
- Pin docs/contract.md to the binary's usage, program, keys, tokens and events
- **fuzz**: Cargo-fuzz crate with six targets
- **real-nft**: Shrink, IPv6 and static traffic, adjacent networks, syntax check, the binary's sync, dormant heal, 65k load
- **memory**: Seeded round-trip and invariant properties over random folds
- The stale path, local errors, unreadable and dormant listings through the binary
- Epoch-info best effort, output and status-write failures, exit codes through the binary
- Special files, the closing log line and --state-dir normalisation through the binary
- Seeded property tests for addresses, ordering, render and mapped networks
- **memory**: Canonical state.json golden and an unchanged run over it
- **nft**: Parse the nft 1.1.3, adjacent and single-port captures
- **release**: Stub the dry run's build with a small static ELF program
- **support**: Fake-nft knobs for unreadable, dormant and failing listings
- **log**: A capped run's stderr ends with its closing line
- **fixtures**: Capture nft 1.1.3 and dormant, adjacent, single-port listings
- **config**: Special files, symlinks and the size cap at config, floor and capture-floor inputs
- **support**: Add a seeded splitmix64 generator for property tests
- **snapshot**: Parse the node's own serialisation of the schema example
- **snapshot**: Cover record_multiaddrs truncation and faults in a worker swarm
- **snapshot**: Pin which v1 fields are required and which may be absent
### Build, CI and chores
- Make mutants runs the weekly mutation pass locally
- Validate-tag drops checks: read; comments describe the jobs-based tag gate
- **release**: The required-checks gate reads the jobs of the push-to-main workflow runs
- Pin the debian:13 image by digest
- **clippy**: Disallow the remaining path-following and tree-changing std::fs calls
- **release**: The required-checks gate counts only GitHub Actions check runs
- **coverage**: Read the baseline without mapfile
- Declare nft-real and nft-real-docker phony
- Raise the coverage baseline to 98.2 after the v0.1.1 tests
- **clippy**: Disallow std::fs and the remaining resolving net calls outside their owners
- The scheduled fuzz job seeds its corpora from the fixtures like make fuzz
- Make fuzz
- **release**: The required-checks gate ignores skipped runs and includes coverage
- Weekly deny and audit move to scheduled.yaml so ci.yaml leaves no skipped check runs
- LICENSE-MIT names Telcoin Association
- **release**: Compare the draft's program with a CI rebuild before sign and publish
- **release**: Require the required checks to have passed on a tag's commit
- **release**: Refuse a tag whose release build sets panic = "abort"
- **release**: Refuse a release program that is not statically linked
- Split the weekly mutants run into four shards
- List the gates in .github/ACTIONS.md as they are
- **release**: Rebuild the tag on a runner before anything is signed
- Run nft-real on Debian 13 and watch the modules that feed nft
- Weekly report-only mutation testing and fuzzing
- Gate total line coverage against etc/coverage-baseline
- Run deny and audit weekly and cancel superseded pull request runs
- Run the real-kernel nft test in a docker container without sudo
### Other
- Put the user's tool directories on the PATH of a step run over ssh
- Take ~/dir in REMOTE as dir under the server's home
- Run every release step on the build server from the laptop
- Tag and sign over ssh with the laptop's forwarded gpg-agent

## [v0.1.0] - 2026-11-05
<!-- release-base: cf52cbf1e8263dd888d947036dbb6e6d2ee85db1 -->
### Features
- Status --json and check
- Sync and render, status file, journald log
- **nft**: Runner and JSON listing parser
- **config**: Config, floor, capture-floor, state dir and lock
- **rpc**: Loopback JSON-RPC over HTTP/1.1
- **memory**: Per-key memory, TTL, two strikes, state.json
- **snapshot**: Tn_admissionSnapshot v1 model and validation
- **render**: Nft program, schema 1
- **addr**: Multiaddr, IP and CIDR parsing, canonical order
### Bug fixes
- **snapshot**: A tracked epoch behind the admission epoch is pool-unpublished
### Documentation
- Contract, operator and maintainer guides
### Tests
- Real-kernel nft job
### Build, CI and chores
- **release**: Signing submodule, reproducible build, workflow
- Fmt, clippy, test, deny, audit
- Skeleton, toolchain, lints, deny
