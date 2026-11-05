# Contract

This page is the interface between `tn-p2p-allow-sync` and the two parties that depend on it.
The deployment scripts (`tn-node-deployment`, `firewall-setup.sh --p2p-committee` and `check-node.sh`) write its inputs, run its commands and read its outputs.
The node team serves the `tn_admissionSnapshot` RPC method whose result the tool reads.
Everything below describes what the code does; a change to any of it is a change to the contract and is versioned.

## Versions

| Item | Version | Where it shows | What a mismatch does |
|---|---|---|---|
| Config file | 1 | `CONFIG_VERSION=1` in `config`, `config-version 1` in `--version` | `config-invalid`, exit 2 |
| Snapshot | 1 | `version` in the `tn_admissionSnapshot` result, `snapshot-versions 1` in `--version` | `snapshot-version-<n>`, exit 3 |
| Status file | 1 | `STATUS_VERSION=1` in `status` | the file reads as the status before the first run |
| Memory file | 1 | `"version":1` in `state.json` | `state-invalid`, exit 2 |
| nft program | schema 1 | the header comment of `current.nft` | not read back; the text is fixed by this page |

### `--version`

`tn-p2p-allow-sync --version` prints five lines on stdout and exits 0:

```text
tn-p2p-allow-sync 0.1.0
commit 0123456789abcdef0123456789abcdef01234567
target x86_64-unknown-linux-musl
snapshot-versions 1
config-version 1
```

- Line 1 is the package name and the `Cargo.toml` version, without a `v`.
- Line 2 is the commit the release build passed in `TN_P2P_ALLOW_GIT_SHA`, 40 lowercase hex digits, or `unknown` for a build without it or with it set to the empty string.
  A build that sets the variable to anything else fails to compile.
- Line 3 is the target triple the binary was built for: `x86_64-unknown-linux-musl`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-musl` or `aarch64-unknown-linux-gnu`, and `unknown` for any other target; a release is always `x86_64-unknown-linux-musl`.
- Line 4 lists the snapshot versions the build accepts, separated by spaces.
- Line 5 is the config version the build reads.

The deployment scripts require line 1 to equal `tn-p2p-allow-sync` and the pinned tag without its `v`, the `config-version` line to equal the version they write, and the `snapshot-versions` line to contain 1.
The release checks require the first three lines.

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

- The global flags come before the command, and every flag appears at most once.
- Only the long forms above exist; anything else, `--help` included, prints the usage text on stderr and exits 2.
- A flag value or a `capture-floor` file that is empty or starts with `-` is a usage error, so a missing value never swallows the next flag; write `./-name` for such a file.
- `--wait` takes decimal digits only, in seconds.
- `--state-dir` defaults to `/var/lib/telcoin-p2p-allow`.
- `--nft` defaults to `/usr/sbin/nft`.
  It must be an absolute path: `sync`, `render`, `check` and `status` refuse a relative one with `error reason=usage` and exit 2.
  An absolute path other than the default is accepted with one `nft-path path=…` warning on every run.
  It is a flag and never an environment variable, so it shows in argv and in the journal; the units never pass it.

The tool does not require root.
It requires the state dir to be owned by its effective uid with mode 0700, and nft needs CAP_NET_ADMIN for every call, `render --check` included.
nft refusing a call for lack of privilege is exit 2 with `nft: operation not permitted, run as root`.

## Exit codes

| Exit | `sync` | `check` | `status` | `render` | `capture-floor` |
|---|---|---|---|---|---|
| 0 | applied, or already loaded | `ok` | printed | program printed | `floor` written |
| 1 | `--require-authoritative` only: any exit-3 reason, nothing written | `missing` or `absent` | never | a stale reason, no program | never |
| 2 | usage, local error, unsafe state dir, nft unavailable | usage, unsafe state dir, `config-invalid`, `nft-unavailable`, stdout failed | usage, unsafe state dir, stdout failed | usage, unsafe state dir, invalid input, `nft-unavailable` or `nft-check-failed` with `--check`, stdout failed | usage, unsafe state dir, unreadable input, write failed |
| 3 | stale, last-good program kept | `stale` or `behind`: cannot judge | never | never | never |
| 4 | busy: the lock is held | never | never | never | busy after 30 s |
| 101 | panic, a bug | panic | panic | panic | panic |

`--version` exits 0, or 2 when stdout cannot be written.

The sync unit lists `SuccessExitStatus=3 4`, so a stale or busy timer run is not a failed unit.
It also sets `TimeoutStartSec=90`: the tool bounds every RPC call but not the run time of nft, so the unit's timeout is what ends a run whose nft hangs while it holds the lock.
`status` exits 0 whenever the state dir passes its checks: a missing status file reads as the status before the first run, and nft failing to list the table leaves `LOADED` and the counter keys empty with an `nft-unavailable` warning.
`AGE_SECS` and `STALE` come from the file and the clock, so they are still computed then.

## Inputs

### The config file

The scripts write `<state-dir>/config` on every verb and every resync:

```text
# tn-p2p-allow-sync config, written by firewall-setup.sh
CONFIG_VERSION=1
RPC_PORT=8545
P2P_PORTS=49590,49594
P2P_ALLOW_FROM=203.0.113.0/24,198.51.100.9
```

Grammar, version 1:

- Lines end at `\n`; a final newline is optional.
- An empty line is skipped, and a line whose first byte is `#` is a comment.
- Every other line is printable ASCII (0x21 to 0x7e): no spaces, tabs, control bytes, non-ASCII bytes, `"` or `'`.
- Such a line is `KEY=VALUE`, split at the first `=`.
- Every line is checked against the grammar above first; then `CONFIG_VERSION` is checked before any other key and must be `1`, so a newer file reports its version rather than an unknown key.
- Each of the four keys appears exactly once; an unknown, duplicate or missing key refuses the whole file.
- `RPC_PORT` is a port: decimal digits, no sign, no leading zero, 1 to 65535.
  The RPC host is always `127.0.0.1` and has no key.
