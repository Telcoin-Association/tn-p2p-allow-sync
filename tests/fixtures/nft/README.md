# nft fixtures

Real output of `nft`, captured in containers, for the unit tests of `src/nft.rs`.
Nothing here was written by hand except `fake-nft.sh`, this file and the inputs under `capture/`.

## Versions

| Image | Digest | `nft --version` |
|---|---|---|
| `ubuntu:24.04` | `sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852` | `nftables v1.0.9 (Old Doc Yak #3)` |
| `ubuntu:22.04` | `sha256:076e7bf82319619f28dd0aa9c02ab34b055ad37100395b4f49a2ffba04fb5e11` | `nftables v1.0.2 (Lester Gooch)` |

Both print `"json_schema_version": 1` in their `metainfo` object.
Captured on 2026-11-05 with Docker 29.8.0 on a Linux 7.0 host kernel.

## How they were made

`--cap-add NET_ADMIN` gives the container its own network namespace in which nft may load tables, so no host root is needed.
With `W` the absolute path of the checkout:

```
docker run --rm --cap-add NET_ADMIN -e DEBIAN_FRONTEND=noninteractive -v "$W:/w" ubuntu:24.04 \
  sh -c 'apt-get -qq update && apt-get -qq install -y --no-install-recommends nftables iproute2 >/dev/null \
         && sh /w/tests/fixtures/nft/capture/capture.sh /w/target/nft-capture/24.04'
```

The same with `ubuntu:22.04`.
`capture.sh` writes `<case>.out`, `<case>.err` and `<case>.rc` for every call it makes; a second run reproduced every file byte for byte.
Files here are named `ubuntu-<release>-<nft version>-<case>` and copied from `<case>.out` (`.json`) or `<case>.err` (`.stderr`) unchanged.

## Cases

| Case | Call | Program loaded first |
|---|---|---|
| `program` | `nft -j list table inet tn_p2p_allow` | `capture/program.nft`, the reference program the renderer emits, byte for byte: two `allow_v4` addresses, one `static_v4` CIDR |
| `ipv6` | the same | the reference table with only `allow_v6` (two addresses) and `static_v6` (one CIDR) filled |
| `empty` | the same | the reference table with every set empty |
| `traffic` | the same | one `allow_v4` address on `lo`; three datagrams from it and two from `127.0.0.1` to the P2P ports |
| `forms` | the same | every element form nft prints: bare addresses, `prefix`, `range`, and `elem` objects around an address and a prefix (element comments force that form) |
| `tables-none` | `nft -j list tables inet` | nothing |
| `tables-other` | the same | `table inet filter` only |
| `tables-present` | the same | `capture/program.nft` only |
| `tables-both` | the same | `capture/program.nft` and `table inet filter` |
| `absent` | `nft -j list table inet tn_p2p_allow` | nothing; exit 1 |
| `eperm-list` | `nft -j list tables inet` as uid 65534 | exit 1 |
| `eperm-load` | `nft -f capture/program.nft` as uid 65534 | exit 1 |
| `syntax-error` | `nft -c -f -` given `table inet tn_p2p_allow { bogus }` | exit 1 |

## What the captures show

- Both versions print the same JSON for the same table, except for rule handles and the `metainfo` object.
- An empty set has no `elem` key at all.
- nft prints `::192.0.2.1` where Rust prints `::c000:201`, so the parser compares parsed addresses, never text.
- Under EPERM, nft 1.0.9 prints `Operation not permitted` for every call, but nft 1.0.2 does so only for the `list` calls: its `-f` and `-c -f -` exit 1 with nothing on stderr (`ubuntu-22.04-1.0.2-eperm-load.stderr` is empty on purpose).
  Running as root without `CAP_NET_ADMIN` gives the same bytes as running as uid 65534.
- Loading the same program again kept both named counters and the five rules of chain `input` on both versions (`traffic.out` against `traffic-reloaded.out` of a capture run; only the first is kept here).

## fake-nft.sh

A shell stand-in for nft that the runner tests point `Nft::new` at.
It answers the two `-j list` calls with the 1.0.9 `tables-present` and `program` captures, reads all of stdin for `-c -f -` before echoing it, prints its argv, stdin and exact environment for `argv`, and exits 64 on any call it does not know.
