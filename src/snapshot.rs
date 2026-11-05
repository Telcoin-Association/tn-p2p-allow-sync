//! The `tn_admissionSnapshot` result, version 1: the serde model, its validation and the parsed form the sync works on.
//!
//! [`parse`] takes the JSON-RPC `result` and returns either a [`Snapshot`] or the reason the snapshot is not authoritative.
//! Any such failure makes the whole snapshot stale: the caller keeps the loaded program and records [`SnapshotError::token`] as `STALE_REASON`.
//! Faults inside an otherwise valid snapshot are absorbed and counted in [`Warnings`]: unknown slot names, uppercase hex in a key, a key listed twice in one swarm, lists longer than [`MAX_MULTIADDRS_PER_LIST`], `dns*` multiaddrs and other multiaddrs without a leading IP.
//!
//! Two stale reasons need the sync's state and are not decided here: `epoch-regressed` (the snapshot's [`Snapshot::epoch`] below the applied one) and `empty-set`.
//!
//! The model ignores unknown fields at every level, so a v1 node can add fields without breaking this parser.
//! A change that an old parser must not read raises `version` instead.

use std::collections::btree_map::Entry as MapEntry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::addr::{AddrClass, MAX_MULTIADDRS_PER_LIST, canonical_ip, classify_multiaddr};

/// The snapshot `version` this build reads.
pub const SNAPSHOT_VERSION: u32 = 1;

/// The most swarms one snapshot may list: the primary and up to 15 workers.
pub const MAX_SWARMS: usize = 16;

/// The most entries one swarm may list.
pub const MAX_ENTRIES_PER_SWARM: usize = 10_000;

/// The `network` name of the primary swarm; a worker swarm is `worker-N`.
pub const PRIMARY_SWARM: &str = "primary";

/// Bytes in a compressed BLS12-381 public key, the form `bls_pubkey` carries.
pub const BLS_KEY_LEN: usize = 96;

/// Hex digits after the `0x` of a `bls_pubkey`: two per byte.
const BLS_KEY_HEX_DIGITS: usize = 192;

/// The `tn_admissionSnapshot` result as the node serialises it.
#[derive(Debug, Clone, Deserialize)]
pub struct RawSnapshot {
    /// The schema version; [`parse`] reads only [`SNAPSHOT_VERSION`].
    pub version: u32,
    /// The primary swarm's accepted admission epoch, or `None` before one is accepted.
    pub epoch: Option<u64>,
    /// One report per swarm: the primary and each worker.
    pub swarms: Vec<RawSwarm>,
}

/// One swarm's report in a [`RawSnapshot`].
#[derive(Debug, Clone, Deserialize)]
pub struct RawSwarm {
    /// The swarm's name: `primary`, or `worker-N` for worker `N`.
    pub network: String,
    /// The worker id of a worker swarm, or `None` for the primary.
    pub worker_id: Option<u16>,
    /// Whether the swarm answered the node in time.
    pub available: bool,
    /// The epoch of the latest scheduled and eligible key publication the swarm accepted, or `None` before the first.
    pub tracked_epoch: Option<u64>,
    /// The swarm's admission status, `None` exactly when `available` is false.
    pub admission: Option<RawAdmission>,
    /// Every key the swarm tracks.
    pub entries: Vec<RawEntry>,
}

/// A swarm's connection admission status in a [`RawSwarm`].
#[derive(Debug, Clone, Deserialize)]
pub struct RawAdmission {
    /// The mode the operator configured: `Open`, `Grace` or `Closed`.
    pub configured: String,
    /// The mode applied to connections, below `configured` while a fallback holds.
    pub effective: String,
    /// The latest accepted committee window epoch, or `None` before one is accepted.
    pub epoch: Option<u64>,
    /// Why `Closed` cannot be enforced (`Missing`, `Stale`, `Contradictory` or `Unresolved`), or `None` when nothing prevents it.
    pub fallback: Option<String>,
    /// Current committee identities with authenticated records, the local one included.
    pub resolved_current: u64,
    /// The resolved-record quorum `Closed` requires.
    pub required_current: u64,
    /// Connected current committee peers, the local identity excluded.
    pub connected_current: u64,
    /// Identities whose PeerId another identity also claims, or whose PeerId differs from the configured binding.
    pub contradicted: u64,
}

/// One tracked key in a [`RawSwarm`].
#[derive(Debug, Clone, Deserialize)]
pub struct RawEntry {
    /// The BLS public key: `0x` and 192 hex digits.
    pub bls_pubkey: String,
    /// The slots the key holds in the swarm, by name.
    pub slots: Vec<String>,
    /// The base58 PeerId of the held record, or `None` while no record is held.
    pub peer_id: Option<String>,
    /// The network public key of the held record, as `tn_info` serialises it, or `None` while no record is held.
    pub network_pubkey: Option<Value>,
    /// The addresses the swarm uses for the key.
    pub multiaddrs: Vec<String>,
    /// The signed record's own addresses, present only when a configured address replaces them in `multiaddrs`.
    pub record_multiaddrs: Option<Vec<String>>,
    /// The signed record's timestamp in seconds since the Unix epoch, or `None` while no signed record is held.
    pub record_timestamp: Option<u64>,
    /// Where `multiaddrs` came from: `record`, `config`, `stub` or `unresolved`.
    pub source: String,
    /// Whether the swarm admits the key's PeerId now.
    pub admitted: bool,
}

/// A 96-byte compressed BLS public key, the identity a snapshot keys its entries by.
///
/// `Display` and `Serialize` give `0x` and 192 lowercase hex digits, the form the node writes.
/// The order is by bytes, which is also the order of that text.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlsKey([u8; BLS_KEY_LEN]);

