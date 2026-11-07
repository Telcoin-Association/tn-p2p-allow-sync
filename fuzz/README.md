# Fuzzing

This directory is a [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) crate with one libFuzzer target for each parser that reads outside input. It is a cargo workspace of its own, with its own `Cargo.lock` and `deny.toml`, so the program's `--locked` builds and `cargo deny check` never see it, and the release build never compiles it.

| Target | What it reads | What it asserts beyond "no panic" |
| --- | --- | --- |
| `multiaddr` | `addr::classify_multiaddr` on any text | The IP prints and parses back to itself. `canonical_ip` drops exactly the unspecified, multicast and link-local addresses and turns `::ffff:a.b.c.d` into `a.b.c.d`. |
| `cidr_or_host` | `addr::parse_cidr_or_host` on any text | An accepted entry is masked and parses back from its `Display`. A mapped address with a prefix below 96 is refused. `unmapped` is right. |
| `config` | `config::load_config` and `load_floor` on any bytes | Loading twice gives the same result. An accepted config is well formed, and the config written back from it loads to the same settings. A floor renders back to its own bytes. |
| `snapshot` | `snapshot::parse` on any JSON, then `memory::apply`, `serialize`, `load_state` | An accepted snapshot folds into empty memory, which then holds exactly its IPs. `serialize(load_state(serialize(state)))` is byte-equal. A second fold changes nothing. |
| `nft_listing` | `nft::parse_listing` on any bytes | Every refusal is `NftError::Parse`. Only the program's four sets are read, sorted and well formed. |
| `http_response` | `rpc::read_response` over a cursor of any bytes (64 KiB at most) | The body stays within 8 MiB and the head within 16 KiB. A chunked body equals the target's own de-chunking, and any other body is the bytes after the head. |

Each file in `fuzz_targets/` starts with its full list of oracles. A panic in an oracle is a finding just like a panic in the code it checks. `config`, `snapshot` and `http_response` also read each input in a second, structured form, so they reach their accepting paths without a corpus.

## Setup

cargo-fuzz needs a nightly compiler. The date is pinned in `FUZZ_TOOLCHAIN` of the `fuzz` job in `.github/workflows/scheduled.yaml` and in the Makefile:

```sh
rustup toolchain install nightly-2026-10-15 --profile minimal
cargo +nightly-2026-10-15 install --locked cargo-fuzz@0.13.2
```

`cargo +nightly-2026-10-15` overrides the stable pin of `rust-toolchain.toml`. Every command below runs in this directory.

## Running

```sh
cargo +nightly-2026-10-15 fuzz list                  # the six targets
cargo +nightly-2026-10-15 fuzz run snapshot          # until stopped or a crash
cargo +nightly-2026-10-15 fuzz run snapshot -- -max_total_time=300
```

`make fuzz` at the root runs all six for `FUZZ_SECONDS` each (default 300), the way the weekly job does. It first copies `tests/fixtures/snapshot/*.json` and `tests/fixtures/nft/*.json` into the `snapshot` and `nft_listing` corpora, which gives those targets real inputs to start from.

The corpus grows in `corpus/<target>/` and is not committed. Add `-- -jobs=8 -workers=8` to run several processes at once.

`cargo fuzz` builds with AddressSanitizer by default, as the weekly job does. The crate forbids `unsafe`, so ASan mostly guards the dependencies, and it costs a lot here: `snapshot` and `http_response` run 20 to 90 times slower with it (`http_response` reserves the 8 MiB body buffer on each read). For a long local run, `-s none` (`--sanitizer none`) gives the speed back.

## A crash

libFuzzer stops at the first failing input and writes it to `artifacts/<target>/crash-<sha1>` (or `oom-…`, `timeout-…`). The weekly job uploads that directory as the `fuzz-artifacts` artifact.

Reproduce, then minimise:

```sh
cargo +nightly-2026-10-15 fuzz run <target> artifacts/<target>/crash-<sha1>
cargo +nightly-2026-10-15 fuzz tmin <target> artifacts/<target>/crash-<sha1>
```

`tmin` writes the smallest input that still fails as `artifacts/<target>/minimized-from-<sha1>`. The run prints the panic message and its `file:line`, and `RUST_BACKTRACE=1` adds a backtrace.

Turn it into a test:

1. Keep the minimised input as `regressions/<target>/<short-name>`. That directory is committed, and the target replays it with `cargo fuzz run <target> regressions/<target>/<short-name>`.
2. Add a unit test to the `mod tests` of the module the target calls (`src/addr.rs`, `src/config.rs`, `src/snapshot.rs`, `src/memory.rs`, `src/nft.rs` or `src/rpc.rs`). Write the input there as a byte-string literal and make the same call and assertion the target makes. The root crate's tests are std-only and never depend on this directory.
3. Fix the code until both the unit test and `cargo fuzz run <target> regressions/<target>/` pass.

## Dependencies

`libfuzzer-sys` is pinned to an exact version. It builds libFuzzer's C++ sources, so a bump is a deliberate edit of `Cargo.toml` and `Cargo.lock`. The shared crates (`serde`, `serde_json` and their dependencies) are locked at the versions in the root `Cargo.lock`.

Check the graph the way the weekly job does:

```sh
cargo +nightly-2026-10-15 deny --config deny.toml check
```

`deny.toml` is the root one with one more license, NCSA, which libFuzzer's LLVM sources in `libfuzzer-sys` carry.
