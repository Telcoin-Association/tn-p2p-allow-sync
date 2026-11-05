# Security Policy

tn-p2p-allow-sync runs as root on every Telcoin Network validator and decides which peers may reach its P2P ports, so a flaw in it can expose a validator or cut it off from the committee.

## Reporting a vulnerability

Please report a vulnerability privately, never through a public GitHub issue, pull request or discussion.

Email security{{[@]}}telcoin<.>org, and include:

- a description of the vulnerability and its impact on a validator;
- the steps to reproduce it, with the tool's version (`tn-p2p-allow-sync --version`), the node version and the nft version;
- a proof of concept, if you have one.

We acknowledge a report within 48 hours and give an initial assessment within 5 business days.
We keep you informed while we fix it, and agree the disclosure timing with you once a fixed release is out.
We may tell validator operators before the disclosure, so they can update first.
Please do not disclose the details publicly before then; tell us if you would like to be credited.

A vulnerability in a dependency belongs to that dependency's maintainers; tell us as well when it affects this tool.
A vulnerability in the node itself belongs to [telcoin-network](https://github.com/Telcoin-Association/telcoin-network/blob/main/SECURITY.md).

## Supported versions

Only the newest release receives fixes.
Validators install the version that a release of tn-node-deployment pins, so a fix reaches them through a new tool release and a deployment release that pins it.

## Signed releases

Every release is a git tag signed by a maintainer, with a tarball of the static `x86_64-unknown-linux-musl` program, a `SHA256SUMS` file and `SHA256SUMS.asc`, the maintainers' detached signatures over `SHA256SUMS`.
The release tooling, the maintainer keys and the signing policy live in the [tn-release-signing](https://github.com/Telcoin-Association/tn-release-signing) repository, which this repository carries as the submodule `release/`.
The keys allowed to sign, the number of signatures a release needs and the co-signer policy are stated once, in [`release/KEYS.md`](release/KEYS.md), at the commit that `main` records for `release/`; `MIN_SIGNATURES` in [`release.conf`](release.conf) may require more than that, never fewer.
Every check reads the keys at that commit, never from the release tag, so a tag cannot bring its own key.

The build is reproducible: `make release-build` builds the program twice from the tag in an image pinned by digest (`etc/Dockerfile`, `BUILD_IMAGE` in `release.conf`) and uploads nothing unless both builds are the same file, and `make release-verify` and the release workflow rebuild it from the tag and compare it with the published one.
To check a release yourself, follow [Verify a release by hand](release/docs/signing.md#verify-a-release-by-hand), with `REPO=Telcoin-Association/tn-p2p-allow-sync` and the tarball name `release/release.sh parse <tag>` prints.