impl BlsKey {
    /// The key with these bytes.
    pub const fn from_bytes(bytes: [u8; BLS_KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The key's bytes.
    pub const fn as_bytes(&self) -> &[u8; BLS_KEY_LEN] {
        &self.0
    }

    /// Parses `0x` and 192 hex digits in any case, and says whether any digit was uppercase.
    ///
    /// The prefix is the lowercase `0x` only.
    /// A snapshot absorbs uppercase digits; a file this tool wrote never holds them.
    pub fn parse_reporting_case(s: &str) -> Result<(Self, bool), BlsKeyError> {
        let digits = s.strip_prefix("0x").ok_or(BlsKeyError)?;
        if digits.len() != BLS_KEY_HEX_DIGITS || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(BlsKeyError);
        }
        let mut bytes = [0u8; BLS_KEY_LEN];
        for (byte, pair) in bytes.iter_mut().zip(digits.as_bytes().chunks_exact(2)) {
            // the digits were checked above, so `from_str_radix` never sees the sign it would accept
            let pair = std::str::from_utf8(pair).map_err(|_| BlsKeyError)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| BlsKeyError)?;
        }
        let uppercase = digits.bytes().any(|b| b.is_ascii_uppercase());
        Ok((Self(bytes), uppercase))
    }
}

impl FromStr for BlsKey {
    type Err = BlsKeyError;

    /// Parses `0x` and 192 hex digits in any case; [`BlsKey::parse_reporting_case`] also says whether the case was upper.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_reporting_case(s).map(|(key, _)| key)
    }
}

impl fmt::Display for BlsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0x")?;
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for BlsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlsKey({self})")
    }
}

impl Serialize for BlsKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Why text is not a [`BlsKey`]: it is not `0x` followed by exactly 192 hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlsKeyError;

impl fmt::Display for BlsKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a BLS public key: expected 0x and 192 hex digits")
    }
}

impl std::error::Error for BlsKeyError {}

/// A reason a swarm tracks a key, read from an entry's `slots`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Slot {
    /// `previous`: the previous epoch's committee.
    Previous,
    /// `current`: the current epoch's committee.
    Current,
    /// `next`: the next epoch's committee.
    Next,
    /// `scheduled`: the committee two epochs ahead.
    Scheduled,
    /// `eligible`: the committee-eligible pool.
    Eligible,
    /// `trusted`: a configured trusted node.
    Trusted,
    /// `bootstrap`: a configured bootstrap peer.
    Bootstrap,
    /// A name this build does not know, counted in [`Warnings::unknown_slots`].
    Unknown,
}

impl Slot {
    /// The slot named `name`, matched case-sensitively as the node writes it; any other name is [`Slot::Unknown`].
    pub fn from_name(name: &str) -> Self {
        match name {
            "previous" => Self::Previous,
            "current" => Self::Current,
            "next" => Self::Next,
            "scheduled" => Self::Scheduled,
            "eligible" => Self::Eligible,
            "trusted" => Self::Trusted,
            "bootstrap" => Self::Bootstrap,
            _ => Self::Unknown,
        }
    }

    /// The slot's name in the snapshot, `unknown` for [`Slot::Unknown`].
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Previous => "previous",
            Self::Current => "current",
            Self::Next => "next",
            Self::Scheduled => "scheduled",
            Self::Eligible => "eligible",
            Self::Trusted => "trusted",
            Self::Bootstrap => "bootstrap",
            Self::Unknown => "unknown",
        }
    }
}

/// A validated snapshot, the input of one sync decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The primary swarm's `tracked_epoch`: the epoch whose pool the snapshot lists, which the sync compares with the applied epoch.
    pub epoch: u64,
    /// Every swarm, by `network` name.
    pub swarms: BTreeMap<String, Swarm>,
    /// What the snapshot held that was absorbed rather than fatal.
    pub warnings: Warnings,
}

/// One swarm of a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swarm {
    /// Whether the entries are the swarm's complete tracked set: it answered, its pool is published, and its fallback is null or `Unresolved`.
    ///
    /// The primary of a parsed snapshot is always authoritative.
    /// A worker swarm that is not is additions-only: its addresses are kept and refreshed, its missing keys are never removed.
    pub authoritative: bool,
    /// The epoch of the swarm's latest accepted pool publication, or `None` before the first.
    pub tracked_epoch: Option<u64>,
    /// Every key the swarm lists, duplicates merged.
    pub entries: BTreeMap<BlsKey, Entry>,
}

/// One key of a [`Swarm`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// Every slot the key holds in the swarm.
    pub slots: BTreeSet<Slot>,
    /// The canonical IPs of the first [`MAX_MULTIADDRS_PER_LIST`] addresses of `multiaddrs` and of `record_multiaddrs`, together.
    ///
    /// Addresses that [`canonical_ip`] drops are left out without a warning; they are never a peer's source.
    pub ips: BTreeSet<IpAddr>,
    /// Whether `record_multiaddrs` was present and its IPs differ from those of `multiaddrs`, in any listing of the key.
    ///
    /// The node then dials a configured address in place of the signed record's; both are allowed, and `check` reports the difference.
    pub mismatch: bool,
}

impl Entry {
    /// Folds a second listing of the same key into this one.
    fn merge(&mut self, other: Self) {
        self.slots.extend(other.slots);
        self.ips.extend(other.ips);
        self.mismatch = self.mismatch || other.mismatch;
    }
}

