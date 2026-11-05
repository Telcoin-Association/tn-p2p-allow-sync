//! Per-key memory: the addresses the sync keeps between runs, their TTL, the two-strike removal and `state.json`.
//!
//! Memory is keyed by `(bls, swarm, ip)` and holds `seen`, the epoch the address was last listed.
//! [`apply`] folds one validated snapshot into it, for the snapshot's epoch E:
//!
//! 1. Every IP of every entry, in every swarm, gets `seen = E`.
//! 2. A remembered address stays while `E - seen` is below [`TTL_EPOCHS`].
//!    A key listed with `source: unresolved`, or with no address, keeps its last-known addresses at E, E+1 and E+2 and loses them at E+3.
//! 3. A key that an authoritative swarm no longer lists gets an `absent` strike for that swarm, and once it has [`STRIKES`] strikes its memory for that swarm is removed.
//!    Strikes count consecutive authoritative runs that omit the key, not epochs: a key missing from two consecutive 5-minute runs at the same epoch is removed.
//!    A listing of the key in that swarm, authoritative or not, resets the count to 0.
//!    A stale run never reaches [`apply`], so it neither strikes nor resets.
//! 4. A swarm the snapshot does not hold, or holds as non-authoritative, is additions-only: its entries refresh `seen` and reset strikes, the keys it does not list get no strike, and its memory is kept subject only to the TTL.
//!
//! The dynamic part of the allow set is [`allow_ips`], every address memory holds.
//!
//! `state.json` is the canonical form of a [`State`]: compact JSON plus a newline, such as `{"version":1,"epoch":42,"keys":{"0x<192 lowercase hex>":{"primary":{"absent":0,"ips":{"192.0.2.10":42}}}}}`.
//! Every map is ordered: keys by bytes, which is the order of their lowercase hex; swarms by name; IPs with IPv4 before IPv6 and by value within a family, never by text.
//! The IP order is std's own order of `IpAddr`, which compares the family first and then the address octets.
//! [`serialize`] writes the file, and [`load_state`] accepts exactly what [`serialize`] writes for a state [`apply`] can reach; anything else is `state-invalid`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::addr::canonical_ip;
use crate::snapshot::{self, BlsKey, Snapshot};

/// The `state.json` format this build reads and writes.
pub const STATE_VERSION: u32 = 1;

/// How many epochs a remembered address outlives its last listing: it stays while `E - seen` is below this.
pub const TTL_EPOCHS: u64 = 3;

/// How many consecutive authoritative runs of a swarm must omit a key before its memory for that swarm is removed.
pub const STRIKES: u8 = 2;

/// Everything the sync remembers between runs, the content of `state.json`.
///
/// [`State::default`] is the state before the first run: [`STATE_VERSION`], epoch 0, no keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct State {
    /// The format, [`STATE_VERSION`].
    pub version: u32,
    /// The epoch of the last applied snapshot, 0 before the first.
    pub epoch: u64,
    /// Memory by BLS key, then by swarm name.
    ///
    /// A key or swarm with nothing to remember has no entry: [`apply`] prunes empty maps.
    pub keys: BTreeMap<BlsKey, BTreeMap<String, SwarmMemory>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            epoch: 0,
            keys: BTreeMap::new(),
        }
    }
}

/// What memory holds for one key in one swarm.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmMemory {
    /// Consecutive authoritative runs of the swarm that omitted the key: 0 while it is listed, at most `STRIKES - 1` in a stored state.
    pub absent: u8,
    /// Every remembered address with its `seen` epoch, the epoch it was last listed.
    pub ips: BTreeMap<IpAddr, u64>,
}

/// What one [`apply`] changed.
///
/// `added`, `removed` and `expired` count `(key, swarm, ip)` memories, so an address remembered under two keys counts twice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Keys in memory afterwards.
    pub keys: usize,
    /// Addresses remembered for the first time under their key and swarm.
    pub added: usize,
    /// Addresses dropped with a key's memory for a swarm on its last strike.
    pub removed: usize,
    /// Addresses dropped because they were last listed [`TTL_EPOCHS`] or more epochs ago.
    pub expired: usize,
}

/// Why memory could not be loaded or a snapshot could not be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    /// `state.json` is not a state this tool writes: malformed, another version, a key or address that does not parse, or not in canonical form.
    ///
    /// The sync exits 2 without touching the table; the recovery is `off` then `on`.
    StateInvalid,
    /// The snapshot's epoch is below the state's, so the snapshot is stale and the state was left untouched.
    EpochRegressed,
}

