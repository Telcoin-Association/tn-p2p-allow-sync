# Installing

A validator gets `tn-p2p-allow-sync` from a signed GitHub release, through the deployment scripts of `tn-node-deployment`.
Nothing else is supported: the binary runs as root on every validator, so it changes only through a reviewed, signed scripts release.
This page describes that path and how an operator verifies a release by hand.

## Release assets

Every release `vX.Y.Z` (or `vX.Y.Z-rcN`) of `Telcoin-Association/tn-p2p-allow-sync` carries three assets:

| Asset | What it is |
|---|---|
| `tn-p2p-allow-sync-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the tarball: one directory of the same name, without `.tar.gz`, holding `tn-p2p-allow-sync`, `LICENSE-APACHE`, `LICENSE-MIT` and `NOTICE` |
| `SHA256SUMS` | one line, the sha256 of the tarball |
| `SHA256SUMS.asc` | the maintainers' detached signatures over `SHA256SUMS` |

The binary is static (`x86_64-unknown-linux-musl`), so it runs unchanged on Ubuntu 22.04 and 24.04 and ignores `LD_PRELOAD` and NSS modules.
There is no aarch64 asset until the node ships one.

## How the deployment scripts install it

`lib/common.sh` pins the release: `TN_P2P_ALLOW_VERSION` names the tag and `TN_P2P_ALLOW_TARBALL_SHA256` the sha256 of its tarball.
A `chore` commit of the scripts moves both; the scripts never fetch "latest", and with no hash pinned the install refuses.

`firewall-setup.sh --p2p-committee on` and `--p2p-committee sync` install the pinned release when the binary is missing or the first line of its `--version` differs from the pin.
`tn_p2p_allow_install` then:

1. Downloads the three assets from `https://github.com/Telcoin-Association/tn-p2p-allow-sync/releases/download/<tag>/` into an empty, root-owned 0700 staging dir, over HTTPS only (`curl --proto '=https' --tlsv1.2`) with a time and size cap.
2. Checks `SHA256SUMS.asc` over `SHA256SUMS` with `gpgv` against a keyring built from `lib/release-keys/*.asc`, the maintainer keys that sign the node's releases too, and needs a good signature from one of them.
3. Checks the tarball's sha256 against its line in `SHA256SUMS` and against `TN_P2P_ALLOW_TARBALL_SHA256`.
4. Checks the tarball's member list, then extracts it.
5. Runs `tn-p2p-allow-sync --version` as the unprivileged `tn-build` account and requires the first line to be `tn-p2p-allow-sync` and the pinned version without its `v`, a `config-version 1` line, and a `snapshot-versions` line that contains 1.
6. Installs the binary with `tn_install_root_binary` at `/usr/local/libexec/telcoin/tn-p2p-allow-sync`, owned by root, mode 0755.

A failure at any step leaves the installed binary, if any, untouched.
No version file is written: the installed binary's own `--version` is the record that `on` and `sync` compare with the pin.

`on` then creates `/var/lib/telcoin-p2p-allow` as root with mode 0700, writes its `config`, runs `capture-floor` on `/etc/telcoin/bootstrap-peers.yaml` (or the genesis `committee.yaml` when there is no peers file), and runs `sync --require-authoritative --wait 30`.
On exit 1 it refuses with nothing changed and removes the state dir again; otherwise it writes the units and enables them.
[contract.md](contract.md) describes every input and output the scripts and the binary exchange.

## Units

| Unit | Runs | Key settings |
|---|---|---|
| `telcoin-p2p-allow.service` | at boot, before the node | `Type=oneshot`, `RemainAfterExit=yes`, `ConditionPathExists=/var/lib/telcoin-p2p-allow/current.nft`, `ExecStart=<nft> -f /var/lib/telcoin-p2p-allow/current.nft`, `ExecStop=-<nft> delete table inet tn_p2p_allow`, `CapabilityBoundingSet=CAP_NET_ADMIN`, `IPAddressDeny=any` |
| `telcoin-p2p-allow-sync.service` | from the timer and on every apply | `ExecStart=/usr/local/libexec/telcoin/tn-p2p-allow-sync sync`, `SuccessExitStatus=3 4`, `UMask=0077`, `TimeoutStartSec=90`, `ReadWritePaths=/var/lib/telcoin-p2p-allow` with `ReadOnlyPaths=` on its `config` and `floor`, `InaccessiblePaths=/etc/telcoin` plus the data dir, `AmbientCapabilities=CAP_NET_ADMIN`, `IPAddressAllow=localhost`, `MemoryDenyWriteExecute=yes`, `MemoryMax=128M` |
| `telcoin-p2p-allow-sync.timer` | every 5 minutes | `OnBootSec=2min`, `OnUnitActiveSec=5min`, `RandomizedDelaySec=30s` |