- `P2P_PORTS` is one or more ports separated by commas, each listed once.
  An empty value is refused, since the program would carry `udp dport {  }`, which nft rejects.
  The program lists the ports sorted.
- `P2P_ALLOW_FROM` is empty or a comma-separated list of IPv4 and IPv6 addresses and networks (`A` or `A/N`).
  A prefix is decimal without a sign or a leading zero (`/0` is fine), at most 32 or 128.
  A bad entry, a hostname included, refuses the whole file: dropping it would cut the static list and resolving it would be DNS.
- A host entry (an address, `/32` or `/128`) goes through the same canonicalisation as snapshot addresses: an IPv4-mapped IPv6 host becomes an IPv4 host, and an unspecified, multicast or link-local host refuses the file, naming the entry.
  Host entries go to the allow sets.
- A network entry is masked to its network address and goes to the static set of its family.
  A network that a wider network of the same family covers is dropped, since nft refuses overlapping intervals in one set; `sync` and `render` log one `static-covered dropped=N` warning per run.
- `0.0.0.0/0` and `::/0` are accepted as the operator's policy, and `sync` and `render` log one `static-catch-all` warning per run.

Every refusal is `config-invalid` and names the line at fault when there is one.
The file is never read through a symlink, and it must be a regular file: a symlink or a special file at `config` is `config-invalid`.
A wrong `RPC_PORT` fails open: the sync goes stale with `rpc-unreachable` and the loaded program stays.

### The floor file

`<state-dir>/floor` holds the bootstrap-peers floor:

```text
# floor 1
192.0.2.50
2001:db8::1
```

- The first line is exactly `# floor 1`.
- Every later line is one address as Rust's std prints it (IPv6 compressed per RFC 5952), canonical, IPv4 before IPv6 and ascending within a family, never repeated.
- A final newline is optional.
- Anything else is `floor-invalid`, exit 2, and so is a symlink or a special file at `floor`, which is never followed.
- A missing file is an empty floor.
  The setup always writes the file, so `sync` and `render` log one `floor-missing` warning per run while it is missing.

`capture-floor FILE...` writes it, as root and outside the sync unit's sandbox:

- Each file must be a regular file of at most 16 MiB; symlinks are followed, since the inputs live in `/etc/telcoin`.
  An unreadable, special or larger file is `floor-invalid` and nothing is written, because a cut input could drop a peer.
- No YAML or JSON parser is involved.
  Each file is split into runs of the bytes a multiaddr uses (letters, digits and `/.:-_%`), and each run goes through the snapshot's multiaddr parser.
  `/ip4/A/…` and `/ip6/A/…` yield `A` after canonicalisation; `/dns4/…` and everything else yield nothing.
- The floor is the union over every file; a file without an address adds nothing, and an empty result writes the header line alone.
- The inputs are read first, then the state-dir lock is taken with a fixed 30 s wait (busy is exit 4), then `floor` is written atomically with mode 0600.
- Nothing is printed on success; a failure is one stderr line `capture-floor: <token>: <detail>`.

The scripts pass `/etc/telcoin/bootstrap-peers.yaml`, or the genesis `committee.yaml` when no peers file exists.

### The snapshot

The tool calls `tn_admissionSnapshot` with params `[]` over HTTP/1.1 on `127.0.0.1:RPC_PORT`:

- `POST /` with `Host`, `Content-Type: application/json`, `Content-Length` and `Connection: close`.
- The response may end by `Content-Length`, by `chunked` encoding or at the end of the stream.
- Connecting has 2 s, each read 5 s, and the whole call 10 s, checked before every read.
- The response head is capped at 16 KiB and the decoded body at 8 MiB.
- The address is a literal, so no name is ever resolved.