impl MemoryError {
    /// The `STALE_REASON` token: `state-invalid` or `epoch-regressed`.
    pub const fn token(&self) -> &'static str {
        match self {
            Self::StateInvalid => "state-invalid",
            Self::EpochRegressed => "epoch-regressed",
        }
    }
}

impl fmt::Display for MemoryError {
    /// Prints the token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

impl std::error::Error for MemoryError {}

/// `state.json` as written, before its keys are parsed and its form is checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    version: u32,
    epoch: u64,
    keys: BTreeMap<String, BTreeMap<String, SwarmMemory>>,
}

/// Reads `state.json`; `None`, a missing file, is the empty [`State::default`].
///
/// The bytes must be exactly what [`serialize`] writes for a state [`apply`] can reach, so the check is strict.
/// Malformed JSON, an unknown or missing field, a version other than [`STATE_VERSION`], a key that is not `0x` and 192 lowercase hex digits, a swarm name the snapshot parser would refuse, an address that does not parse or is not its own [`canonical_ip`], a `seen` above the epoch or outside the TTL, an `absent` of [`STRIKES`] or more, an empty map, and any other spelling (spacing, order, duplicate keys, a missing newline) are all [`MemoryError::StateInvalid`].
pub fn load_state(bytes: Option<&[u8]>) -> Result<State, MemoryError> {
    let Some(bytes) = bytes else {
        return Ok(State::default());
    };
    let raw: RawState = serde_json::from_slice(bytes).map_err(|_| MemoryError::StateInvalid)?;
    if raw.version != STATE_VERSION {
        return Err(MemoryError::StateInvalid);
    }
    let mut keys = BTreeMap::new();
    for (text, swarms) in raw.keys {
        // the writer prints lowercase only, so an uppercase digit means another writer
        let (key, uppercase) =
            BlsKey::parse_reporting_case(&text).map_err(|_| MemoryError::StateInvalid)?;
        if uppercase {
            return Err(MemoryError::StateInvalid);
        }
        keys.insert(key, swarms);
    }
    let state = State {
        version: raw.version,
        epoch: raw.epoch,
        keys,
    };
    // the byte compare also catches what the typed model cannot see: duplicate JSON keys, of which serde keeps the last without a word
    if !reachable(&state) || serialize(&state) != bytes {
        return Err(MemoryError::StateInvalid);
    }
    Ok(state)
}

/// Whether `state` holds only what [`apply`] leaves behind.
fn reachable(state: &State) -> bool {
    state.keys.values().all(|swarms| {
        !swarms.is_empty()
            && swarms.iter().all(|(name, memory)| {
                // memory is kept by the swarm names of the snapshots it came from
                snapshot::is_swarm_name(name)
                    && memory.absent < STRIKES
                    && !memory.ips.is_empty()
                    && memory.ips.iter().all(|(ip, seen)| {
                        canonical_ip(*ip) == Some(*ip)
                            && *seen <= state.epoch
                            && live(state.epoch, *seen)
                    })
            })
    })
}

/// Whether an address last listed at `seen` is still remembered at `epoch`.
fn live(epoch: u64, seen: u64) -> bool {
    epoch.saturating_sub(seen) < TTL_EPOCHS
}

/// Folds the snapshot into memory under the rules in the module documentation, for E = [`Snapshot::epoch`].
///
/// A snapshot below the state's epoch is [`MemoryError::EpochRegressed`] and leaves `state` untouched.
/// An equal epoch is the normal case of the runs within one epoch.
/// The entries' IPs are taken as the snapshot holds them, already through [`canonical_ip`].
pub fn apply(state: &mut State, snap: &Snapshot) -> Result<ApplyOutcome, MemoryError> {
    if snap.epoch < state.epoch {
        return Err(MemoryError::EpochRegressed);
    }
    let epoch = snap.epoch;
    let mut outcome = ApplyOutcome::default();

    // rules 1 and 4: every listing refreshes its addresses and clears its swarm's strikes
    for (name, swarm) in &snap.swarms {
        for (key, entry) in &swarm.entries {
            let memory = state
                .keys
                .entry(*key)
                .or_default()
                .entry(name.clone())
                .or_default();
            memory.absent = 0;
            for ip in &entry.ips {
                if memory.ips.insert(*ip, epoch).is_none() {
                    outcome.added = outcome.added.saturating_add(1);
                }
            }
        }
    }

    for (key, swarms) in &mut state.keys {
        swarms.retain(|name, memory| {
            // rule 3; a swarm the snapshot lacks or holds as non-authoritative strikes nothing (rule 4)
            let omitted = snap
                .swarms
                .get(name)
                .is_some_and(|swarm| swarm.authoritative && !swarm.entries.contains_key(key));
            if omitted {
                memory.absent = memory.absent.saturating_add(1);
                if memory.absent >= STRIKES {
                    outcome.removed = outcome.removed.saturating_add(memory.ips.len());
                    return false;
                }
            }
            // rule 2, for every memory that survives the strikes
            let before = memory.ips.len();
            memory.ips.retain(|_, seen| live(epoch, *seen));
            outcome.expired = outcome
                .expired
                .saturating_add(before.saturating_sub(memory.ips.len()));
            !memory.ips.is_empty()
        });
    }
    state.keys.retain(|_, swarms| !swarms.is_empty());

    state.epoch = epoch;
    outcome.keys = state.keys.len();
    Ok(outcome)
}