/// Counts of the faults a valid snapshot absorbed, over every swarm.
///
/// The sync logs one warning per nonzero kind and reports the counts in `status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Warnings {
    /// Slot names this build does not know, one per occurrence.
    pub unknown_slots: u64,
    /// Entries whose `bls_pubkey` had uppercase hex digits; the key is normalised.
    pub uppercase_keys: u64,
    /// Entries listing a key already listed in the same swarm; their slots and addresses are merged.
    pub duplicate_keys: u64,
    /// `dns`, `dns4`, `dns6` and `dnsaddr` multiaddrs, which are never resolved.
    pub dns_skipped: u64,
    /// Other multiaddrs without a leading `/ip4/` or `/ip6/` address.
    pub unparsed: u64,
    /// Address lists longer than [`MAX_MULTIADDRS_PER_LIST`], of which only the first entries are read.
    pub truncated_lists: u64,
}

/// Why a snapshot is not authoritative; [`SnapshotError::token`] is the `STALE_REASON`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotError {
    /// `snapshot-malformed`: `result` is not an object, `version` is missing or not an unsigned integer, the fields do not fit the v1 model, or the available primary has no admission status.
    Malformed,
    /// `snapshot-version-<n>`: a version other than [`SNAPSHOT_VERSION`].
    Version(u64),
    /// `snapshot-swarms`: no swarm, more than [`MAX_SWARMS`], a name other than `primary` or `worker-N`, or a name listed twice.
    Swarms,
    /// `snapshot-entries`: a swarm with more than [`MAX_ENTRIES_PER_SWARM`] entries.
    Entries,
    /// `snapshot-bls`: a `bls_pubkey` that is not `0x` and 192 hex digits.
    Bls,
    /// `primary-unavailable`: no primary swarm, or one with `available: false`.
    PrimaryUnavailable,
    /// `pool-unpublished`: the primary's `tracked_epoch` is null.
    PoolUnpublished,
    /// `fallback-missing`: the primary's fallback is `Missing`.
    FallbackMissing,
    /// `fallback-stale`: the primary's fallback is `Stale`.
    FallbackStale,
    /// `fallback-contradictory`: the primary's fallback is `Contradictory`, or a name this build does not know.
    FallbackContradictory,
}

impl SnapshotError {
    /// The `STALE_REASON` token of the failure.
    pub fn token(&self) -> String {
        match self {
            Self::Malformed => "snapshot-malformed".to_owned(),
            Self::Version(version) => format!("snapshot-version-{version}"),
            Self::Swarms => "snapshot-swarms".to_owned(),
            Self::Entries => "snapshot-entries".to_owned(),
            Self::Bls => "snapshot-bls".to_owned(),
            Self::PrimaryUnavailable => "primary-unavailable".to_owned(),
            Self::PoolUnpublished => "pool-unpublished".to_owned(),
            Self::FallbackMissing => "fallback-missing".to_owned(),
            Self::FallbackStale => "fallback-stale".to_owned(),
            Self::FallbackContradictory => "fallback-contradictory".to_owned(),
        }
    }
}

impl fmt::Display for SnapshotError {
    /// Prints the token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.token())
    }
}

impl std::error::Error for SnapshotError {}

/// Validates the `result` of `tn_admissionSnapshot` and reads it into a [`Snapshot`].
///
/// A snapshot with several faults reports the first in this order, which is the order of the failure table:
/// malformed, version, swarms, entries, BLS key, primary unavailable, pool unpublished, fallback.
/// `version` is read before the rest, so a later version that changed the shape still reports its version.
pub fn parse(result: Value) -> Result<Snapshot, SnapshotError> {
    let version = match &result {
        Value::Object(fields) => fields.get("version").and_then(Value::as_u64),
        _ => None,
    }
    .ok_or(SnapshotError::Malformed)?;
    if version != u64::from(SNAPSHOT_VERSION) {
        return Err(SnapshotError::Version(version));
    }
    let raw: RawSnapshot = serde_json::from_value(result).map_err(|_| SnapshotError::Malformed)?;
    check_swarm_names(&raw.swarms)?;
    if raw
        .swarms
        .iter()
        .any(|swarm| swarm.entries.len() > MAX_ENTRIES_PER_SWARM)
    {
        return Err(SnapshotError::Entries);
    }
    // read before the swarms are consumed, reported after their keys are checked
    let epoch = primary_epoch(&raw.swarms);
    let mut warnings = Warnings::default();
    let mut swarms = BTreeMap::new();
    for raw_swarm in raw.swarms {
        let (name, swarm) = read_swarm(raw_swarm, &mut warnings)?;
        swarms.insert(name, swarm);
    }
    Ok(Snapshot {
        epoch: epoch?,
        swarms,
        warnings,
    })
}

/// Checks the swarm count and that every name is valid and listed once.
fn check_swarm_names(swarms: &[RawSwarm]) -> Result<(), SnapshotError> {
    if swarms.is_empty() || swarms.len() > MAX_SWARMS {
        return Err(SnapshotError::Swarms);
    }
    let mut names = BTreeSet::new();
    for swarm in swarms {
        if !is_swarm_name(&swarm.network) || !names.insert(swarm.network.as_str()) {
            return Err(SnapshotError::Swarms);
        }
    }
    Ok(())
}

/// Whether `name` is `primary` or `worker-N`, with `N` a worker id written in plain decimal.
///
/// A leading zero is refused, so one worker cannot appear under two names.
pub(crate) fn is_swarm_name(name: &str) -> bool {
    if name == PRIMARY_SWARM {
        return true;
    }
    name.strip_prefix("worker-").is_some_and(|id| {
        let digits = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
        let leading_zero = id.len() > 1 && id.starts_with('0');
        digits && !leading_zero && id.parse::<u16>().is_ok()
    })
}