The boot unit is a no-op until the first sync has written `current.nft`.
The sync unit treats a stale run (exit 3) and a busy one (exit 4) as success, so a node restart never leaves a failed unit.
`firewall-setup.sh --p2p-committee off` disables the three units, deletes the table and removes the state dir under the state-dir lock, and never runs the binary.

## Verify a release by hand

Check a release before extracting or running anything from it.
The commands take the maintainer keys from the signing submodule at the commit this repository's `main` records for `release/`, never from the release's tag, so a tag cannot bring its own key.
They clone `main` with only the `release` submodule, build a keyring from `release/keys/`, download the three assets, stopping at the first download that fails, check the signatures over `SHA256SUMS`, and check the hash.

Run them in one shell in a new, empty directory; the first command stops on a directory that is not empty, so no file from an earlier download is checked in place of this release's.
Set `TAG` first.

```sh
TAG="<tag>"
REPO="Telcoin-Association/tn-p2p-allow-sync"
TARBALL="tn-p2p-allow-sync-$TAG-x86_64-unknown-linux-musl.tar.gz"
BASE="https://github.com/$REPO/releases/download/$TAG"
if [ -n "$(ls -A)" ]; then echo "Run these commands in a new, empty directory" >&2; false; fi &&
git clone --quiet --depth 1 --branch main --recurse-submodules=release --shallow-submodules "https://github.com/$REPO.git" main &&
for k in main/release/keys/*.asc; do gpg --dearmor < "$k"; done > release-keys.gpg &&
gpg --show-keys --with-fingerprint main/release/keys/*.asc &&
curl -fsSL --fail-early --remote-name-all "$BASE/SHA256SUMS" "$BASE/SHA256SUMS.asc" "$BASE/$TARBALL" &&
gpgv --status-fd 1 --keyring ./release-keys.gpg SHA256SUMS.asc SHA256SUMS |
awk '$2 ~ /^(BADSIG|ERRSIG|EXPSIG|EXPKEYSIG|REVKEYSIG)$/ { bad = 1 } $2 ~ /^(GOODSIG|BADSIG|ERRSIG|EXPSIG|EXPKEYSIG|REVKEYSIG)$/ { s = $2; print $2 } $2 == "VALIDSIG" && s == "GOODSIG" { print "signed by primary key " $NF; n++ } END { exit (n && !bad) ? 0 : 1 }' &&
sha256sum --check SHA256SUMS &&
echo "Signature and hashes verified"
```

If the last line `Signature and hashes verified` is missing, the files are not verified; stop, do not extract or run anything from this release, and report it.
gpgv prints `Good signature from` and exits 0 even when the signing key is revoked or expired, so the block reads gpgv's status lines instead: it counts only a `GOODSIG`, and it fails when any signature in `SHA256SUMS.asc` is `BADSIG`, `ERRSIG`, `EXPSIG`, `EXPKEYSIG` or `REVKEYSIG`, even if another one is good.
Then check the fingerprints: every primary key fingerprint `gpg --show-keys` printed, including the one after `signed by primary key`, must appear in the table in `main/release/KEYS.md` and at `https://github.com/<handle>.gpg`, where `<handle>` is the key file's name without `.asc`.
Stop on any mismatch.
`git -C main submodule status -- release` prints the commit of the signing repository the keys came from.

With the release verified, check it against the pin of the scripts you run and look inside it, in the same directory:

```sh
grep -E '^readonly TN_P2P_ALLOW_(VERSION|TARBALL_SHA256)=' <path to tn-node-deployment>/lib/common.sh &&
sha256sum "$TARBALL" &&
tar -tzf "$TARBALL" &&
tar -xzf "$TARBALL" &&
"${TARBALL%.tar.gz}/tn-p2p-allow-sync" --version
```

- The tag and the sha256 must equal the two pinned values.
- `tar -tzf` lists the directory `tn-p2p-allow-sync-<tag>-x86_64-unknown-linux-musl/` and its four files, `LICENSE-APACHE`, `LICENSE-MIT`, `NOTICE` and `tn-p2p-allow-sync`, and nothing else.
- `--version` prints five lines: `tn-p2p-allow-sync` and the tag without its `v`, `commit` and the 40-hex commit the tag names, `target x86_64-unknown-linux-musl`, `snapshot-versions 1` and `config-version 1`.

`--version` needs no privileges and touches nothing.
Leave the install to `firewall-setup.sh --p2p-committee sync`, which repeats every check above against its own keyring.