/// Every address memory holds, the dynamic part of the allow set.
///
/// [`apply`] has already dropped the addresses past their TTL.
pub fn allow_ips(state: &State) -> BTreeSet<IpAddr> {
    state
        .keys
        .values()
        .flat_map(BTreeMap::values)
        .flat_map(|memory| memory.ips.keys().copied())
        .collect()
}

/// The canonical `state.json` bytes of `state`: compact JSON and a newline.
///
/// Equal states give equal bytes, so the sync writes the file only when they differ.
pub fn serialize(state: &State) -> Vec<u8> {
    match serde_json::to_vec(state) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            bytes
        }
        // cannot happen: every map key serialises as a string and a Vec never fails a write;
        // were it to, empty bytes are refused by `load_state`, so it surfaces as state-invalid
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{BLS_KEY_LEN, Entry, Slot, Swarm, Warnings, parse};
    use std::error::Error;
    use std::net::{Ipv4Addr, Ipv6Addr};

    type TestResult = Result<(), Box<dyn Error>>;

    const GOOD: &str = include_str!("../tests/fixtures/snapshot/good.json");

    /// One swarm of a test snapshot: its name, whether it is authoritative, and each key with its addresses.
    type SwarmSpec<'a> = (&'a str, bool, &'a [(BlsKey, &'a [IpAddr])]);

    const K1: BlsKey = key(0x11);
    const K2: BlsKey = key(0x22);
    const A: IpAddr = v4(192, 0, 2, 10);
    const B: IpAddr = v4(198, 51, 100, 7);
    const C: IpAddr = v4(203, 0, 113, 5);
    const D: IpAddr = doc6(0xd);

    const fn key(byte: u8) -> BlsKey {
        BlsKey::from_bytes([byte; BLS_KEY_LEN])
    }

    const fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    /// An address of 2001:db8::/32, the IPv6 documentation range.
    const fn doc6(low: u16) -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, low))
    }

    fn snapshot(epoch: u64, swarms: &[SwarmSpec<'_>]) -> Snapshot {
        let swarms = swarms
            .iter()
            .map(|(name, authoritative, entries)| {
                let entries = entries
                    .iter()
                    .map(|(key, ips)| {
                        let entry = Entry {
                            slots: BTreeSet::from([Slot::Current]),
                            ips: ips.iter().copied().collect(),
                            mismatch: false,
                        };
                        (*key, entry)
                    })
                    .collect();
                let swarm = Swarm {
                    authoritative: *authoritative,
                    tracked_epoch: authoritative.then_some(epoch),
                    entries,
                };
                (String::from(*name), swarm)
            })
            .collect();
        Snapshot {
            epoch,
            swarms,
            warnings: Warnings::default(),
        }
    }

    /// A snapshot holding only the primary swarm.
    fn primary(epoch: u64, entries: &[(BlsKey, &[IpAddr])]) -> Snapshot {
        snapshot(epoch, &[("primary", true, entries)])
    }

    fn ips(state: &State, key: BlsKey, swarm: &str) -> Option<BTreeMap<IpAddr, u64>> {
        state
            .keys
            .get(&key)?
            .get(swarm)
            .map(|memory| memory.ips.clone())
    }

    fn absent(state: &State, key: BlsKey, swarm: &str) -> Option<u8> {
        state.keys.get(&key)?.get(swarm).map(|memory| memory.absent)
    }

    fn reload(state: &State) -> Result<State, MemoryError> {
        load_state(Some(serialize(state).as_slice()))
    }

    /// The single-key file of the plan at epoch 42, with `memory` as the key's primary-swarm memory.
    fn file(memory: &str) -> String {
        format!(
            "{{\"version\":1,\"epoch\":42,\"keys\":{{\"0x{}\":{{\"primary\":{memory}}}}}}}\n",
            "ab".repeat(BLS_KEY_LEN)
        )
    }

    #[test]
    fn no_file_is_the_empty_state() -> TestResult {
        let state = load_state(None)?;
        assert_eq!(state, State::default());
        assert_eq!((state.version, state.epoch), (STATE_VERSION, 0));
        assert_eq!(
            serialize(&state),
            b"{\"version\":1,\"epoch\":0,\"keys\":{}}\n"
        );
        assert_eq!(reload(&state)?, state);
        Ok(())
    }

    #[test]
    fn serialises_the_documented_form() -> TestResult {
        let mut state = State::default();
        apply(&mut state, &primary(42, &[(key(0xab), &[A])]))?;
        let expected = file(r#"{"absent":0,"ips":{"192.0.2.10":42}}"#);
        assert_eq!(String::from_utf8(serialize(&state))?, expected);
        assert_eq!(load_state(Some(expected.as_bytes()))?, state);
        Ok(())
    }

    #[test]
    fn parsed_snapshot_gives_the_canonical_file() -> TestResult {
        let snap = parse(serde_json::from_str(GOOD)?)?;
        let mut state = State::default();
        let outcome = apply(&mut state, &snap)?;
        let expected = format!(
            concat!(
                r#"{{"version":1,"epoch":42,"keys":{{"#,
                r#""0x8f3a{zeros}c21d":{{"primary":{{"absent":0,"ips":{{"203.0.113.10":42}}}}}},"#,
                r#""0xa71e{zeros}09b4":{{"primary":{{"absent":0,"ips":{{"198.51.100.7":42}}}}}}"#,
                "}}}}\n"
            ),
            zeros = "0".repeat(184)
        );
        assert_eq!(String::from_utf8(serialize(&state))?, expected);
        let expected_outcome = ApplyOutcome {
            keys: 2,
            added: 2,
            removed: 0,
            expired: 0,
        };
        assert_eq!(outcome, expected_outcome);
        assert_eq!(allow_ips(&state), BTreeSet::from([B, v4(203, 0, 113, 10)]));
        assert_eq!(reload(&state)?, state);
        Ok(())
    }

    #[test]
    fn ip_keys_sort_ipv4_first_then_numerically() -> TestResult {
        // text order would put 10.0.0.1 first, 9.9.9.9 last and 2001:db8:1::1 before 2001:db8::2
        let scrambled = [
            doc6(0x10),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1)),
            v4(192, 0, 2, 1),
            doc6(2),
            v4(10, 0, 0, 1),
            v4(9, 9, 9, 9),
        ];
        let mut state = State::default();
        apply(&mut state, &primary(42, &[(K1, &scrambled)]))?;
        let printed: Vec<String> = allow_ips(&state).iter().map(IpAddr::to_string).collect();
        let expected = [
            "9.9.9.9",
            "10.0.0.1",
            "192.0.2.1",
            "2001:db8::2",
            "2001:db8::10",
            "2001:db8:1::1",
        ];
        assert_eq!(printed, expected);
        let text = String::from_utf8(serialize(&state))?;
        let ips = expected.map(|ip| format!("\"{ip}\":42")).join(",");
        assert!(text.contains(&format!("\"ips\":{{{ips}}}")), "{text}");
        assert_eq!(reload(&state)?, state);
        Ok(())
    }

    #[test]
    fn permuted_insertion_serialises_byte_equal() -> TestResult {
        let items = [
            (K2, "worker-1", D, 41),
            (K1, "primary", A, 42),
            (K2, "primary", B, 40),
            (K1, "worker-1", C, 42),
            (K1, "primary", doc6(1), 40),
            (K2, "worker-0", v4(10, 0, 0, 1), 42),
        ];
        let build = |order: &mut dyn Iterator<Item = &(BlsKey, &str, IpAddr, u64)>| {
            let mut state = State {
                epoch: 42,
                ..State::default()
            };
            for (key, swarm, ip, seen) in order {
                let memory = state.keys.entry(*key).or_default();
                memory
                    .entry(String::from(*swarm))
                    .or_default()
                    .ips
                    .insert(*ip, *seen);
            }
            state
        };
        let forward = build(&mut items.iter());
        let backward = build(&mut items.iter().rev());
        assert_eq!(serialize(&forward), serialize(&backward));
        assert_eq!(reload(&backward)?, forward);
        Ok(())
    }

    #[test]
    fn round_trip_through_load_state() -> TestResult {
        let mut state = State::default();
        apply(
            &mut state,
            &snapshot(
                40,
                &[
                    ("primary", true, &[(K1, &[A, D]), (K2, &[B])]),
                    ("worker-0", false, &[(K1, &[C])]),
                ],
            ),
        )?;
        // K1 unresolved, K2 struck once, worker-0 gone from the snapshot
        apply(&mut state, &primary(42, &[(K1, &[])]))?;
        assert_eq!(absent(&state, K2, "primary"), Some(1));
        let bytes = serialize(&state);
        let loaded = load_state(Some(bytes.as_slice()))?;
        assert_eq!(loaded, state);
        assert_eq!(serialize(&loaded), bytes);
        Ok(())
    }

    #[test]
    fn uppercase_key_is_state_invalid() -> TestResult {
        let mut state = State::default();
        apply(&mut state, &primary(42, &[(key(0xab), &[A])]))?;
        let lower = String::from_utf8(serialize(&state))?;
        assert_eq!(load_state(Some(lower.as_bytes()))?, state);
        let upper = lower.replace(&"ab".repeat(BLS_KEY_LEN), &"AB".repeat(BLS_KEY_LEN));
        assert_ne!(upper, lower);
        assert_eq!(
            load_state(Some(upper.as_bytes())),
            Err(MemoryError::StateInvalid)
        );
        Ok(())
    }

    #[test]
    fn anything_but_a_reachable_canonical_file_is_state_invalid() -> TestResult {
        let good = r#"{"absent":0,"ips":{"192.0.2.10":42}}"#;
        let key_text = format!("0x{}", "ab".repeat(BLS_KEY_LEN));
        let cases = [
            ("not json", String::from("nope\n")),
            ("empty", String::new()),
            (
                "version 2",
                String::from("{\"version\":2,\"epoch\":0,\"keys\":{}}\n"),
            ),
            (
                "version 0",
                String::from("{\"version\":0,\"epoch\":0,\"keys\":{}}\n"),
            ),
            (
                "missing field",
                String::from("{\"version\":1,\"keys\":{}}\n"),
            ),
            (
                "unknown field",
                String::from("{\"version\":1,\"epoch\":0,\"keys\":{},\"x\":0}\n"),
            ),
            (
                "float epoch",
                String::from("{\"version\":1,\"epoch\":0.0,\"keys\":{}}\n"),
            ),
            (
                "keys not a map",
                String::from("{\"version\":1,\"epoch\":0,\"keys\":[]}\n"),
            ),
            (
                "unknown memory field",
                file(r#"{"absent":0,"ips":{"192.0.2.10":42},"x":0}"#),
            ),
            ("short key", file(good).replace(&key_text, "0xabab")),
            (
                "no 0x",
                file(good).replace(&key_text, &"ab".repeat(BLS_KEY_LEN)),
            ),
            ("bad ip", file(r#"{"absent":0,"ips":{"192.0.2.300":42}}"#)),
            (
                "mapped ip",
                file(r#"{"absent":0,"ips":{"::ffff:192.0.2.10":42}}"#),
            ),
            (
                "uppercase ipv6",
                file(r#"{"absent":0,"ips":{"2001:DB8::1":42}}"#),
            ),
            (
                "uncompressed ipv6",
                file(r#"{"absent":0,"ips":{"2001:0db8::1":42}}"#),
            ),
            (
                "unspecified ip",
                file(r#"{"absent":0,"ips":{"0.0.0.0":42}}"#),
            ),
            (
                "multicast ip",
                file(r#"{"absent":0,"ips":{"224.0.0.1":42}}"#),
            ),
            (
                "seen above epoch",
                file(r#"{"absent":0,"ips":{"192.0.2.10":43}}"#),
            ),
            (
                "seen past the ttl",
                file(r#"{"absent":0,"ips":{"192.0.2.10":39}}"#),
            ),
            (
                "absent at strikes",
                file(r#"{"absent":2,"ips":{"192.0.2.10":42}}"#),
            ),
            (
                "absent negative",
                file(r#"{"absent":-1,"ips":{"192.0.2.10":42}}"#),
            ),
            (
                "absent over u8",
                file(r#"{"absent":256,"ips":{"192.0.2.10":42}}"#),
            ),
            ("empty ips", file(r#"{"absent":0,"ips":{}}"#)),
            (
                "duplicate ip",
                file(r#"{"absent":0,"ips":{"192.0.2.10":42,"192.0.2.10":41}}"#),
            ),
            (
                "unsorted ips",
                file(r#"{"absent":0,"ips":{"192.0.2.10":42,"10.0.0.1":42}}"#),
            ),
            (
                "empty swarms",
                format!("{{\"version\":1,\"epoch\":42,\"keys\":{{\"{key_text}\":{{}}}}}}\n"),
            ),
            (
                "duplicate key",
                format!(
                    "{{\"version\":1,\"epoch\":42,\"keys\":{{\"{key_text}\":{{\"primary\":{good}}},\"{key_text}\":{{\"primary\":{good}}}}}}}\n"
                ),
            ),
            ("no newline", file(good).trim_end().to_owned()),
            ("crlf", file(good).replace('\n', "\r\n")),
            ("spaced", file(good).replace(':', ": ")),
        ];
        assert!(load_state(Some(file(good).as_bytes())).is_ok());
        for (name, text) in &cases {
            assert_eq!(
                load_state(Some(text.as_bytes())),
                Err(MemoryError::StateInvalid),
                "{name}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_swarm_name_the_snapshot_parser_refuses_is_state_invalid() -> TestResult {
        let good = file(r#"{"absent":0,"ips":{"192.0.2.10":42}}"#);
        let named = |name: &str| good.replace("\"primary\":", &format!("\"{name}\":"));
        for name in ["primary", "worker-0", "worker-7", "worker-65535"] {
            assert!(load_state(Some(named(name).as_bytes())).is_ok(), "{name}");
        }
        for name in [
            "bogus-swarm",
            "Primary",
            "",
            "worker-",
            "worker-01",
            "worker-65536",
            "worker--1",
            "worker-1 ",
        ] {
            assert_eq!(
                load_state(Some(named(name).as_bytes())),
                Err(MemoryError::StateInvalid),
                "{name:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn every_listed_ip_gets_the_snapshot_epoch() -> TestResult {
        let mut state = State::default();
        let swarms: [SwarmSpec<'_>; 2] = [
            ("primary", true, &[(K1, &[A])]),
            ("worker-0", false, &[(K1, &[C])]),
        ];
        apply(&mut state, &snapshot(40, &swarms))?;
        apply(&mut state, &snapshot(41, &swarms))?;
        assert_eq!(ips(&state, K1, "primary"), Some(BTreeMap::from([(A, 41)])));
        assert_eq!(ips(&state, K1, "worker-0"), Some(BTreeMap::from([(C, 41)])));
        assert_eq!(state.epoch, 41);
        Ok(())
    }

    #[test]
    fn unresolved_key_keeps_its_ips_for_three_epochs() -> TestResult {
        let mut state = State::default();
        apply(&mut state, &primary(42, &[(K1, &[A]), (K2, &[B])]))?;
        // K1 stops resolving; K2 moves from B to C, so B is remembered the same way
        for epoch in [42, 43, 44] {
            apply(&mut state, &primary(epoch, &[(K1, &[]), (K2, &[C])]))?;
            assert_eq!(
                ips(&state, K1, "primary"),
                Some(BTreeMap::from([(A, 42)])),
                "{epoch}"
            );
            assert_eq!(allow_ips(&state), BTreeSet::from([A, B, C]), "{epoch}");
        }
        let outcome = apply(&mut state, &primary(45, &[(K1, &[]), (K2, &[C])]))?;
        assert_eq!(ips(&state, K1, "primary"), None);
        assert!(!state.keys.contains_key(&K1));
        assert_eq!(ips(&state, K2, "primary"), Some(BTreeMap::from([(C, 45)])));
        assert_eq!(allow_ips(&state), BTreeSet::from([C]));
        assert_eq!((outcome.expired, outcome.removed, outcome.keys), (2, 0, 1));
        // resolving again starts a new window
        apply(&mut state, &primary(46, &[(K1, &[A]), (K2, &[C])]))?;
        assert_eq!(ips(&state, K1, "primary"), Some(BTreeMap::from([(A, 46)])));
        Ok(())
    }

    #[test]
    fn key_missing_from_two_runs_is_removed() -> TestResult {
        let mut state = State::default();
        apply(&mut state, &primary(42, &[(K1, &[A]), (K2, &[B])]))?;
        apply(&mut state, &primary(43, &[(K1, &[A])]))?;
        assert_eq!(absent(&state, K2, "primary"), Some(1));
        assert_eq!(allow_ips(&state), BTreeSet::from([A, B]));
        let outcome = apply(&mut state, &primary(44, &[(K1, &[A])]))?;
        assert!(!state.keys.contains_key(&K2));
        assert_eq!(allow_ips(&state), BTreeSet::from([A]));
        assert_eq!((outcome.removed, outcome.expired, outcome.keys), (1, 0, 1));
        Ok(())
    }

    #[test]
    fn strikes_count_runs_not_epochs() -> TestResult {
        // two 5-minute runs within one epoch are two strikes
        let mut state = State::default();
        apply(&mut state, &primary(42, &[(K1, &[A]), (K2, &[B])]))?;
        apply(&mut state, &primary(42, &[(K1, &[A])]))?;
        assert_eq!(absent(&state, K2, "primary"), Some(1));
        apply(&mut state, &primary(42, &[(K1, &[A])]))?;
        assert!(!state.keys.contains_key(&K2));
        Ok(())
    }

    #[test]
    fn listing_between_misses_resets_the_strike() -> TestResult {
        let mut state = State::default();
        apply(&mut state, &primary(42, &[(K1, &[A]), (K2, &[B])]))?;
        apply(&mut state, &primary(42, &[(K1, &[A])]))?;
        assert_eq!(absent(&state, K2, "primary"), Some(1));
        // listed without an address: the strike clears and B stays under the TTL
        apply(&mut state, &primary(42, &[(K1, &[A]), (K2, &[])]))?;
        assert_eq!(absent(&state, K2, "primary"), Some(0));
        apply(&mut state, &primary(43, &[(K1, &[A])]))?;
        assert_eq!(absent(&state, K2, "primary"), Some(1));
        assert_eq!(ips(&state, K2, "primary"), Some(BTreeMap::from([(B, 42)])));
        apply(&mut state, &primary(43, &[(K1, &[A])]))?;
        assert!(!state.keys.contains_key(&K2));
        Ok(())
    }

    #[test]
    fn strike_removes_only_that_swarms_memory() -> TestResult {
        let mut state = State::default();
        let both: [SwarmSpec<'_>; 2] = [
            ("primary", true, &[(K1, &[A])]),
            ("worker-0", true, &[(K1, &[C])]),
        ];
        let primary_only: [SwarmSpec<'_>; 2] =
            [("primary", true, &[(K1, &[A])]), ("worker-0", true, &[])];
        apply(&mut state, &snapshot(42, &both))?;
        apply(&mut state, &snapshot(42, &primary_only))?;
        apply(&mut state, &snapshot(42, &primary_only))?;
        assert_eq!(ips(&state, K1, "primary"), Some(BTreeMap::from([(A, 42)])));
        assert_eq!(ips(&state, K1, "worker-0"), None);
        assert_eq!(allow_ips(&state), BTreeSet::from([A]));
        Ok(())
    }

    #[test]
    fn absent_worker_swarm_keeps_its_ips() -> TestResult {
        let mut state = State::default();
        let both: [SwarmSpec<'_>; 2] = [
            ("primary", true, &[(K1, &[A])]),
            ("worker-0", true, &[(K2, &[D])]),
        ];
        apply(&mut state, &snapshot(42, &both))?;
        // the worker swarm is missing from the snapshot: no strike, only the TTL
        for epoch in [42, 43, 44] {
            apply(&mut state, &primary(epoch, &[(K1, &[A])]))?;
            assert_eq!(absent(&state, K2, "worker-0"), Some(0), "{epoch}");
            assert_eq!(allow_ips(&state), BTreeSet::from([A, D]), "{epoch}");
        }
        apply(&mut state, &primary(45, &[(K1, &[A])]))?;
        assert_eq!(allow_ips(&state), BTreeSet::from([A]));
        Ok(())
    }

    #[test]
    fn unavailable_worker_refreshes_but_never_strikes() -> TestResult {
        let mut state = State::default();
        let full: [SwarmSpec<'_>; 2] = [
            ("primary", true, &[(K1, &[A]), (K2, &[B])]),
            ("worker-0", true, &[(K1, &[C]), (K2, &[D])]),
        ];
        // as `available: false` parses: entries carried, not authoritative
        let timed_out: [SwarmSpec<'_>; 2] = [
            ("primary", true, &[(K1, &[A]), (K2, &[B])]),
            ("worker-0", false, &[(K1, &[C])]),
        ];
        apply(&mut state, &snapshot(42, &full))?;
        for _ in 0..3 {
            apply(&mut state, &snapshot(43, &timed_out))?;
        }
        assert_eq!(ips(&state, K1, "worker-0"), Some(BTreeMap::from([(C, 43)])));
        assert_eq!(ips(&state, K2, "worker-0"), Some(BTreeMap::from([(D, 42)])));
        assert_eq!(absent(&state, K2, "worker-0"), Some(0));

        // a listing in a non-authoritative swarm also clears a strike
        let k2_missing: [SwarmSpec<'_>; 2] = [
            ("primary", true, &[(K1, &[A]), (K2, &[B])]),
            ("worker-0", true, &[(K1, &[C])]),
        ];
        let k2_listed: [SwarmSpec<'_>; 2] = [
            ("primary", true, &[(K1, &[A]), (K2, &[B])]),
            ("worker-0", false, &[(K2, &[D])]),
        ];
        apply(&mut state, &snapshot(43, &k2_missing))?;
        assert_eq!(absent(&state, K2, "worker-0"), Some(1));
        apply(&mut state, &snapshot(43, &k2_listed))?;
        assert_eq!(absent(&state, K2, "worker-0"), Some(0));
        assert_eq!(ips(&state, K2, "worker-0"), Some(BTreeMap::from([(D, 43)])));
        Ok(())
    }

    #[test]
    fn epoch_regression_leaves_state_byte_identical() -> TestResult {
        let mut state = State::default();
        apply(&mut state, &primary(42, &[(K1, &[A]), (K2, &[B])]))?;
        apply(&mut state, &primary(42, &[(K1, &[])]))?;
        let before = state.clone();
        let bytes = serialize(&state);
        let older = snapshot(
            41,
            &[
                ("primary", true, &[(K1, &[C])]),
                ("worker-0", true, &[(K2, &[D])]),
            ],
        );
        assert_eq!(apply(&mut state, &older), Err(MemoryError::EpochRegressed));
        assert_eq!(serialize(&state), bytes);
        assert_eq!(state, before);
        Ok(())
    }

    #[test]
    fn outcome_counts_memories() -> TestResult {
        let mut state = State::default();
        let shared: [SwarmSpec<'_>; 2] = [
            ("primary", true, &[(K1, &[A, B]), (K2, &[C])]),
            ("worker-0", true, &[(K2, &[A])]),
        ];
        let first = apply(&mut state, &snapshot(42, &shared))?;
        let expected = ApplyOutcome {
            keys: 2,
            added: 4,
            removed: 0,
            expired: 0,
        };
        assert_eq!(first, expected);
        // A is remembered under two keys and allowed once
        assert_eq!(allow_ips(&state), BTreeSet::from([A, B, C]));

        let second = apply(
            &mut state,
            &snapshot(43, &[("primary", true, &[(K1, &[A, D])])]),
        )?;
        assert_eq!(
            second,
            ApplyOutcome {
                keys: 2,
                added: 1,
                removed: 0,
                expired: 0
            }
        );
        let third = apply(
            &mut state,
            &snapshot(44, &[("primary", true, &[(K1, &[A])])]),
        )?;
        assert_eq!(
            third,
            ApplyOutcome {
                keys: 2,
                added: 0,
                removed: 1,
                expired: 0
            }
        );
        // worker-0 is gone from the snapshot, so K2's worker memory only ages
        assert_eq!(ips(&state, K2, "worker-0"), Some(BTreeMap::from([(A, 42)])));
        let fourth = apply(
            &mut state,
            &snapshot(45, &[("primary", true, &[(K1, &[A])])]),
        )?;
        assert_eq!(
            fourth,
            ApplyOutcome {
                keys: 1,
                added: 0,
                removed: 0,
                expired: 2
            }
        );
        assert_eq!(allow_ips(&state), BTreeSet::from([A, D]));
        Ok(())
    }

    #[test]
    fn tokens_follow_the_contract() {
        assert_eq!(MemoryError::StateInvalid.token(), "state-invalid");
        assert_eq!(MemoryError::EpochRegressed.token(), "epoch-regressed");
        assert_eq!(MemoryError::StateInvalid.to_string(), "state-invalid");
        assert_eq!(MemoryError::EpochRegressed.to_string(), "epoch-regressed");
    }
}