/// The snapshot's epoch, if the primary swarm makes the snapshot authoritative (rule 1 of the fail-open rules).
fn primary_epoch(swarms: &[RawSwarm]) -> Result<u64, SnapshotError> {
    let primary = swarms
        .iter()
        .find(|swarm| swarm.network == PRIMARY_SWARM)
        .filter(|primary| primary.available)
        .ok_or(SnapshotError::PrimaryUnavailable)?;
    let epoch = primary
        .tracked_epoch
        .ok_or(SnapshotError::PoolUnpublished)?;
    // an available swarm always reports its status, so a missing one is a broken schema
    let admission = primary.admission.as_ref().ok_or(SnapshotError::Malformed)?;
    match fallback_fault(admission.fallback.as_deref()) {
        Some(fault) => Err(fault),
        None => Ok(epoch),
    }
}

/// The failure a fallback means for membership, or `None` when the listed keys can be trusted.
fn fallback_fault(fallback: Option<&str>) -> Option<SnapshotError> {
    match fallback {
        // unresolved records leave the membership known; only their addresses are missing
        None | Some("Unresolved") => None,
        Some("Missing") => Some(SnapshotError::FallbackMissing),
        Some("Stale") => Some(SnapshotError::FallbackStale),
        // a fallback this build does not know cannot be shown harmless, so it reads as doubt
        Some(_) => Some(SnapshotError::FallbackContradictory),
    }
}

/// Reads one swarm whose name and size were checked; fails only on a bad key.
fn read_swarm(raw: RawSwarm, warnings: &mut Warnings) -> Result<(String, Swarm), SnapshotError> {
    let authoritative = raw.available
        && raw.tracked_epoch.is_some()
        && raw
            .admission
            .as_ref()
            .is_some_and(|admission| fallback_fault(admission.fallback.as_deref()).is_none());
    let mut entries = BTreeMap::new();
    for raw_entry in &raw.entries {
        let (key, uppercase) =
            BlsKey::parse_reporting_case(&raw_entry.bls_pubkey).map_err(|_| SnapshotError::Bls)?;
        if uppercase {
            bump(&mut warnings.uppercase_keys);
        }
        let entry = read_entry(raw_entry, warnings);
        match entries.entry(key) {
            MapEntry::Vacant(vacant) => {
                vacant.insert(entry);
            }
            MapEntry::Occupied(mut listed) => {
                bump(&mut warnings.duplicate_keys);
                listed.get_mut().merge(entry);
            }
        }
    }
    let swarm = Swarm {
        authoritative,
        tracked_epoch: raw.tracked_epoch,
        entries,
    };
    Ok((raw.network, swarm))
}

/// Reads one entry's slots and addresses.
fn read_entry(raw: &RawEntry, warnings: &mut Warnings) -> Entry {
    let mut slots = BTreeSet::new();
    for name in &raw.slots {
        let slot = Slot::from_name(name);
        if slot == Slot::Unknown {
            bump(&mut warnings.unknown_slots);
        }
        slots.insert(slot);
    }
    let mut ips = read_ips(&raw.multiaddrs, warnings);
    let mut mismatch = false;
    if let Some(record) = &raw.record_multiaddrs {
        let record_ips = read_ips(record, warnings);
        mismatch = record_ips != ips;
        ips.extend(record_ips);
    }
    Entry {
        slots,
        ips,
        mismatch,
    }
}

/// The canonical IPs of the first [`MAX_MULTIADDRS_PER_LIST`] multiaddrs of one list.
fn read_ips(multiaddrs: &[String], warnings: &mut Warnings) -> BTreeSet<IpAddr> {
    if multiaddrs.len() > MAX_MULTIADDRS_PER_LIST {
        bump(&mut warnings.truncated_lists);
    }
    let mut ips = BTreeSet::new();
    for multiaddr in multiaddrs.iter().take(MAX_MULTIADDRS_PER_LIST) {
        match classify_multiaddr(multiaddr) {
            // a dropped address is never a peer's source, so it is no fault either
            AddrClass::Ip(ip) => ips.extend(canonical_ip(ip)),
            AddrClass::Dns => bump(&mut warnings.dns_skipped),
            AddrClass::Unparsed => bump(&mut warnings.unparsed),
        }
    }
    ips
}

