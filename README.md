# tn-p2p-allow-sync

`tn-p2p-allow-sync` keeps the host firewall of a Telcoin Network validator in step with the committee.
It reads the node's own view of who may connect from the loopback RPC method `tn_admissionSnapshot`, folds it into a small per-key memory, adds the operator's static list and the bootstrap-peers floor, and loads the result as one nftables table, `inet tn_p2p_allow`, in front of the node's QUIC ports.
UDP from a listed source to those ports is accepted and counted, UDP from any other source to them is counted and dropped, and all other traffic is untouched.
It is one static binary that owns fetch, validation, memory, floor, static list, rendering, apply, state, status and the consistency check; the deployment scripts keep the verbs, the systemd units, the ufw interplay, the kill switch and the download, verification and install of the binary.

The set follows the chain: every validator in the eligible pool (PendingActivation, Active and PendingExit) and in the committees of epochs E-1 through E+2, plus trusted hubs and bootstrap peers, so a validator that joins after genesis is allowed hours before its first seat and one that exits leaves once the node stops listing it.
The node does the resolving and the verifying (BLS-signed records, publisher equal to the record's network key), and the tool only applies what the node reports.
On bad input it fails open to the last good set: a snapshot that is not authoritative changes nothing, nothing ever flushes the sets on an error path, and the static list and the floor are always present.

## Command line

```text
usage: tn-p2p-allow-sync [--state-dir DIR] [--nft PATH] COMMAND
  sync [--require-authoritative] [--wait SECS]
  check
  status [--json]
  render [--snapshot FILE] [--check]
  capture-floor FILE...
  --version
```

| Command | What it does |
|---|---|
| `sync` | Fetches the snapshot, updates memory and loads the program, or keeps the last good one; prints one `result=…` line. |
| `sync --require-authoritative` | The same, but a snapshot that is not authoritative, or a dynamic set that is empty, is refused with exit 1 and nothing written. |
| `sync --wait SECS` | Waits up to `SECS` for the state-dir lock instead of exiting 4 at once. |
| `check` | Reports every current committee address the live table lacks, and every key whose signed record lists other addresses than the ones the node dials. |
| `status` | Prints the status file's keys and the live facts of the table. |
| `status --json` | Prints the same as one JSON object on one line. |
| `render` | Prints the program `sync` would load and writes nothing; `--snapshot FILE` reads the snapshot from a file, `--check` has nft check the program. |
| `capture-floor FILE...` | Writes the floor from the IP literals of the given files. |
| `--version` | Prints the version, the commit, the target, the snapshot versions and the config version. |

| Exit | Meaning |
|---|---|
| 0 | Fresh. `sync`: applied or already loaded. `check`: consistent. `status`, `render`, `capture-floor`: done. |
| 1 | Refused, nothing written, not even `status`. `sync --require-authoritative`: the snapshot is not authoritative or the dynamic set is empty. `check`: an address is missing or the table is absent. `render`: not authoritative. |
| 2 | Error: usage, config, floor or state invalid, unsafe state dir, nft missing or EPERM, apply failed. The stale-path protections still run where possible. |
| 3 | Stale. `sync`: last-good kept. `check`: cannot judge (RPC down, the snapshot not authoritative, or the last sync behind the snapshot: a newer epoch or an address it never held). |
| 4 | Busy: the lock is held. |
| 101 | Panic, a bug. |

`--state-dir` defaults to `/var/lib/telcoin-p2p-allow`, which must be owned by the effective uid with mode 0700; `--nft` defaults to `/usr/sbin/nft`.
The tool does not require root, but nft needs CAP_NET_ADMIN.
[docs/contract.md](docs/contract.md) has the rest: the config grammar, the status keys, the JSON object, every reason token and the program text.

## Build and test

The toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (Rust 1.94.1 with the `x86_64-unknown-linux-musl` target), and `cargo` picks it up inside the repository.

```sh
cargo build --locked --release --target x86_64-unknown-linux-musl
cargo run -- --version
```

CI runs these jobs; [.github/ACTIONS.md](.github/ACTIONS.md) has each one's command and what it stops:

- `ci.yaml`, on every pull request and every push to `main`: `fmt`, `clippy`, `test`, `coverage`, `deny`, `audit` and `release-dry-run`.
- `nft-real.yaml`, on every push to `main` and every pull request that touches the code it covers: the real-kernel test, on Ubuntu 24.04 and 22.04 runners and in a Debian 13 container.
- `scheduled.yaml`, weekly on `main`: `deny-weekly` and `audit-weekly` on Mondays, so a new advisory shows without a pull request, and report-only mutation testing and fuzzing on Saturdays.

Locally, `make ci` runs `fmt`, `clippy`, `test`, `deny` and `audit` as CI does, `make nft-real-docker DISTRO=<image>` runs the real-kernel test in a container of `ubuntu:24.04`, `ubuntu:22.04` or `debian:13` without sudo, and `RELEASE_BUILD_STUB=1` makes the release dry run quick by swapping its Docker build for a stub:

```sh
make ci
make nft-real-docker DISTRO=debian:13
RELEASE_BUILD_STUB=1 bash etc/tests/release_dry_run.sh
```

The tests run rootless: the integration tests drive the built binary against a fake JSON-RPC server and a fake nft (`tests/support/`), and `tests/golden/` pins the program text byte for byte.
The real-kernel test loads the program into nftables, sends traffic from a listed and an unlisted source and checks the counters.

## Documentation

- [docs/contract.md](docs/contract.md): the cross-repository contract, for the deployment scripts and the node team.
- [docs/installing.md](docs/installing.md): how the deployment scripts install the binary, and how to verify a release by hand.
- [docs/releasing.md](docs/releasing.md): how a maintainer cuts and signs a release.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option; see [NOTICE](NOTICE).