The result is validated whole; the first fault in this order makes the whole snapshot stale (tokens in [Reason tokens](#reason-tokens)):

1. `result` is an object whose `version` is an unsigned integer.
   `version` is read before the shape, so a later version reports `snapshot-version-<n>` even when its shape changed.
2. `version` is 1.
3. The fields fit the v1 model; unknown fields are ignored at every level, so v1 can gain fields.
4. 1 to 16 swarms, each named `primary` or `worker-N` (`N` a decimal worker id from 0 to 65535 without a leading zero), each name once.
5. At most 10000 entries per swarm.
6. Every `bls_pubkey` is `0x` and 192 hex digits.
7. The primary swarm exists and is `available: true`.
8. The primary's `tracked_epoch` is not null.
9. The available primary has a non-null `admission` (a null one there is `snapshot-malformed`).
10. The primary's `tracked_epoch` is not below its `admission.epoch`, when both are present.
11. The primary's `admission.fallback` is null or `Unresolved`; `Missing`, `Stale`, `Contradictory` and any other value are stale.

The snapshot's epoch is the primary's `tracked_epoch`, the epoch whose pool it lists; the top-level `epoch` is not used.
A snapshot whose epoch is below the last applied epoch is `epoch-regressed`; an equal epoch is the normal case of the runs within one epoch.
A primary `tracked_epoch` above its `admission.epoch` is accepted, and counted as one `pool-ahead` warning.

A worker swarm is authoritative when it is available, its `tracked_epoch` is not null and not below its `admission.epoch`, and its `admission` is present with a fallback of null or `Unresolved`.
A worker swarm that is absent from the snapshot or not authoritative is additions-only (see [Memory](#memory)); it never makes the snapshot stale, `admission: null` included.

Faults inside a valid snapshot are absorbed, counted as occurrences over every swarm, and logged once per kind:

| Fault | What happens | Log kind | Status key |
|---|---|---|---|
| a slot name this build does not know | the slot is ignored | `unknown-slots` | `UNKNOWN_SLOTS` |
| uppercase hex in a `bls_pubkey` | the key is normalised to lowercase | `uppercase-keys` | none |
| a key listed twice in one swarm | slots and addresses merged | `duplicate-keys` | none |
| more than 32 entries in `multiaddrs` or `record_multiaddrs` | the first 32 of each list are read | `truncated-lists` | none |
| a `dns`, `dns4`, `dns6` or `dnsaddr` multiaddr | skipped, never resolved | `dns-skipped` | `DNS_SKIPPED` |
| any other multiaddr without a leading address | skipped | `unparsed` | `UNPARSED` |
| the primary's pool ahead of its window | accepted | `pool-ahead` | none |

Addresses: the first 32 of `multiaddrs` and the first 32 of `record_multiaddrs` both count.
The leading `/ip4/A` or `/ip6/A` segment is parsed into an address.
An IPv4-mapped IPv6 address becomes IPv4 (the mapped form only, never the deprecated compatible form).
Unspecified, multicast and link-local addresses (169.254.0.0/16, fe80::/10) are dropped without a warning, since they are never a peer's source.
When `record_multiaddrs` is present and its addresses differ from those of `multiaddrs`, the entry is a mismatch: both sets are allowed and `check` reports the key.

Fields the decision reads: `version`, `swarms[].network`, `available`, `tracked_epoch`, `admission.epoch`, `admission.fallback`, `entries[].bls_pubkey`, `slots`, `multiaddrs` and `record_multiaddrs`.
The others (`worker_id`, `peer_id`, `network_pubkey`, `record_timestamp`, `source`, `admitted`, `admission.configured`, `admission.effective` and the admission counts) are parsed and not used; the admission mode never matters, because the firewall is a cost filter independent of the node's gate.
Unused is not optional: `source` (a string) and `admitted` (a bool) on every entry, and `configured`, `effective` (strings) and `resolved_current`, `required_current`, `connected_current`, `contradicted` (unsigned integers) in every `admission` object, are required, and a snapshot that lacks one or gives it another type is `snapshot-malformed`.
`worker_id`, `peer_id`, `network_pubkey` and `record_timestamp` may be null or absent.

After a snapshot that validated and folded into memory, the tool calls `tn_getCurrentEpochInfo` best effort and stores its `epochDuration` as `EPOCH_DURATION`.
A failure there logs `epoch-info-failed reason=<token>`, with the token of the RPC failure or `no-epoch-duration` when the answer holds no positive integer `epochDuration`, and never makes the run stale; the default is 21600 until a value is learned.

## The state dir

The scripts create the state dir as root with mode 0700; the tool never creates it.
Every run checks it first and refuses, with `state-dir-invalid` and exit 2 before touching anything, a path that is missing, a symlink, not a directory, owned by a uid other than the effective one (read from `/proc/self/status`) or not exactly mode 0700.

| File | Written by | Format | When |
|---|---|---|---|
| `config` | the scripts | [The config file](#the-config-file) | every verb and resync |
| `floor` | `capture-floor` | [The floor file](#the-floor-file) | `on` and every resync |
| `current.nft` | `sync` | [The program](#the-program) | renamed from `.current.nft.new` only after `nft -f` loaded it |
| `state.json` | `sync` | [Memory](#memory) | whenever its canonical bytes change, after the program is loaded or found loaded |
| `status` | `sync` | [The status file](#the-status-file) | every run that reached a decision |
| `lock` | every locking run | empty, mode 0600, never deleted | created on the first locked run |

Every file the tool writes has mode 0600 whatever the umask; a `lock` found with another mode is set to 0600 by the next locked run.
The directory is opened for its sync with `O_NOFOLLOW|O_DIRECTORY`, so a symlink swapped in for it is never followed.
`state.json`, `status` and `floor` are written atomically: the bytes go to `.<name>.new`, created with `O_CREAT|O_EXCL|O_NOFOLLOW` and synced, then renamed into place, then the directory is synced.
The program is staged the same way in `.current.nft.new`.
A leftover `.<name>.new` from a run that died is removed by the next writer, which holds the lock.
Every file is opened without following symlinks, and every temporary file lives in the state dir, never in `/tmp`.

### The lock

The lock is an exclusive `flock` on `lock`.

- `sync` without `--wait` exits 4 at once when the lock is held; with `--wait SECS` it retries every 200 ms for up to that long.
  The timer runs without `--wait`; the verbs that run the binary pass `--wait 30`.
- `capture-floor` waits up to 30 s.
- `check`, `status` and `render` take no lock and write nothing, so they answer while a sync runs.
- Busy and a failure to take the lock write nothing, not even `status`.
- `lock` itself is created by the first run that takes the lock, a refused one included.

Bash never holds `lock` while the binary runs, since a second open file description conflicts with the tool's own attempt.
`off` never runs the binary: it stops the timer and the sync service, then holds `flock -w 30` on `lock` itself around the table delete and the removal of the state dir.

## Memory

Memory is keyed by `(bls_pubkey, swarm, ip)` and holds `seen`, the epoch the address was last listed.
Each validated snapshot of epoch E is folded in:

1. Every address of every entry in every swarm gets `seen = E`, and the key's absent count in that swarm goes back to 0.
2. A remembered address stays while `E - seen` is below 3: a key listed as `unresolved` or without addresses keeps its last-known addresses at E, E+1 and E+2 and loses them at E+3.
3. A key that an authoritative swarm no longer lists gets one absent strike for that swarm, and its memory for that swarm is removed on the second consecutive strike.
   Strikes count authoritative runs, not epochs, so a key missing from two consecutive 5-minute runs is removed.
4. A swarm that is absent or not authoritative is additions-only: its entries refresh `seen` and clear strikes, the keys it omits get no strike, and its memory is kept subject only to the TTL.
5. Empty maps are pruned and the state's epoch becomes E.

A stale run never reaches memory, so it neither strikes nor refreshes.
The dynamic set is every address memory holds; when it is empty the run is stale with `empty-set` (the same guard `on` applies), so a floor-only table is never built.

`state.json` is compact JSON and a newline:

```json
{"version":1,"epoch":42,"keys":{"0x<192 lowercase hex>":{"primary":{"absent":0,"ips":{"192.0.2.10":42}}}}}
```

Keys sort by their lowercase hex, swarms by name, and addresses IPv4 first and numerically within a family.
A missing file is the empty state at epoch 0.
The file must be exactly what the tool writes for a state it can reach: malformed JSON, another version, an unknown field, an uppercase or malformed key, a swarm name other than `primary` or `worker-N` as the snapshot spells them, a non-canonical address, a `seen` above the epoch or past the TTL, an absent count of 2 or more, an empty map, or any other spelling is `state-invalid`, exit 2.
The recovery is `firewall-setup.sh --p2p-committee off`, then `on`.

`state.json` is rewritten whenever its canonical bytes differ from the file, even when the program is unchanged and `nft -f` is skipped, because a first strike or a refreshed `seen` changes memory without changing any address.
An unchanged snapshot produces identical bytes, so the file and its mtime stay.

## The program

The sets of one program:

- `allow_v4` and `allow_v6`: the dynamic set, the floor and the host entries of `P2P_ALLOW_FROM`.
- `static_v4` and `static_v6`: the network entries of `P2P_ALLOW_FROM`.

The program for ports 49590 and 49594, two IPv4 addresses and one static network, byte for byte:

```text
# firewall-setup.sh: P2P committee allowlist, schema 1; loaded whole by nft -f (one transaction).
table inet tn_p2p_allow {
    set allow_v4 { type ipv4_addr; }
    set allow_v6 { type ipv6_addr; }
    set static_v4 { type ipv4_addr; flags interval; }
    set static_v6 { type ipv6_addr; flags interval; }
    counter tn_accept_p2p_listed { }
    counter tn_drop_p2p_unlisted { }
}
flush table inet tn_p2p_allow
flush set inet tn_p2p_allow allow_v4
flush set inet tn_p2p_allow allow_v6
flush set inet tn_p2p_allow static_v4
flush set inet tn_p2p_allow static_v6
table inet tn_p2p_allow {
    chain input {
        type filter hook input priority -20; policy accept;
        udp dport { 49590, 49594 } ip saddr @allow_v4 counter name "tn_accept_p2p_listed" accept
        udp dport { 49590, 49594 } ip saddr @static_v4 counter name "tn_accept_p2p_listed" accept
        udp dport { 49590, 49594 } ip6 saddr @allow_v6 counter name "tn_accept_p2p_listed" accept
        udp dport { 49590, 49594 } ip6 saddr @static_v6 counter name "tn_accept_p2p_listed" accept
        udp dport { 49590, 49594 } counter name "tn_drop_p2p_unlisted" drop
    }
}
add element inet tn_p2p_allow allow_v4 {
    192.0.2.10,
    198.51.100.7
}
add element inet tn_p2p_allow static_v4 { 203.0.113.0/24 }
```

`tests/golden/program-6.6.nft` pins this text, and further goldens pin the other shapes:

- The header comment is part of the contract; the host-metrics collector sums the two `tn_` counters by name.
- Ports are always written as `{ a, b }`, sorted and deduplicated, even for a single port: `{ 49590 }`.
- An `add element` block is left out when its set is empty.
- A set with one element is written inline, `{ X }`; a set with more has one element per line, indented 4 spaces, separated by commas.
- Addresses and networks are in numeric order; IPv6 is printed in the RFC 5952 compressed form; a network is printed as its masked network address and prefix.
- The text holds no time, epoch or count, so equal sets give byte-equal programs.
- No input string reaches the text, only formatted addresses, networks and ports, so nft syntax cannot be injected.

### Idempotence

`sync` skips `nft -f` and the write of `current.nft` only when all of these hold:

- the program equals `current.nft` byte for byte;
- the table exists, and its four sets hold exactly the desired elements;
- chain `input` exists with type `filter`, hook `input`, priority -20 and policy `accept`;
- it holds exactly the program's 5 rules in order, each with its `udp dport` ports, its `ip`/`ip6 saddr` set, its named counter and its verdict;
- both named counters exist.

Elements are compared as parsed values, never as text: a `/32` or `/128` prefix counts as its address, and a range that is exactly one CIDR block counts as that block (nft 1.0.2 lists a network reaching the top of the address space as a range).
Rules are compared as nft lists their expressions, so a single port printed as a number and a port set printed as an anonymous set both compare as ports, and any expression outside those forms makes the rule differ.
Anything else re-applies, which is how a sync heals an element, a set, a rule or a counter deleted by hand, and a verdict, a policy or a port list changed by hand.
The listing runs only when the table is present and the program equals `current.nft`, since either failing already re-applies; on such a run the check costs one `nft -j list table inet tn_p2p_allow`.
Re-applying the same program or a changed one keeps both counters: seen on nft 1.0.2 and 1.0.9, and checked on the Ubuntu 22.04 and 24.04 runner kernels by the real-kernel job.

### How nft is run

nft runs from `/usr/sbin/nft` (or `--nft`), never through a shell, with a cleared environment holding only `PATH=/usr/sbin:/usr/bin:/sbin:/bin` and `LC_ALL=C`:

| Call | Used by | For |
|---|---|---|
| `nft -j list tables inet` | `sync`, `check`, `status`, `render --check` | whether the table exists, and whether nft works at all |
| `nft -j list table inet tn_p2p_allow` | `sync` (when the program is unchanged), `check`, `status` | the sets, the counters, and the chain's declaration and rules |
| `nft -f <state-dir>/.current.nft.new` | `sync` | applying the program, one transaction |
| `nft -f <state-dir>/current.nft` | the stale path of `sync` | reloading the last-good program into an absent table |
| `nft -c -f -` | `render --check` | checking the program without applying it |

Every `sync` lists the tables before it applies anything, so a missing CAP_NET_ADMIN is caught there; nft 1.0.2 prints nothing on a refused `-f`.
Any failure of either listing call is `nft-unavailable`, exit 2, nothing touched, but one: when the table listing of `sync` fails and a second tables listing shows the table gone, deleted while the node was being asked, the program is applied.
The tool never deletes the table; `off` does.

## A sync run

In the order that decides what a run may touch:

1. The state dir passes its checks (`state-dir-invalid`) and the lock is free (busy, exit 4); otherwise nothing is touched.
2. `config`, `floor` and `state.json` are read; an invalid one is a local error.
3. nft lists its tables (`nft-unavailable`, exit 2, nothing touched).
4. The snapshot is fetched, validated and folded into memory; a snapshot that is not authoritative, a regressed epoch and an empty dynamic set are stale.
5. The program is rendered and applied, or found already loaded ([Idempotence](#idempotence)).
6. `state.json` is written when its bytes changed, then `status`, then the stdout line.

### The stale path

Every stale reason and every local error after step 1 takes the stale path; `nft-unavailable` does not: it exits 2 with nothing touched.

- The tables are listed again first, unless the run has just loaded the program, so a table deleted while the node was being asked counts as absent.
- A present table is kept as it is and never re-applied.
- An absent table is reloaded with `nft -f current.nft` when `current.nft` exists; with no `current.nft` nothing is loaded.
  The sets are never flushed and a floor-only table is never built.
- `state.json`, `current.nft` and `LAST_GOOD` are left untouched.
- `status` gets `RESULT=stale` (exit 3) or `RESULT=error` (exit 2), `STALE_REASON` the token, and `STALE_SINCE` only when it was empty.
- One log line `stale reason=<token> action=<action>` (warning) or `error reason=<token> action=<action> detail=…` (error), with action `kept`, `reloaded`, `none` or `reload-failed`.
- A failed reload overrides the run's reason: `STALE_REASON=reload-failed`, `RESULT=error`, exit 2, and the original reason stays in the first log line.

With `--require-authoritative` every exit-3 reason is refused instead: exit 1, nothing written (not `status`, `state.json`, `current.nft` or the table), and no reload.
Local errors under `--require-authoritative` still exit 2 and take the stale path.

### Reason tokens

Every token below appears as `STALE_REASON` in `status`, as `reason=` on the stdout line and as `reason=` in the log line, except where the table says otherwise.

| Token | Trigger | `sync` exit | `RESULT` |
|---|---|---|---|
| `rpc-unreachable` | connection refused, broken or closed before any response byte, or a connect, read or overall timeout | 3 | `stale` |
| `rpc-http-<code>` | an HTTP status other than 200, such as `rpc-http-503` | 3 | `stale` |
| `rpc-bad-http` | a response that breaks HTTP/1.1 framing | 3 | `stale` |
| `rpc-body-too-large` | a head over 16 KiB or a decoded body over 8 MiB | 3 | `stale` |
| `snapshot-malformed` | a body that is not JSON, no `result`, a `result` that is not an object, no valid `version`, a shape outside the v1 model, or an available primary with `admission: null` | 3 | `stale` |
| `rpc-method-missing` | JSON-RPC error -32601: this node release lacks `tn_admissionSnapshot` | 3 | `stale` |
| `rpc-error-<code>` | any other JSON-RPC error, its code written literally, so -32001 reads `rpc-error--32001`; an `error` member without an integer `code` is `snapshot-malformed` | 3 | `stale` |
| `snapshot-version-<n>` | `version` other than 1, such as `snapshot-version-2` | 3 | `stale` |
| `snapshot-swarms` | no swarm, more than 16, a bad name or a name listed twice | 3 | `stale` |
| `snapshot-entries` | more than 10000 entries in a swarm | 3 | `stale` |
| `snapshot-bls` | a `bls_pubkey` that is not `0x` and 192 hex digits | 3 | `stale` |
| `primary-unavailable` | no primary swarm, or `available: false` | 3 | `stale` |
| `pool-unpublished` | the primary's `tracked_epoch` is null, or below its `admission.epoch` | 3 | `stale` |
| `fallback-missing` | the primary's fallback is `Missing` | 3 | `stale` |
| `fallback-stale` | the primary's fallback is `Stale` | 3 | `stale` |
| `fallback-contradictory` | the primary's fallback is `Contradictory`, or a value this build does not know | 3 | `stale` |
| `epoch-regressed` | the snapshot's epoch is below the applied epoch; after a far-future `tracked_epoch` was accepted once, every honest snapshot regresses, and the recovery is `off`, then `on` | 3 | `stale` |
| `empty-set` | memory holds no address after the snapshot | 3 | `stale` |
| `config-invalid` | `config` is missing, unreadable or breaks the grammar | 2 | `error` |
| `floor-invalid` | `floor` is unreadable or malformed; for `capture-floor`, an input it cannot read | 2 | `error` |
| `state-invalid` | `state.json` is not in the form the tool writes; recover with `off`, then `on` | 2 | `error` |
| `state-io-failed` | a read, write, rename, sync or lock failure in the state dir, or a symlink or special file where a file should be (at `config` or `floor` it is `config-invalid` or `floor-invalid` instead) | 2 | `error`, or no status when the lock itself failed |
| `nft-apply-failed` | `nft -f` refused the program; the transaction is atomic, so the live table is unchanged | 2 | `error` |
| `reload-failed` | the stale path's `nft -f current.nft` failed | 2 | `error` |
| `state-dir-invalid` | the state dir failed its checks | 2 | no status written |
| `nft-unavailable` | nft is missing, failed a listing call or was refused it | 2 | no status written |
| `busy` | the lock is held (log line `busy`) | 4 | no status written |

Log-only tokens, never written to `status`: `usage` (a relative `--nft`), `snapshot-unreadable` (`render --snapshot` cannot read its file), `nft-check-failed` (`render --check` and nft rejected the program), `output-failed` (stdout could not be written, or `status --json` could not be serialised) and `no-epoch-duration` (the `epoch-info-failed` reason of an answer without a positive integer `epochDuration`).

The deployment's `on` maps exit 1 with `reason=rpc-method-missing` on the stdout line to "this node release lacks tn_admissionSnapshot".
`check-node.sh` reports `rpc-method-missing`, `snapshot-version-*` and `epoch-regressed` at once, since they never clear on their own, and every other reason once `STALE_SINCE` is older than 2 epochs.

## Outputs

### The `sync` line

Every `sync` run that wrote `status`, tried to and failed, or was refused under `--require-authoritative`, prints exactly one line on stdout:

```text
result=applied reason= epoch=42 v4=3 v6=1 static=1 floor=2 keys=2 dns_skipped=0 unparsed=0
```

- `result` is `applied`, `unchanged`, `stale` or `error`, the `RESULT` written; a refusal prints `stale` and exits 1.
- `reason` is `STALE_REASON`, empty on a fresh run.
- `epoch` is `EPOCH`, empty before the first good run.
- `v4` and `v6` are `ALLOWED_V4` and `ALLOWED_V6`, `static` is `STATIC_V4` plus `STATIC_V6`, and `floor`, `keys`, `dns_skipped` and `unparsed` are `FLOOR`, `KEYS`, `DNS_SKIPPED` and `UNPARSED`.

The counts come from the status the run wrote.
A stale or error run keeps every count of the previous status, so its line shows the previous counts: a first run that goes stale prints `floor=0` even with a floor file, and a refusal prints the counts of the last status on disk.
A run whose write of `status` failed prints `result=error reason=state-io-failed` although no new status reached the disk.
No line is printed on a usage error, an unsafe state dir, busy, a lock failure or `nft-unavailable`.

### `check`

`check` reads `config` for the RPC port, `status` for the applied `EPOCH` and `state.json` for the addresses the last good sync held, lists the table, then fetches the snapshot as `sync` does.
It reads `state.json` before the listing, because `sync` renames it into place only after the table holds it.
It takes no lock and writes nothing.
It prints up to 5 lines `missing <ip>`, then up to 5 lines `mismatch 0x<first 16 hex digits of the key>`, then one line:

```text
result=ok missing_total=0 mismatch_total=0
```

| Verdict | Meaning | Exit | Lines before it |
|---|---|---|---|
| `ok` | every current committee address of the primary swarm is in `allow_v4` or `allow_v6` | 0 | `mismatch` lines |
| `missing` | a current committee address is in neither allow set, and `state.json` holds one such address or cannot be read | 1 | `missing` and `mismatch` lines |
| `absent` | the table does not exist; the node is not asked | 1 | none |
| `behind` | the snapshot's epoch is above `EPOCH`, no good sync has run yet, or every missing current address is one `state.json` does not hold yet: the next sync catches up | 3 | `missing` and `mismatch` lines, for information |
| `stale` | the snapshot is not authoritative, or its epoch is below `EPOCH` | 3 | none |

- An address counts as present only as the very element, a full-length prefix included; a static network that covers it does not count.
- A mismatch is a key, in any swarm, whose `record_multiaddrs` give other addresses than its `multiaddrs`; both are allowed, so mismatches never change the verdict.
- The totals count every missing address and every mismatched key, past the 5 lines shown.
- `stale` logs `stale reason=<token>`, and for a snapshot below `EPOCH` `stale reason=epoch-regressed snapshot_epoch=N applied_epoch=N`; `behind` logs `behind snapshot_epoch=N applied_epoch=N`, with `applied_epoch=` empty before the first good sync and both epochs equal when the missing addresses are ones `state.json` does not hold yet.

### The status file

`<state-dir>/status` holds one `KEY=VALUE` line per key, in this order:

| Key | Value | Meaning |
|---|---|---|
| `STATUS_VERSION` | `1` | the format |
| `TOOL_VERSION` | `0.1.0` | the version of the build that wrote the file |
| `RESULT` | `applied`, `unchanged`, `stale`, `error` or empty | how the last run that reached a decision ended; empty before the first |
| `LAST_RUN` | unix seconds | when that run started, once it held the lock |
| `LAST_GOOD` | unix seconds or empty | when a run last applied the program or found it loaded |
| `EPOCH` | integer or empty | the epoch of the snapshot the loaded program was built from |
| `EPOCH_DURATION` | seconds | the node's `epochDuration`, 21600 until learned |
| `STALE_SINCE` | unix seconds or empty | when the current series of stale or failed runs began; empty while fresh |
| `STALE_REASON` | a [reason token](#reason-tokens) or empty | the reason of the last stale or failed run; empty while fresh |
| `ALLOWED_V4` | integer | elements of `allow_v4` |
| `ALLOWED_V6` | integer | elements of `allow_v6` |
| `STATIC_V4` | integer | networks of `static_v4` |
| `STATIC_V6` | integer | networks of `static_v6` |
| `FLOOR` | integer | addresses in the floor |
| `KEYS` | integer | BLS keys in memory |
| `DNS_SKIPPED` | integer | `dns*` multiaddrs in the snapshot, never resolved |
| `UNPARSED` | integer | other multiaddrs without a leading address |
| `UNKNOWN_SLOTS` | integer | slot names this build does not know |

- Every value matches `[A-Za-z0-9._:-]*`, so a per-key regex read in bash is safe and nothing is ever unquoted.
  A value outside that grammar would be a bug in the writer; it is written as `invalid` with a `status-value-invalid key=<KEY>` warning.
- An empty value means not known yet.
- A good run writes every key, with `LAST_GOOD=LAST_RUN` and both `STALE_*` keys empty.
- A stale or failed run changes only `TOOL_VERSION`, `RESULT`, `LAST_RUN`, `STALE_SINCE` (when empty), `STALE_REASON` and `EPOCH_DURATION` (when learned in that run); every count, `EPOCH` and `LAST_GOOD` stay as the last good run left them.
- Busy runs, usage errors, an unsafe state dir, `nft-unavailable`, a lock failure and refusals write no status.
- Runs read the file back only to carry values forward: every value on the stale path, `EPOCH_DURATION` on a good run whose epoch info failed, and the applied `EPOCH` that `check` compares with.
  A file of another `STATUS_VERSION` reads as the status before the first run, and so does an unreadable one, with a `status-unreadable` warning.

### `status`

`status` prints the status file's 18 lines, then 7 live keys:

| Key | Value | Meaning |
|---|---|---|
| `LOADED` | `true`, `false` or empty | whether the table exists; empty when nft could not list it |
| `AGE_SECS` | integer or empty | now minus `LAST_GOOD`, 0 when the clock went back; empty before the first good run |
| `STALE` | `true`, `false` or empty | whether `AGE_SECS` is above 2 × `EPOCH_DURATION`; empty before the first good run |
| `COUNTER_ACCEPT_PACKETS`, `COUNTER_ACCEPT_BYTES` | integer or empty | counter `tn_accept_p2p_listed` |
| `COUNTER_DROP_PACKETS`, `COUNTER_DROP_BYTES` | integer or empty | counter `tn_drop_p2p_unlisted` |

The counters are empty when nft could not list the table, the table is absent or the counter is gone.
`status` takes no lock and writes nothing.

### `status --json`

`status --json` prints exactly one line, a JSON object, with these keys in this order:

| Key | Type | Source |
|---|---|---|
| `loaded` | bool or null | `LOADED` |
| `epoch` | integer or null | `EPOCH` |
| `allowed_v4` | integer | `ALLOWED_V4` |
| `allowed_v6` | integer | `ALLOWED_V6` |
| `static` | integer | `STATIC_V4` plus `STATIC_V6` |
| `floor` | integer | `FLOOR` |
| `keys` | integer | `KEYS` |
| `dns_skipped` | integer | `DNS_SKIPPED` |
| `last_good` | integer or null | `LAST_GOOD` |
| `age_secs` | integer or null | `AGE_SECS` |
| `stale` | bool or null | `STALE` |
| `stale_since` | integer or null | `STALE_SINCE` |
| `stale_reason` | string or null | `STALE_REASON`, null when empty |
| `counters` | object | `{"accepted": …, "dropped": …}`, the packet counts of the two counters, each an integer or null |

A value that is not known is `null`, never an empty string.
Before the first run, with the table loaded and nft working, it prints:

```json
{"loaded":true,"epoch":null,"allowed_v4":0,"allowed_v6":0,"static":0,"floor":0,"keys":0,"dns_skipped":0,"last_good":null,"age_secs":null,"stale":null,"stale_since":null,"stale_reason":null,"counters":{"accepted":0,"dropped":0}}
```

The tool never emits the keys `enabled`, `persistent`, `timer` or `reason`.
The scripts own those facts and splice them in front with `printf` (`"${json#\{}"`); when the binary is missing or exits non-zero they emit only their four keys, with `reason` set to `tool-missing` or `tool-error`.

### `render`

`render [--snapshot FILE] [--check]` prints the program `sync` would load on stdout and one summary line on stderr, and writes nothing.
The summary goes through the log's 8-line cap like any log line, so a run with many warnings can lose it to `log-capped`.

- Memory comes from `state.json` and is updated in memory only; no lock is taken.
- `--snapshot FILE` reads a regular file of at most 8 MiB holding either the `result` object or a whole JSON-RPC response; without it the node is asked.
  A larger file is `rpc-body-too-large` and bytes that are not JSON are `snapshot-malformed`, both refused with exit 1 as from the node.
- A printed program has the summary `result=rendered reason= epoch=… v4=… …` and exit 0.
- A stale reason prints no program, the summary `result=stale reason=<token> …` and exit 1; its `epoch` and counts are those of memory as `state.json` holds it, epoch 0 and no keys when there is none.
- With `--check`, nft must list its tables once `config`, `floor` and `state.json` are read and before the snapshot is, and must accept the program with `nft -c -f -`, or the run exits 2 without printing it; this needs CAP_NET_ADMIN.

### Log lines

Every log line goes to stderr, one event per line: the event name, then `key=value` fields.

- Under systemd a line starts with `<3>` (error) or `<4>` (warning), which journald reads as its priority.
  The prefix appears only when `JOURNAL_STREAM` names the device and inode of fd 2, systemd's documented check, so interactive output stays plain.
- A value that is not plain printable ASCII is written quoted and escaped, so an event always stays on one line.
- A run writes at most 8 lines; past that, the eighth is replaced by `log-capped dropped=N`.
- The events: `error`, `stale`, `refused`, `busy`, `behind`, `nft-path`, `nft-unavailable`, `static-catch-all`, `static-covered`, `floor-missing`, `absorbed kind=<kind> count=N`, `epoch-info-failed`, `status-unreadable`, `status-value-invalid` and `log-capped`.
- `capture-floor` writes its single failure line without a prefix: `capture-floor: <token>: <detail>`.

The deployment reads exit codes, stdout lines and the status file, never the log lines.
One exception: the `result=` line of `check` carries no reason, so the deployment reads the `reason=` word of the `<3>error reason=…` and `<4>stale reason=…` lines `check` writes, and that word on those two events of `check` is part of this contract.

## What the tool never does

- It never reads `/etc/telcoin`, `.node-meta` or the node's data dir; the scripts hand it `config` and `floor`, and only `capture-floor` (run outside the sandbox) and `render --snapshot` open files named on the command line.
- It reads no environment variable but `JOURNAL_STREAM`, and nft gets a cleared environment.
- It opens one kind of connection, to `127.0.0.1:RPC_PORT`, and never resolves a name; `dns*` multiaddrs are skipped.
- It runs no program but nft, never through a shell.
- It never creates the state dir, never writes outside it, and never deletes the table or flushes the sets on an error path.
- It never parses bash quoting, YAML or the deployment's own files.

The sync unit holds it to that: `InaccessiblePaths=/etc/telcoin` plus the data dir, `ReadWritePaths=` for the state dir with `ReadOnlyPaths=` for `config` and `floor` inside it, `IPAddressAllow=localhost`, `MemoryDenyWriteExecute=yes` and `CapabilityBoundingSet=CAP_NET_ADMIN`.
