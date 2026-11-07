# nft fixtures

Real output of `nft`, captured in containers, for the unit tests of `src/nft.rs`.
Nothing here was written by hand except `fake-nft.sh`, this file and the inputs under `capture/`.

## Versions

| Image | Digest | `nft --version` |
|---|---|---|
| `debian:13` | `sha256:9c6e381421d0f48d1b8133e573f33c185ee16cbf505eda9c4a8a4791b25ea8a6` | `nftables v1.1.3 (Commodore Bullmoose #4)` |
| `ubuntu:24.04` | `sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852` | `nftables v1.0.9 (Old Doc Yak #3)` |
| `ubuntu:22.04` | `sha256:076e7bf82319619f28dd0aa9c02ab34b055ad37100395b4f49a2ffba04fb5e11` | `nftables v1.0.2 (Lester Gooch)` |

All three print `"json_schema_version": 1` in their `metainfo` object.
The two Ubuntu sets were captured on 2026-11-05; the Debian set and the cases `dormant*`, `adjacent` and `single-port` on 2026-11-07.
Both days used Docker 29.8.0 on a Linux 7.0 host kernel, so every container ran against that one kernel and only the nft userspace differs.

## How they were made

`--cap-add NET_ADMIN` gives the container its own network namespace in which nft may load tables, so no host root is needed.
With `W` the absolute path of the checkout:

```
docker run --rm --cap-add NET_ADMIN -e DEBIAN_FRONTEND=noninteractive -v "$W:/w" ubuntu:24.04 \
  sh -c 'apt-get -qq update && apt-get -qq install -y --no-install-recommends nftables iproute2 >/dev/null \
         && sh /w/tests/fixtures/nft/capture/capture.sh /w/target/nft-capture/24.04'
```

The same with `ubuntu:22.04` (into `22.04`) and `debian:13` (into `13`).
`capture.sh` writes `<case>.out`, `<case>.err` and `<case>.rc` for every call it makes; a second run reproduced every file byte for byte on all three images.
The cases added on 2026-11-07 run after all the others, so the 2026-11-05 Ubuntu files, handles included, still come out byte for byte the same.
Files here are named `<distro>-<release>-<nft version>-<case>` and copied from `<case>.out` (`.json`) or `<case>.err` (`.stderr`) unchanged.
`eperm-list` and `eperm-load` come from `eperm-user-list` and `eperm-user-load`.

## Cases

Debian 13 has every case. The Ubuntu sets have every case too; the last six were added to them on 2026-11-07.

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
| `dormant` | `nft -j list table inet tn_p2p_allow` | `capture/program.nft`, one datagram from `127.0.0.1` to port 49590, then `nft add table inet tn_p2p_allow '{ flags dormant; }'` and two more datagrams |
| `dormant-tables` | `nft -j list tables inet` | the same dormant table |
| `dormant-reapplied` | `nft -j list table inet tn_p2p_allow` | the dormant table, then `capture/program.nft` loaded again and two more datagrams |
| `dormant-forms` | the same | `capture/forms.nft`, then the dormant flag |
| `adjacent` | the same | `capture/adjacent.nft`: two adjacent `/24`s and a third one apart in `static_v4`, two adjacent `/48`s and a third one apart in `static_v6` |
| `single-port` | the same | `capture/single-port.nft`, a copy of `tests/golden/single-port.nft`: the reference program with `P2P_PORTS` holding only 49590 |

## What the captures show

- All three versions print the same objects for the same table, except for rule handles, the `metainfo` object and the dormant flag below.
  nft 1.0.2 numbers the five rules one after another; 1.0.9 and 1.1.3 leave a gap between them, except in the single-port table.
- nft 1.1.3 prints exactly the objects 1.0.9 prints, every handle included, but in another order: the `chain` object comes right after the `table` object, before the counters and sets, where 1.0.x prints it after the sets.
  The rules still come last, in rule order.
