# Changelog

Generated from commit messages by git-cliff when a release is cut (`make release-prep`); sections are never edited by hand.

## [Unreleased]

The commits on `main` since the newest release below; `make release-prep` writes them into the next release's section.

<!-- ANCHOR: releases -->
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
