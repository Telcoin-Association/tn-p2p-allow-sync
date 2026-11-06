# Maintainer targets: the CI gates, the real-kernel test and the release steps.
# Every target runs the command CI or the release procedure runs, so a local pass means a CI
# pass (.github/ACTIONS.md) and a release step behaves the same from make or by hand.

.PHONY: help ci nft-real release-prep release-tag release-check-tag release-notes release-draft release-build release-sign release-verify release-publish release-setup-remote release-prep-remote release-tag-remote release-build-remote release-sign-remote release-verify-remote release-publish-remote

.DEFAULT_GOAL := help

help:
	@echo "make ci                        the five gates of ci.yaml: fmt, clippy, test, deny, audit"
	@echo "make nft-real                  the real-kernel nft test (tests/real_nft.rs) under sudo"
	@echo "make release-<step> TAG=vX.Y.Z[-rcN]"
	@echo "                               one step of release/release.sh: prep, tag, check-tag, notes,"
	@echo "                               draft, build, sign, verify or publish (release/docs/signing.md)"
	@echo "make release-<step>-remote TAG=vX.Y.Z[-rcN] REMOTE=host[:dir]"
	@echo "                               the same step, run over ssh on the build server at host, in the"
	@echo "                               checkout dir there (default: one named like this directory under"
	@echo "                               its home); tag and sign forward this machine's gpg-agent, so the"
	@echo "                               YubiKey stays here (docs/releasing.md, Running from the laptop)"
	@echo "make release-setup-remote REMOTE=host[:dir]"
	@echo "                               prepare the server's GnuPG home for the forwarded agent and show it;"
	@echo "                               tag and sign do this themselves, run it alone to check a server"

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

# what each step does and asks for, printed before it runs
release-prep: what = sets the version and the changelog for $(TAG) on this checkout of main, then prints the commands that open the release PR
release-tag: what = signs $(TAG) on main's tip with your key and pushes it: enter the PIN and touch the YubiKey when pinentry asks, then type yes to push
release-check-tag release-notes release-draft: what = a CI step, run by hand only to rehearse
release-build: what = builds $(TAG) twice in docker (several minutes) and uploads the tarball and SHA256SUMS to the draft: keep the SHA256SUMS sha256 line it prints last for the sign step
release-sign: what = fetches the draft's files, asks for the SHA256SUMS sha256 the build step printed and for a yes, then signs: enter the PIN and touch the YubiKey when pinentry asks
release-verify: what = re-checks the tag, the assets, the signatures and the --version lines, and rebuilds the program from the tag unless TN_P2P_ALLOW_REBUILD=0
release-publish: what = verifies the draft, then publishes it after you type yes

# release pipeline: every step runs the release script of the signing submodule, which
# parses and checks TAG
release-prep release-tag release-check-tag release-notes release-draft release-build release-sign release-verify release-publish:
	@test -n "$(TAG)" || { echo "usage: make $@ TAG=vX.Y.Z[-rcN]"; exit 2; }
	@test -f release/release.sh || { echo "release/ is not checked out: run git submodule update --init -- release"; exit 2; }
	@printf '==> %s %s: %s\n' '$@' '$(TAG)' "$(what)" >&2
	./release/release.sh $(patsubst release-%,%,$@) "$(TAG)"

# the same steps from the laptop that holds the YubiKey: etc/release-remote.sh runs the plain
# target over ssh in the checkout at REMOTE=host[:dir], so docker, gh and git push run on the
# server, and for tag and sign forwards this machine's restricted gpg-agent socket into a GnuPG
# home of its own there, so the key never leaves the laptop (docs/releasing.md, "Running from
# the laptop"); the script passes every RELEASE_* and TN_P2P_ALLOW_* variable on, among them
# RELEASE_REMOTE_GNUPGHOME, a path on the server
release-prep-remote release-tag-remote release-build-remote release-sign-remote release-verify-remote release-publish-remote:
	@test -n "$(TAG)" || { echo "usage: make $@ TAG=vX.Y.Z[-rcN] REMOTE=host[:dir]"; exit 2; }
	@test -n "$(REMOTE)" || { echo "usage: make $@ TAG=vX.Y.Z[-rcN] REMOTE=host[:dir]"; exit 2; }
	./etc/release-remote.sh run "$(REMOTE)" $(patsubst release-%-remote,%,$@) "$(TAG)"

release-setup-remote:
	@test -n "$(REMOTE)" || { echo "usage: make $@ REMOTE=host[:dir]"; exit 2; }
	./etc/release-remote.sh run "$(REMOTE)" setup