/// Adds one to a warning count.
fn bump(count: &mut u64) {
    *count = count.saturating_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::net::Ipv4Addr;

    type TestResult = Result<(), Box<dyn Error>>;

    const GOOD: &str = include_str!("../tests/fixtures/snapshot/good.json");
    const ABSORBED: &str = include_str!("../tests/fixtures/snapshot/absorbed.json");

    /// Every stored fixture that fails, with the token it must give.
    const FAILING: &[(&str, &str, &str)] = &[
        (
            "version-2",
            include_str!("../tests/fixtures/snapshot/version-2.json"),
            "snapshot-version-2",
        ),
        (
            "swarms-zero",
            include_str!("../tests/fixtures/snapshot/swarms-zero.json"),
            "snapshot-swarms",
        ),
        (
            "swarms-17",
            include_str!("../tests/fixtures/snapshot/swarms-17.json"),
            "snapshot-swarms",
        ),
        (
            "swarms-dup",
            include_str!("../tests/fixtures/snapshot/swarms-dup.json"),
            "snapshot-swarms",
        ),
        (
            "swarms-badname",
            include_str!("../tests/fixtures/snapshot/swarms-badname.json"),
            "snapshot-swarms",
        ),
        (
            "bls-short",
            include_str!("../tests/fixtures/snapshot/bls-short.json"),
            "snapshot-bls",
        ),
        (
            "bls-nonhex",
            include_str!("../tests/fixtures/snapshot/bls-nonhex.json"),
            "snapshot-bls",
        ),
        (
            "primary-missing",
            include_str!("../tests/fixtures/snapshot/primary-missing.json"),
            "primary-unavailable",
        ),
        (
            "primary-unavailable",
            include_str!("../tests/fixtures/snapshot/primary-unavailable.json"),
            "primary-unavailable",
        ),
        (
            "pool-unpublished",
            include_str!("../tests/fixtures/snapshot/pool-unpublished.json"),
            "pool-unpublished",
        ),
        (
            "fallback-missing",
            include_str!("../tests/fixtures/snapshot/fallback-missing.json"),
            "fallback-missing",
        ),
        (
            "fallback-stale",
            include_str!("../tests/fixtures/snapshot/fallback-stale.json"),
            "fallback-stale",
        ),
        (
            "fallback-contradictory",
            include_str!("../tests/fixtures/snapshot/fallback-contradictory.json"),
            "fallback-contradictory",
        ),
        (
            "malformed-not-object",
            include_str!("../tests/fixtures/snapshot/malformed-not-object.json"),
            "snapshot-malformed",
        ),
    ];

    fn value(text: &str) -> Result<Value, Box<dyn Error>> {
        Ok(serde_json::from_str(text)?)
    }

    fn token_of(result: Value) -> Option<String> {
        parse(result).err().map(|e| e.token())
    }

    fn at<'a>(result: &'a mut Value, pointer: &str) -> Result<&'a mut Value, Box<dyn Error>> {
        result
            .pointer_mut(pointer)
            .ok_or_else(|| format!("no {pointer} in the fixture").into())
    }

    /// Sets `field` of the object at `object`, adding the field when it is absent.
    fn set(result: &mut Value, object: &str, field: &str, to: Value) -> TestResult {
        at(result, object)?
            .as_object_mut()
            .ok_or_else(|| format!("{object} is not an object"))?
            .insert(field.to_owned(), to);
        Ok(())
    }

    fn push_swarm(result: &mut Value, swarm: &str) -> TestResult {
        at(result, "/swarms")?
            .as_array_mut()
            .ok_or("swarms is not an array")?
            .push(value(swarm)?);
        Ok(())
    }

    fn swarm<'a>(snapshot: &'a Snapshot, name: &str) -> Result<&'a Swarm, Box<dyn Error>> {
        snapshot
            .swarms
            .get(name)
            .ok_or_else(|| format!("no swarm {name}").into())
    }

    fn entry<'a>(swarm: &'a Swarm, key: &str) -> Result<&'a Entry, Box<dyn Error>> {
        swarm
            .entries
            .get(&key.parse::<BlsKey>()?)
            .ok_or_else(|| format!("no entry {key}").into())
    }

    /// A key of the 5.3 example: four hex digits, zeros, four hex digits.
    fn example_key(head: &str, tail: &str) -> String {
        format!("0x{head}{}{tail}", "0".repeat(184))
    }

    fn repeated_key(pair: &str) -> String {
        format!("0x{}", pair.repeat(96))
    }

    const fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    /// A worker swarm that timed out, listing one key with one address.
    fn unavailable_worker(name: &str, key: &str) -> String {
        format!(
            r#"{{"network": "{name}", "worker_id": 0, "available": false, "tracked_epoch": null,
                "admission": null, "entries": [{{"bls_pubkey": "{key}", "slots": ["current"],
                "peer_id": null, "network_pubkey": null, "multiaddrs": ["/ip4/192.0.2.50/udp/49594/quic-v1"],
                "record_timestamp": null, "source": "record", "admitted": true}}]}}"#
        )
    }

    #[test]
    fn every_failing_fixture_gives_its_token() -> TestResult {
        for (name, text, token) in FAILING {
            assert_eq!(token_of(value(text)?).as_deref(), Some(*token), "{name}");
        }
        Ok(())
    }

    #[test]
    fn good_fixture_parses_to_the_schema_example() -> TestResult {
        let snapshot = parse(value(GOOD)?)?;
        assert_eq!(snapshot.epoch, 42);
        assert_eq!(snapshot.warnings, Warnings::default());
        assert_eq!(
            snapshot
                .swarms
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [PRIMARY_SWARM]
        );
        let primary = swarm(&snapshot, PRIMARY_SWARM)?;
        assert!(primary.authoritative);
        assert_eq!(primary.tracked_epoch, Some(42));
        let expected = BTreeMap::from([
            (
                example_key("8f3a", "c21d").parse::<BlsKey>()?,
                Entry {
                    slots: BTreeSet::from([Slot::Current, Slot::Eligible]),
                    ips: BTreeSet::from([v4(203, 0, 113, 10)]),
                    mismatch: false,
                },
            ),
            (
                example_key("a71e", "09b4").parse::<BlsKey>()?,
                Entry {
                    slots: BTreeSet::from([Slot::Eligible]),
                    ips: BTreeSet::from([v4(198, 51, 100, 7)]),
                    mismatch: false,
                },
            ),
        ]);
        assert_eq!(primary.entries, expected);
        Ok(())
    }

    #[test]
    fn absorbed_fixture_counts_each_warning_once_and_merges_the_duplicate() -> TestResult {
        let snapshot = parse(value(ABSORBED)?)?;
        assert_eq!(
            snapshot.warnings,
            Warnings {
                unknown_slots: 1,
                uppercase_keys: 1,
                duplicate_keys: 1,
                dns_skipped: 1,
                unparsed: 1,
                truncated_lists: 1,
            }
        );
        let primary = swarm(&snapshot, PRIMARY_SWARM)?;
        assert!(primary.authoritative);
        assert_eq!(primary.entries.len(), 2);
        // the uppercase listing and the lowercase one are the same key; the mapped address is IPv4
        // and the link-local one is dropped without a warning
        assert_eq!(
            entry(primary, &repeated_key("a1"))?,
            &Entry {
                slots: BTreeSet::from([Slot::Current, Slot::Next, Slot::Unknown]),
                ips: BTreeSet::from([v4(192, 0, 2, 1), v4(192, 0, 2, 2), v4(192, 0, 2, 3)]),
                mismatch: false,
            }
        );
        // only the first 32 of the 33 addresses are read
        let long = entry(primary, &repeated_key("b2"))?;
        assert_eq!(
            long.ips,
            (1..=32)
                .map(|d| v4(198, 51, 100, d))
                .collect::<BTreeSet<_>>()
        );
        assert!(!long.ips.contains(&v4(198, 51, 100, 33)));
        Ok(())
    }

    #[test]
    fn more_than_10000_entries_fail_and_10000_pass() -> TestResult {
        assert_eq!(MAX_ENTRIES_PER_SWARM, 10_000);
        let mut over = value(GOOD)?;
        let first = at(&mut over, "/swarms/0/entries/0")?.clone();
        *at(&mut over, "/swarms/0/entries")? = Value::Array(vec![first.clone(); 10_001]);
        assert_eq!(token_of(over).as_deref(), Some("snapshot-entries"));

        let mut full = value(GOOD)?;
        *at(&mut full, "/swarms/0/entries")? = Value::Array(vec![first; 10_000]);
        let snapshot = parse(full)?;
        assert_eq!(snapshot.warnings.duplicate_keys, 9_999);
        assert_eq!(swarm(&snapshot, PRIMARY_SWARM)?.entries.len(), 1);
        Ok(())
    }

    #[test]
    fn an_unavailable_worker_is_not_authoritative_and_keeps_its_entries() -> TestResult {
        let mut result = value(GOOD)?;
        push_swarm(
            &mut result,
            &unavailable_worker("worker-0", &repeated_key("c3")),
        )?;
        let snapshot = parse(result)?;
        assert!(swarm(&snapshot, PRIMARY_SWARM)?.authoritative);
        let worker = swarm(&snapshot, "worker-0")?;
        assert!(!worker.authoritative);
        assert_eq!(worker.tracked_epoch, None);
        // an additions-only swarm still refreshes the addresses it lists
        assert_eq!(
            entry(worker, &repeated_key("c3"))?.ips,
            BTreeSet::from([v4(192, 0, 2, 50)])
        );
        Ok(())
    }

    #[test]
    fn worker_doubts_are_not_fatal() -> TestResult {
        // `None` is an available swarm without a status, whose fallback cannot be read
        for (tracked_epoch, fallback, authoritative) in [
            ("42", Some("null"), true),
            ("42", Some(r#""Unresolved""#), true),
            ("null", Some("null"), false),
            ("42", Some(r#""Missing""#), false),
            ("42", Some(r#""Stale""#), false),
            ("42", Some(r#""Contradictory""#), false),
            ("42", Some(r#""Lost""#), false),
            ("42", None, false),
        ] {
            let mut result = value(GOOD)?;
            let mut worker = at(&mut result, "/swarms/0")?.clone();
            set(&mut worker, "", "network", "worker-3".into())?;
            set(&mut worker, "", "tracked_epoch", value(tracked_epoch)?)?;
            match fallback {
                Some(fallback) => set(&mut worker, "/admission", "fallback", value(fallback)?)?,
                None => set(&mut worker, "", "admission", Value::Null)?,
            }
            push_swarm(&mut result, &worker.to_string())?;
            let snapshot = parse(result)?;
            assert!(swarm(&snapshot, PRIMARY_SWARM)?.authoritative);
            assert_eq!(
                swarm(&snapshot, "worker-3")?.authoritative,
                authoritative,
                "{tracked_epoch} {fallback:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn primary_fallback_unresolved_is_authoritative_and_unknown_is_contradictory() -> TestResult {
        let mut unresolved = value(GOOD)?;
        *at(&mut unresolved, "/swarms/0/admission/fallback")? = "Unresolved".into();
        assert!(swarm(&parse(unresolved)?, PRIMARY_SWARM)?.authoritative);

        for fallback in ["Lost", "missing", ""] {
            let mut result = value(GOOD)?;
            *at(&mut result, "/swarms/0/admission/fallback")? = fallback.into();
            assert_eq!(
                token_of(result).as_deref(),
                Some("fallback-contradictory"),
                "{fallback:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn an_available_primary_without_admission_is_malformed() -> TestResult {
        let mut result = value(GOOD)?;
        *at(&mut result, "/swarms/0/admission")? = Value::Null;
        assert_eq!(token_of(result).as_deref(), Some("snapshot-malformed"));
        Ok(())
    }

    #[test]
    fn the_epoch_is_the_primary_tracked_epoch() -> TestResult {
        let mut result = value(GOOD)?;
        *at(&mut result, "/epoch")? = Value::Null;
        *at(&mut result, "/swarms/0/tracked_epoch")? = 43.into();
        assert_eq!(parse(result)?.epoch, 43);
        Ok(())
    }

    #[test]
    fn version_is_read_before_the_shape() -> TestResult {
        for (text, token) in [
            (
                r#"{"version": 2, "swarms": {"primary": {}}}"#,
                "snapshot-version-2",
            ),
            (r#"{"version": 0}"#, "snapshot-version-0"),
            (
                r#"{"version": 1099511627776, "swarms": []}"#,
                "snapshot-version-1099511627776",
            ),
            (r#"{"swarms": []}"#, "snapshot-malformed"),
            (r#"{"version": "1", "swarms": []}"#, "snapshot-malformed"),
            (r#"{"version": -1, "swarms": []}"#, "snapshot-malformed"),
            (r#"{"version": 1.0, "swarms": []}"#, "snapshot-malformed"),
            (r#"{"version": null, "swarms": []}"#, "snapshot-malformed"),
        ] {
            assert_eq!(token_of(value(text)?).as_deref(), Some(token), "{text}");
        }
        Ok(())
    }

    #[test]
    fn shapes_outside_the_model_are_malformed() -> TestResult {
        for text in ["null", "true", "1", r#""snapshot""#, "[]", "{}"] {
            assert_eq!(
                token_of(value(text)?).as_deref(),
                Some("snapshot-malformed"),
                "{text}"
            );
        }
        for (object, field, replacement) in [
            ("", "swarms", "{}"),
            ("/swarms/0", "available", r#""yes""#),
            ("/swarms/0", "worker_id", "70000"),
            ("/swarms/0", "tracked_epoch", "-1"),
            ("/swarms/0/admission", "resolved_current", "-4"),
            ("/swarms/0/entries/0", "bls_pubkey", "null"),
            ("/swarms/0/entries/0", "slots", r#""current""#),
            ("/swarms/0/entries/0", "multiaddrs", "null"),
            (
                "/swarms/0/entries/0",
                "record_multiaddrs",
                r#""/ip4/192.0.2.1""#,
            ),
        ] {
            let mut result = value(GOOD)?;
            set(&mut result, object, field, value(replacement)?)?;
            assert_eq!(
                token_of(result).as_deref(),
                Some("snapshot-malformed"),
                "{object}/{field}"
            );
        }
        Ok(())
    }

    #[test]
    fn unknown_fields_are_ignored_at_every_level() -> TestResult {
        let mut result = value(GOOD)?;
        for (pointer, field) in [
            ("", "generated_at"),
            ("/swarms/0", "kad_records"),
            ("/swarms/0/admission", "grace_until"),
            ("/swarms/0/entries/0", "network_key_verified"),
        ] {
            set(&mut result, pointer, field, Value::Bool(true))?;
        }
        assert_eq!(parse(result)?, parse(value(GOOD)?)?);
        Ok(())
    }

    #[test]
    fn swarm_names_and_count() -> TestResult {
        for name in ["worker-0", "worker-7", "worker-65535"] {
            let mut result = value(GOOD)?;
            push_swarm(&mut result, &unavailable_worker(name, &repeated_key("c3")))?;
            assert!(parse(result)?.swarms.contains_key(name), "{name}");
        }
        for name in [
            "",
            "Primary",
            "worker",
            "worker-",
            "worker-01",
            "worker-00",
            "worker-+1",
            "worker--1",
            "worker-1 ",
            "worker-x",
            "worker-65536",
            "Worker-1",
            "worker_1",
        ] {
            let mut result = value(GOOD)?;
            push_swarm(&mut result, &unavailable_worker(name, &repeated_key("c3")))?;
            assert_eq!(
                token_of(result).as_deref(),
                Some("snapshot-swarms"),
                "{name:?}"
            );
        }
        // the cap is inclusive: the primary and 15 workers parse
        let mut result = value(GOOD)?;
        for id in 0..15 {
            push_swarm(
                &mut result,
                &unavailable_worker(&format!("worker-{id}"), &repeated_key("c3")),
            )?;
        }
        assert_eq!(parse(result)?.swarms.len(), MAX_SWARMS);
        Ok(())
    }

    #[test]
    fn record_multiaddrs_are_read_and_compared_by_ip() -> TestResult {
        for (record, mismatch, ips) in [
            (None, false, vec![v4(203, 0, 113, 10)]),
            (
                Some(r#"["/ip4/203.0.113.10/udp/49600/quic-v1"]"#),
                false,
                vec![v4(203, 0, 113, 10)],
            ),
            (
                Some(r#"["/ip6/::ffff:203.0.113.10/udp/49590/quic-v1"]"#),
                false,
                vec![v4(203, 0, 113, 10)],
            ),
            (
                Some(r#"["/ip4/192.0.2.77/udp/49590/quic-v1"]"#),
                true,
                vec![v4(192, 0, 2, 77), v4(203, 0, 113, 10)],
            ),
            (Some("[]"), true, vec![v4(203, 0, 113, 10)]),
        ] {
            let mut result = value(GOOD)?;
            *at(&mut result, "/swarms/0/entries/0/source")? = "stub".into();
            if let Some(record) = record {
                set(
                    &mut result,
                    "/swarms/0/entries/0",
                    "record_multiaddrs",
                    value(record)?,
                )?;
            }
            let snapshot = parse(result)?;
            let stub = entry(
                swarm(&snapshot, PRIMARY_SWARM)?,
                &example_key("8f3a", "c21d"),
            )?;
            assert_eq!(stub.mismatch, mismatch, "{record:?}");
            assert_eq!(stub.ips, ips.into_iter().collect(), "{record:?}");
        }
        Ok(())
    }

    #[test]
    fn faults_report_in_table_order() -> TestResult {
        // a bad key in a worker outranks an unavailable primary
        let mut result = value(GOOD)?;
        *at(&mut result, "/swarms/0/available")? = false.into();
        push_swarm(&mut result, &unavailable_worker("worker-0", "0xc3"))?;
        assert_eq!(token_of(result).as_deref(), Some("snapshot-bls"));

        // a bad swarm name outranks an oversized swarm
        let mut result = value(GOOD)?;
        let first = at(&mut result, "/swarms/0/entries/0")?.clone();
        *at(&mut result, "/swarms/0/entries")? = Value::Array(vec![first; 10_001]);
        push_swarm(
            &mut result,
            &unavailable_worker("worker-01", &repeated_key("c3")),
        )?;
        assert_eq!(token_of(result).as_deref(), Some("snapshot-swarms"));

        // an unpublished pool outranks a fallback
        let mut result = value(GOOD)?;
        *at(&mut result, "/swarms/0/tracked_epoch")? = Value::Null;
        *at(&mut result, "/swarms/0/admission/fallback")? = "Missing".into();
        assert_eq!(token_of(result).as_deref(), Some("pool-unpublished"));
        Ok(())
    }

    #[test]
    fn every_token_matches_the_failure_table() {
        let tokens = [
            SnapshotError::Malformed,
            SnapshotError::Version(2),
            SnapshotError::Swarms,
            SnapshotError::Entries,
            SnapshotError::Bls,
            SnapshotError::PrimaryUnavailable,
            SnapshotError::PoolUnpublished,
            SnapshotError::FallbackMissing,
            SnapshotError::FallbackStale,
            SnapshotError::FallbackContradictory,
        ]
        .map(|e| (e.token(), e.to_string()));
        for (token, displayed) in &tokens {
            assert_eq!(token, displayed);
        }
        assert_eq!(
            tokens.map(|(token, _)| token),
            [
                "snapshot-malformed",
                "snapshot-version-2",
                "snapshot-swarms",
                "snapshot-entries",
                "snapshot-bls",
                "primary-unavailable",
                "pool-unpublished",
                "fallback-missing",
                "fallback-stale",
                "fallback-contradictory",
            ]
        );
    }

    #[test]
    fn bls_key_text_round_trips_in_lowercase() -> TestResult {
        let lower = example_key("8f3a", "c21d");
        let (key, uppercase) = BlsKey::parse_reporting_case(&lower)?;
        assert!(!uppercase);
        assert_eq!(key.to_string(), lower);
        assert_eq!(format!("{key:?}"), format!("BlsKey({lower})"));
        assert_eq!(key.as_bytes().first(), Some(&0x8f));
        assert_eq!(key.as_bytes().last(), Some(&0x1d));
        assert_eq!(BlsKey::from_bytes(*key.as_bytes()), key);

        let mixed = example_key("8F3a", "C21D");
        let (same, uppercase) = BlsKey::parse_reporting_case(&mixed)?;
        assert!(uppercase);
        assert_eq!(same, key);
        assert_eq!(mixed.parse::<BlsKey>()?, key);
        assert_eq!(same.to_string(), lower);
        Ok(())
    }

    #[test]
    fn bls_key_order_is_the_order_of_its_text() -> TestResult {
        let texts = [
            repeated_key("00"),
            example_key("0000", "0001"),
            example_key("8f3a", "c21d"),
            example_key("a71e", "09b4"),
            repeated_key("ff"),
        ];
        let keys = texts
            .iter()
            .map(|text| text.parse::<BlsKey>())
            .collect::<Result<BTreeSet<_>, _>>()?;
        assert_eq!(
            keys.iter().map(BlsKey::to_string).collect::<Vec<_>>(),
            texts
        );
        Ok(())
    }

    #[test]
    fn bls_key_refuses_anything_but_0x_and_192_hex_digits() {
        let good = example_key("8f3a", "c21d");
        for text in [
            String::new(),
            "0x".to_owned(),
            good.replacen("0x", "0X", 1),
            good.replacen("0x", "", 1),
            good.replacen("0x", " 0x", 1),
            format!("{good}0"),
            format!("{good}00"),
            good.replacen("0x8f", "0x", 1),
            good.replacen("0x8f", "0x+f", 1),
            good.replacen("0x8f", "0x-f", 1),
            good.replacen("0x8f", "0x g", 1),
            good.replacen("0x8f", "0x\u{e9}", 1),
            format!("{good} "),
        ] {
            assert_eq!(text.parse::<BlsKey>(), Err(BlsKeyError), "{text:?}");
        }
    }

    #[test]
    fn bls_key_serializes_as_its_text_even_as_a_map_key() -> TestResult {
        let key = example_key("a71e", "09b4").parse::<BlsKey>()?;
        assert_eq!(
            serde_json::to_string(&BTreeMap::from([(key, 42)]))?,
            format!("{{\"{key}\":42}}")
        );
        assert_eq!(serde_json::to_string(&key)?, format!("\"{key}\""));
        Ok(())
    }

    #[test]
    fn slot_names_round_trip_and_match_case_sensitively() {
        for slot in [
            Slot::Previous,
            Slot::Current,
            Slot::Next,
            Slot::Scheduled,
            Slot::Eligible,
            Slot::Trusted,
            Slot::Bootstrap,
        ] {
            assert_eq!(Slot::from_name(slot.as_str()), slot);
        }
        for name in ["Current", "unknown", "observer", ""] {
            assert_eq!(Slot::from_name(name), Slot::Unknown, "{name:?}");
        }
    }

    #[test]
    fn the_version_matches_the_version_line() {
        assert_eq!(crate::cli::SNAPSHOT_VERSIONS, [u64::from(SNAPSHOT_VERSION)]);
    }
}