- Element forms are the same on all three versions (`forms`).
- An empty set has no `elem` key at all.
- nft prints `::192.0.2.1` where Rust prints `::c000:201`, so the parser compares parsed addresses, never text.
- Under EPERM, nft 1.0.9 and 1.1.3 print `Operation not permitted` for every call, but nft 1.0.2 does so only for the `list` calls: its `-f` and `-c -f -` exit 1 with nothing on stderr (`ubuntu-22.04-1.0.2-eperm-load.stderr` is empty on purpose).
  Running as root without `CAP_NET_ADMIN` gives the same bytes as running as uid 65534.
  Every `.stderr` file of 1.1.3 is byte for byte the 1.0.9 one.
- Loading the same program again kept both named counters and the five rules of chain `input` on all three versions (`traffic.out` against `traffic-reloaded.out` of a capture run; only the first is kept here).
  The same holds for the adjacent and single-port tables: a reload lists the same objects with new rule handles.
- A table without flags has no `flags` key on any version.
  nft 1.1.3 prints a dormant table as `{"table": {"family": "inet", "name": "tn_p2p_allow", "handle": 13, "flags": "dormant"}}`, in `list table` and in `list tables` alike: a string, not an array, for the one flag.
- nft 1.0.2 and 1.0.9 print the `flags` key for a dormant table, but its value is some other string of the same output, which looks like a read of memory libnftables has already freed: `"inet"` or `"tn_accept_p2p_listed"` in `list table`, and a string from `metainfo` (`"Lester Gooch"`, `"Old Doc Yak #3"`, once `"1.0.2"`) in `list tables`.
  The value is the same on every run for the same table and call, and changes with the table's content.
  For some contents the output stops right after `"flags": ` and a newline, with exit status 0 and nothing on stderr: `ubuntu-24.04-1.0.9-dormant-forms.json` is cut off like that and is not valid JSON on purpose.
  A one-off run showed the same on 1.0.2 for the content of `capture/traffic.nft` without traffic.
  So on 1.0.x only the presence of the key says anything, and a dormant table can also make `list table` unreadable.
  The text listing (`dormant-text.out` of a capture run) prints `flags dormant` correctly on all three.
- Loading the program again wakes a dormant table on all three versions: the program re-declares the table without flags.
  The counters show it: `tn_drop_p2p_unlisted` is at 1 packet, 29 bytes in `dormant`, after one datagram before the flag and two while dormant, and at 3 packets, 87 bytes in `dormant-reapplied`, after two more.
  A one-off run on all three also showed that `nft add table inet tn_p2p_allow` alone wakes the table, that `nft -c -f -` of the program does not, and that `delete table` plus a reload wakes it with counters back at zero.
- Adjacent networks in an interval set stay separate on all three versions: `10.0.0.0/24` and `10.0.1.0/24` list as two `prefix` objects, not as `10.0.0.0/23` or a range, and so do `2001:db8:1::/48` and `2001:db8:2::/48`.
- A single port lists as a bare number, `{"match": {"op": "==", "left": {"payload": {"protocol": "udp", "field": "dport"}}, "right": 49590}}`, where two ports list as `"right": {"set": [49590, 49594]}`; the text form is `udp dport 49590` without braces.
- `nft -c -f -` reads all of stdin before it prints anything.
  In a one-off run on all three versions, a 2.75 MB program written into a pipe that was held open for 3 s after the write was read whole within 1.5 s, and stderr stayed empty until EOF.
  That held with the syntax error at the end, at the start, and with 20 errors at the start, where nft stops at 10 errors (30 lines of stderr) but only after EOF.

## fake-nft.sh

A shell stand-in for nft that the runner tests point `Nft::new` at.
It answers the two `-j list` calls with the 1.0.9 `tables-present` and `program` captures, reads all of stdin for `-c -f -` before echoing it, prints its argv, stdin and exact environment for `argv`, and exits 64 on any call it does not know.
