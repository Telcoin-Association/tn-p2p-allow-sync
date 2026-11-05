# Maintainer targets: the CI gates, the real-kernel test and the release steps.
# Every target runs the command CI or the release procedure runs, so a local pass means a CI
# pass (.github/ACTIONS.md) and a release step behaves the same from make or by hand.

.PHONY: help ci nft-real release-prep release-tag release-check-tag release-notes release-draft release-build release-sign release-verify release-publish

.DEFAULT_GOAL := help

help:
	@echo "make ci                        the five gates of ci.yaml: fmt, clippy, test, deny, audit"
	@echo "make nft-real                  the real-kernel nft test (tests/real_nft.rs) under sudo"
	@echo "make release-<step> TAG=vX.Y.Z[-rcN]"
	@echo "                               one step of release/release.sh: prep, tag, check-tag, notes,"
	@echo "                               draft, build, sign, verify or publish (release/docs/signing.md)"

# the gates as CI runs them, warnings denied for every compile as in ci.yaml
ci: export RUSTFLAGS = -D warnings
ci:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo test --locked
	cargo deny check
	cargo audit

# the test binary is built as the invoking user and only that binary runs as root, as in
# nft-real.yaml; TN_REAL_NFT_REQUIRED=1 turns a skip (no root, nft or ip) into a failure
nft-real:
	cargo test --locked --no-run --test real_nft
	bin=$$(cargo test --locked --no-run --test real_nft --message-format=json | \
		jq -r 'select(.reason == "compiler-artifact" and .target.name == "real_nft" and .profile.test) | .executable') && \
	test -x "$$bin" && \
	TN_REAL_NFT_REQUIRED=1 sudo -E "$$bin" --ignored --exact real_kernel_nft --nocapture --test-threads=1

# verify rebuilds the program from the tag and compares it with the release's, unless
# TN_P2P_ALLOW_REBUILD=0 is given; sign rebuilds only with TN_P2P_ALLOW_REBUILD=1
release-verify: export TN_P2P_ALLOW_REBUILD ?= 1

# release pipeline: every step runs the release script of the signing submodule, which
# parses and checks TAG
release-prep release-tag release-check-tag release-notes release-draft release-build release-sign release-verify release-publish:
	@test -n "$(TAG)" || { echo "usage: make $@ TAG=vX.Y.Z[-rcN]"; exit 2; }
	@test -f release/release.sh || { echo "release/ is not checked out: run git submodule update --init -- release"; exit 2; }
	./release/release.sh $(patsubst release-%,%,$@) "$(TAG)"
