//! Seeded property tests of memory: `state.json` round trips and the fold rules, over random sequences of snapshots.
//!
//! A seed gives one sequence of [`RUNS`] snapshots, written as the node serialises them and changing the way a
//! committee does: keys join and leave each swarm and change addresses, some are listed `unresolved`, in uppercase hex
//! or twice, workers come and go and lose their authority, the primary now and then makes a run stale, and the epoch
//! mostly holds, sometimes steps or jumps, and sometimes goes back.
//! Every snapshot goes through [`snapshot::parse`], as in a sync; a stale one never reaches memory, and every valid one
//! is folded with [`apply`] into a [`State`] that starts empty.
//! The sequences of the seeds `1..=SEEDS` are generated once, shared by every property, and checked a share per core.
//! A failure names the seed and the sequence up to the failing fold, in the notation of [`Gen::run`].
//!
//! The properties, one test each:
//!
//! - `state_json_round_trips`: after every fold, [`load_state`] reads back what [`serialize`] wrote, byte for byte,
//!   and the bytes are one line ending in a newline, with no empty map.
//! - `address_unlisted_for_ttl_epochs_is_gone` (rule 2): an address not listed again is still held after the fold at
//!   `seen + 2` and gone after the fold at `seen + 3`, or after the first fold past it when the epoch jumps.
//! - `two_consecutive_authoritative_omissions_remove_a_key` (rule 3): the strike count of a key in a swarm is the number
//!   of authoritative runs of that swarm that omitted it since its last listing there, and at [`STRIKES`] the memory goes.
//! - `additions_only_swarm_never_strikes_or_removes` (rule 4): under a swarm the snapshot lacks or holds as
//!   non-authoritative, a remembered key keeps its strike count and loses addresses only to the TTL.
//! - `epoch_is_monotone_and_a_regressed_snapshot_changes_nothing`: a snapshot below the state's epoch is
//!   [`MemoryError::EpochRegressed`] and leaves the state byte-identical; any other moves the epoch to its own.
//! - `reapplying_a_snapshot_only_completes_its_strikes`: a second run of the same snapshot removes exactly the
//!   memories its first run struck and changes nothing else, and a third run changes nothing at all.
//! - `allow_ips_is_every_remembered_address`: [`allow_ips`] is the union of every remembered address, each one its
//!   own [`canonical_ip`].
//!
//! Three hand-written sequences pin the edges the properties count on: the TTL boundary, a strike reset by a listing,
//! and the second run of a snapshot that struck a key.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display, Write as _};
use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::panic::resume_unwind;
use std::sync::OnceLock;
use std::thread;

use serde_json::{Value, json};

use support::rng::Rng;
use tn_p2p_allow_sync::addr::canonical_ip;
use tn_p2p_allow_sync::memory::{
    ApplyOutcome, MemoryError, STRIKES, State, SwarmMemory, TTL_EPOCHS, allow_ips, apply,
    load_state, serialize,
};
use tn_p2p_allow_sync::snapshot::{self, BlsKey, Entry, Snapshot, SnapshotError};

type TestResult = Result<(), Box<dyn Error>>;
type Res<T> = Result<T, Box<dyn Error>>;

/// Seeds, each one sequence that every property checks.
const SEEDS: u64 = 200;

/// Snapshots in one sequence.
const RUNS: usize = 12;

/// Keys a sequence draws from: key `i` is `k<i>` in a sequence's description, and a swarm lists 0 to all of them.
const KEYS: u8 = 6;

/// The swarms a sequence draws from: the primary is in every snapshot, each worker in about half.
const SWARMS: [&str; 4] = ["primary", "worker-0", "worker-1", "worker-7"];

/// The multiaddrs an entry draws from, by index; the first [`PLAIN`] give an address, the rest mostly do not.
const ADDRS: [&str; 20] = [
    "/ip4/192.0.2.10/udp/49590/quic-v1",
    "/ip4/192.0.2.11/udp/49590/quic-v1",
    "/ip4/198.51.100.7/udp/49594/quic-v1",
    "/ip4/10.0.0.1/udp/49590/quic-v1",
    "/ip4/203.0.113.200",
    "/ip6/2001:db8::1/udp/49590/quic-v1",
    "/ip6/2001:db8:1::ff/udp/49590/quic-v1",
    // a spelling of 2001:db8::2 that is not its canonical text
    "/ip6/2001:DB8:0:0::2/udp/49590/quic-v1",
    // IPv4-mapped: the same address as index 0, and a new IPv4 one
    "/ip6/::ffff:192.0.2.10/udp/49590/quic-v1",
    "/ip6/::ffff:198.51.100.8/udp/49590/quic-v1",
    // loopback is kept, and the compatible form stays IPv6
    "/ip6/::1/udp/49590/quic-v1",
    "/ip4/127.0.0.1/udp/49590/quic-v1",
    // dropped by canonical_ip: multicast, link-local, unspecified, and link-local after the mapping
    "/ip4/224.0.0.251/udp/5353",
    "/ip4/169.254.10.1/udp/49590/quic-v1",
    "/ip4/0.0.0.0/udp/49590/quic-v1",
    "/ip6/fe80::1/udp/49590/quic-v1",
    "/ip6/ff02::1/udp/49590/quic-v1",
    "/ip6/::ffff:169.254.1.1/udp/49590/quic-v1",
    // no leading IP: skipped as dns, and unparsed
    "/dns4/node.example/udp/49590/quic-v1",
    "/udp/49590",
];

/// How many of [`ADDRS`], from the start, give an address that memory keeps.
const PLAIN: u64 = 12;

/// Every address [`ADDRS`] can put in memory, in canonical text.
const CANONICAL: [&str; 11] = [
    "192.0.2.10",
    "192.0.2.11",
    "198.51.100.7",
    "10.0.0.1",
    "203.0.113.200",
    "2001:db8::1",
    "2001:db8:1::ff",
    "2001:db8::2",
    "198.51.100.8",
    "::1",
    "127.0.0.1",
];

/// How a generated swarm reports itself, which decides whether parse calls it authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    /// Available, tracked at the epoch, its admission window at the epoch and no fallback.
    Sound,
    /// As [`Health::Sound`] with a window of unknown epoch.
    NoWindow,
    /// As [`Health::Sound`] with the `Unresolved` fallback, which leaves the membership known.
    Unresolved,
    /// As [`Health::Sound`] with the pool one epoch ahead of its window.
    Ahead,
    /// Not available.
    Unavailable,
    /// Tracked at the epoch, its window one epoch later: the pool is behind.
    Behind,
    /// No pool publication yet: `tracked_epoch` is null.
    Unpublished,
    /// Available with `admission: null`.
    NoAdmission,
    /// A fallback that casts doubt on the membership.
    Fallback(&'static str),
}

/// The healths of an authoritative swarm.
const AUTHORITATIVE: [Health; 4] = [
    Health::Sound,
    Health::NoWindow,
    Health::Unresolved,
    Health::Ahead,
];

/// The healths of a swarm that is not authoritative; a primary in one of them makes the snapshot stale.
const DOUBTFUL: [Health; 7] = [
    Health::Unavailable,
    Health::Behind,
    Health::Unpublished,
    Health::NoAdmission,
    Health::Fallback("Missing"),
    Health::Fallback("Stale"),
    Health::Fallback("Contradictory"),
];

impl Health {
    /// Its letter in a sequence's description: `A` for every authoritative health.
    const fn code(self) -> &'static str {
        match self {
            Self::Sound | Self::NoWindow | Self::Unresolved | Self::Ahead => "A",
            Self::Unavailable => "U",
            Self::Behind => "B",
            Self::Unpublished => "P",
            Self::NoAdmission => "N",
            Self::Fallback(_) => "F",
        }
    }
}

/// One listing of a key, as the generator writes it into a swarm's `entries`.
#[derive(Debug, Clone, Default)]
struct EntrySpec {
    /// The key's index, below [`KEYS`].
    key: u8,
    /// `multiaddrs`, as indices into [`ADDRS`].
    addrs: Vec<usize>,
    /// `record_multiaddrs`, as indices into [`ADDRS`], when present.
    record: Option<Vec<usize>>,
    /// Listed with `source: unresolved` and no address.
    unresolved: bool,
    /// The key written in uppercase hex, which parse folds to the same key.
    upper: bool,
}

impl EntrySpec {
    /// The entry as the node serialises it.
    fn json(&self) -> Res<Value> {
        Ok(json!({
            "bls_pubkey": key_text(self.key, self.upper)?,
            "slots": ["current", "eligible"],
            "peer_id": format!("12D3KooWk{}", self.key),
            "network_pubkey": null,
            "multiaddrs": multiaddrs(&self.addrs)?,
            "record_multiaddrs": self.record.as_deref().map(multiaddrs).transpose()?,
            "record_timestamp": 1_793_750_400_u64,
            "source": if self.unresolved { "unresolved" } else { "record" },
            "admitted": true,
        }))
    }

    /// Appends the entry's notation: `k3:0.8` lists key 3 with addresses 0 and 8 of [`ADDRS`], `^` marks uppercase
    /// hex, `:u` an unresolved listing, `:-` one without addresses, and `+r5` record address 5.
    fn describe(&self, out: &mut String) -> fmt::Result {
        write!(out, "k{}", self.key)?;
        if self.upper {
            out.push('^');
        }
        if self.unresolved {
            out.push_str(":u");
            return Ok(());
        }
        out.push(':');
        join_indices(out, &self.addrs)?;
        if let Some(record) = &self.record {
            out.push_str("+r");
            join_indices(out, record)?;
        }
        Ok(())
    }
}

/// Appends `indices` joined by `.`, or `-` when there are none.
fn join_indices(out: &mut String, indices: &[usize]) -> fmt::Result {
    if indices.is_empty() {
        out.push('-');
    }
    for (n, index) in indices.iter().enumerate() {
        if n > 0 {
            out.push('.');
        }
        write!(out, "{index}")?;
    }
    Ok(())
}

/// The multiaddr texts of `indices`.
fn multiaddrs(indices: &[usize]) -> Res<Vec<&'static str>> {
    indices
        .iter()
        .map(|index| {
            ADDRS
                .get(*index)
                .copied()
                .ok_or_else(|| format!("no address {index}").into())
        })
        .collect()
}

/// The `bls_pubkey` text of key `index`, lowercase or uppercase: `0x` and 192 hex digits, the first byte
/// `0x80 | index`. Built once per key, since formatting 96 bytes is slow in a debug build.
fn key_text(index: u8, upper: bool) -> Res<&'static str> {
    static TEXTS: OnceLock<Vec<[String; 2]>> = OnceLock::new();
    let texts = TEXTS.get_or_init(|| {
        (0..KEYS)
            .map(|index| {
                let digits: String = (0..96_u8)
                    .map(|byte| {
                        let value = if byte == 0 {
                            0x80 | index
                        } else {
                            byte.wrapping_mul(29) ^ index.wrapping_mul(53)
                        };
                        format!("{value:02x}")
                    })
                    .collect();
                [
                    format!("0x{digits}"),
                    format!("0x{}", digits.to_uppercase()),
                ]
            })
            .collect()
    });
    let [lower, upper_text] = texts
        .get(usize::from(index))
        .ok_or_else(|| format!("no key {index}"))?;
    Ok(if upper { upper_text } else { lower })
}

/// Key `index` as memory holds it.
fn key(index: u8) -> Res<BlsKey> {
    Ok(key_text(index, false)?.parse()?)
}

/// A key's name in a sequence's description, `k<i>`, read back from its first byte.
fn label(key: &BlsKey) -> String {
    key.as_bytes()
        .first()
        .map_or_else(|| "k?".to_owned(), |byte| format!("k{}", byte & 0x7f))
}

/// One swarm of a snapshot as the node serialises it.
fn swarm_json(name: &str, health: Health, epoch: u64, entries: &[EntrySpec]) -> Res<Value> {
    let worker_id = name
        .strip_prefix("worker-")
        .map(str::parse::<u16>)
        .transpose()?;
    let window = match health {
        Health::NoWindow => None,
        Health::Ahead => Some(epoch.saturating_sub(1)),
        Health::Behind => Some(epoch.checked_add(1).ok_or("epoch overflow")?),
        _ => Some(epoch),
    };
    let fallback = match health {
        Health::Unresolved => Some("Unresolved"),
        Health::Fallback(fallback) => Some(fallback),
        _ => None,
    };
    let admission = (health != Health::NoAdmission).then(|| {
        json!({
            "configured": "Closed",
            "effective": "Closed",
            "epoch": window,
            "fallback": fallback,
            "resolved_current": 0,
            "required_current": 0,
            "connected_current": 0,
            "contradicted": 0,
        })
    });
    let entries = entries
        .iter()
        .map(EntrySpec::json)
        .collect::<Res<Vec<_>>>()?;
    Ok(json!({
        "network": name,
        "worker_id": worker_id,
        "available": health != Health::Unavailable,
        "tracked_epoch": (health != Health::Unpublished).then_some(epoch),
        "admission": admission,
        "entries": entries,
    }))
}

/// A whole `tn_admissionSnapshot` result.
fn snapshot_json(epoch: u64, swarms: Vec<Value>) -> Value {
    json!({"version": 1, "epoch": epoch, "swarms": swarms})
}

/// A hand-written snapshot of `epoch`, through [`snapshot::parse`].
fn snapshot_of(epoch: u64, swarms: &[(&str, Health, Vec<EntrySpec>)]) -> Res<Snapshot> {
    let swarms = swarms
        .iter()
        .map(|(name, health, entries)| swarm_json(name, *health, epoch, entries))
        .collect::<Res<Vec<_>>>()?;
    Ok(snapshot::parse(snapshot_json(epoch, swarms))?)
}

/// A listing of key `key` with the addresses `addrs` of [`ADDRS`].
fn listed(key: u8, addrs: &[usize]) -> EntrySpec {
    EntrySpec {
        key,
        addrs: addrs.to_vec(),
        ..EntrySpec::default()
    }
}

/// An `unresolved` listing of key `key`, without addresses.
fn unresolved(key: u8) -> EntrySpec {
    EntrySpec {
        key,
        unresolved: true,
        ..EntrySpec::default()
    }
}

/// 0 to 5 indices into [`ADDRS`], most of them plain.
fn draw_addrs(rng: &mut Rng) -> Res<Vec<usize>> {
    let count = rng.below(6);
    let mut addrs = Vec::new();
    for _ in 0..count {
        let range = if rng.chance(3, 4) {
            PLAIN
        } else {
            u64::try_from(ADDRS.len())?
        };
        addrs.push(usize::try_from(rng.below(range))?);
    }
    Ok(addrs)
}

/// Where a key stands in one swarm of the generated committee; it drifts a little every run.
#[derive(Debug, Clone)]
struct Member {
    /// Whether the swarm lists the key.
    listed: bool,
    /// The addresses it lists the key with.
    addrs: Vec<usize>,
}

/// The generator of one seed's sequence.
struct Gen {
    /// The seed, for messages.
    seed: u64,
    /// The draws.
    rng: Rng,
    /// The node's epoch: where the next run starts from, never lowered by a regressed run.
    epoch: u64,
    /// The committee, by swarm in the order of [`SWARMS`], then by key.
    roster: Vec<Vec<Member>>,
}

/// One generated snapshot: its description and what parse made of it.
struct Run {
    /// The snapshot in the notation of [`Gen::run`].
    desc: String,
    /// The validated snapshot, or why the run is stale.
    parsed: Result<Snapshot, SnapshotError>,
}

/// One seed's sequence.
struct Sequence {
    /// The seed that generated it.
    seed: u64,
    /// Its [`RUNS`] snapshots, in order.
    runs: Vec<Run>,
}

impl Gen {
    /// The generator for `seed`, with a random committee and a starting epoch of 0 to 3.
    fn new(seed: u64) -> Res<Self> {
        let mut rng = Rng::new(seed);
        let epoch = rng.below(4);
        let mut roster = Vec::new();
        for _ in SWARMS {
            let mut members = Vec::new();
            for _ in 0..KEYS {
                members.push(Member {
                    listed: rng.chance(1, 2),
                    addrs: draw_addrs(&mut rng)?,
                });
            }
            roster.push(members);
        }
        Ok(Self {
            seed,
            rng,
            epoch,
            roster,
        })
    }

    /// The next run's epoch: the same in half the runs, one more in 30%, 2 to 5 more in 12%, and 1 to 3 below the
    /// node's epoch in 8% (clamped at 0), which does not move the node's epoch.
    fn next_epoch(&mut self) -> Res<u64> {
        let step = match self.rng.below(100) {
            0..50 => 0,
            50..80 => 1,
            80..92 => self.rng.below(4).checked_add(2).ok_or("step overflow")?,
            _ => {
                let back = self.rng.below(3).saturating_add(1);
                return Ok(self.epoch.saturating_sub(back));
            }
        };
        self.epoch = self.epoch.checked_add(step).ok_or("epoch overflow")?;
        Ok(self.epoch)
    }

    /// The next snapshot, described as `e<epoch>` and then, per swarm, `<name>/<health>[<entries>]` with the letters
    /// of [`Health::code`] and the entries of [`EntrySpec::describe`]; a stale run ends in `=stale:<reason>`.
    fn run(&mut self) -> Res<Run> {
        let epoch = self.next_epoch()?;
        let mut desc = format!("e{epoch}");
        let mut swarms = Vec::new();
        // what parse must call each swarm, or the properties would test something other than they say
        let mut sound = BTreeMap::new();
        for (index, name) in SWARMS.into_iter().enumerate() {
            let members = self.roster.get_mut(index).ok_or("no roster")?;
            for member in members.iter_mut() {
                if self.rng.chance(1, 6) {
                    member.listed = !member.listed;
                }
                if self.rng.chance(1, 5) {
                    member.addrs = draw_addrs(&mut self.rng)?;
                }
            }
            let primary = index == 0;
            if !primary && self.rng.chance(1, 2) {
                continue;
            }
            let authoritative = if primary {
                !self.rng.chance(1, 12)
            } else {
                self.rng.chance(3, 5)
            };
            let healths = if authoritative {
                AUTHORITATIVE.as_slice()
            } else {
                DOUBTFUL.as_slice()
            };
            let health = *self.rng.pick(healths).ok_or("no health")?;
            let mut entries = Vec::new();
            for (key, member) in (0..KEYS).zip(members.iter()) {
                if !member.listed {
                    continue;
                }
                let unresolved = self.rng.chance(1, 6);
                let record = if !unresolved && self.rng.chance(1, 6) {
                    Some(draw_addrs(&mut self.rng)?)
                } else {
                    None
                };
                entries.push(EntrySpec {
                    key,
                    addrs: if unresolved {
                        Vec::new()
                    } else {
                        member.addrs.clone()
                    },
                    record,
                    unresolved,
                    upper: self.rng.chance(1, 10),
                });
                // a key listed twice in one swarm, which parse merges
                if self.rng.chance(1, 10) {
                    entries.push(EntrySpec {
                        key,
                        addrs: draw_addrs(&mut self.rng)?,
                        ..EntrySpec::default()
                    });
                }
            }
            if health == Health::Unavailable && self.rng.chance(1, 2) {
                entries.clear();
            }
            write!(desc, " {name}/{}[", health.code())?;
            for (n, entry) in entries.iter().enumerate() {
                if n > 0 {
                    desc.push(' ');
                }
                entry.describe(&mut desc)?;
            }
            desc.push(']');
            swarms.push(swarm_json(name, health, epoch, &entries)?);
            sound.insert(name, authoritative);
        }
        let parsed = snapshot::parse(snapshot_json(epoch, swarms));
        let primary_sound = sound.get("primary").copied().unwrap_or(false);
        match &parsed {
            Ok(snap) => {
                let agrees = primary_sound
                    && snap.epoch == epoch
                    && snap.swarms.len() == sound.len()
                    && sound.iter().all(|(name, authoritative)| {
                        snap.swarms.get(*name).map(|swarm| swarm.authoritative)
                            == Some(*authoritative)
                    });
                if !agrees {
                    return Err(format!(
                        "seed {}: {desc}: parse disagrees with the generator: {snap:?}",
                        self.seed
                    )
                    .into());
                }
            }
            Err(error) => {
                if primary_sound {
                    return Err(format!(
                        "seed {}: {desc}: an authoritative primary refused as {}",
                        self.seed,
                        error.token()
                    )
                    .into());
                }
                write!(desc, "=stale:{}", error.token())?;
            }
        }
        Ok(Run { desc, parsed })
    }
}

/// The sequence of `seed`.
fn generate(seed: u64) -> Res<Sequence> {
    let mut generator = Gen::new(seed)?;
    let runs = (0..RUNS)
        .map(|_| generator.run())
        .collect::<Res<Vec<_>>>()?;
    Ok(Sequence { seed, runs })
}

/// The sequences of every seed, generated once for all the properties, a share of the seeds on each core.
fn sequences() -> Res<&'static [Sequence]> {
    static SEQUENCES: OnceLock<Result<Vec<Sequence>, String>> = OnceLock::new();
    let generated = SEQUENCES.get_or_init(|| {
        let seeds: Vec<u64> = (1..=SEEDS).collect();
        let shares = per_core(&seeds, |share| {
            share
                .iter()
                .map(|seed| generate(*seed).map_err(|error| error.to_string()))
                .collect::<Result<Vec<_>, String>>()
        })?;
        Ok(shares.into_iter().flatten().collect())
    });
    Ok(generated.as_deref().map_err(String::as_str)?)
}

/// Runs `work` on a share of `items` per core and returns each share's result, in order.
///
/// A panic in a share, such as a failed assertion, is resumed here, so the test fails with its message.
fn per_core<T: Sync, R: Send>(
    items: &[T],
    work: impl Fn(&[T]) -> Result<R, String> + Sync,
) -> Result<Vec<R>, String> {
    let cores = thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let share = items.len().div_ceil(cores).max(1);
    let work = &work;
    thread::scope(|scope| {
        let workers: Vec<_> = items
            .chunks(share)
            .map(|chunk| scope.spawn(move || work(chunk)))
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap_or_else(|panic| resume_unwind(panic)))
            .collect()
    })
}

/// Runs `property` on the sequence of every seed, a share of the seeds per core, and adds up what it tallied.
fn check_all(property: impl Fn(&Sequence, &mut Tally) -> TestResult + Sync) -> Res<Tally> {
    let shares = per_core(sequences()?, |share| {
        let mut tally = Tally::default();
        for sequence in share {
            property(sequence, &mut tally).map_err(|error| error.to_string())?;
        }
        Ok(tally)
    })?;
    let mut total = Tally::default();
    for tally in shares {
        total.add(tally);
    }
    Ok(total)
}

/// One fold of a generated sequence: the state before it, the snapshot, what [`apply`] returned and the state after.
struct Fold<'a> {
    /// The seed of the sequence.
    seed: u64,
    /// The sequence up to and including this snapshot.
    trail: &'a str,
    /// The snapshot folded in.
    snap: &'a Snapshot,
    /// The state before the fold.
    before: &'a State,
    /// The state after the fold.
    after: &'a State,
    /// What [`apply`] returned.
    result: Result<ApplyOutcome, MemoryError>,
}

impl Fold<'_> {
    /// An assertion message: the seed, what went wrong, and the sequence that got there.
    fn fail(&self, what: impl Display) -> String {
        format!("seed {}: {what}\n  sequence: {}", self.seed, self.trail)
    }

    /// The fold's epoch, E.
    const fn epoch(&self) -> u64 {
        self.snap.epoch
    }

    /// `E - seen`, the age of an address at this fold.
    fn age(&self, seen: u64) -> Res<u64> {
        self.epoch().checked_sub(seen).ok_or_else(|| {
            self.fail(format_args!("seen {seen} above the epoch"))
                .into()
        })
    }

    /// Whether the fold applied the snapshot, rather than refusing it as regressed.
    const fn applied(&self) -> bool {
        self.result.is_ok()
    }
}

/// Folds `sequence` into an empty state and hands every fold of a valid snapshot to `check`.
fn fold_each(sequence: &Sequence, mut check: impl FnMut(&Fold<'_>) -> TestResult) -> TestResult {
    let mut state = State::default();
    let mut trail = String::new();
    for run in &sequence.runs {
        if !trail.is_empty() {
            trail.push_str(" | ");
        }
        trail.push_str(&run.desc);
        // a stale run never reaches memory
        let Ok(snap) = &run.parsed else {
            continue;
        };
        let before = state.clone();
        let result = apply(&mut state, snap);
        check(&Fold {
            seed: sequence.seed,
            trail: &trail,
            snap,
            before: &before,
            after: &state,
            result,
        })?;
    }
    Ok(())
}

/// What memory holds for `key` in `swarm`.
fn memory<'a>(state: &'a State, key: &BlsKey, swarm: &str) -> Option<&'a SwarmMemory> {
    state.keys.get(key)?.get(swarm)
}

/// Every `(key, swarm)` memory of `state`.
fn memories(state: &State) -> impl Iterator<Item = (&BlsKey, &String, &SwarmMemory)> {
    state
        .keys
        .iter()
        .flat_map(|(key, swarms)| swarms.iter().map(move |(name, memory)| (key, name, memory)))
}

/// The snapshot's listing of `key` in `swarm`.
fn listing<'a>(snap: &'a Snapshot, key: &BlsKey, swarm: &str) -> Option<&'a Entry> {
    snap.swarms.get(swarm)?.entries.get(key)
}

/// Whether the snapshot holds `swarm` as authoritative, the only case in which its omissions strike.
fn authoritative(snap: &Snapshot, swarm: &str) -> bool {
    snap.swarms
        .get(swarm)
        .is_some_and(|held| held.authoritative)
}

/// Whether the snapshot strikes `key` in `swarm`: the swarm is authoritative and does not list it.
fn strikes(snap: &Snapshot, key: &BlsKey, swarm: &str) -> bool {
    authoritative(snap, swarm) && listing(snap, key, swarm).is_none()
}

/// How often each named case came up over every seed, so that a property cannot pass without being exercised.
#[derive(Debug, Default)]
struct Tally(BTreeMap<&'static str, u64>);

impl Tally {
    /// Counts one occurrence of `case`.
    fn bump(&mut self, case: &'static str) {
        let count = self.0.entry(case).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// Adds the counts of `other`.
    fn add(&mut self, other: Self) {
        for (case, more) in other.0 {
            let count = self.0.entry(case).or_insert(0);
            *count = count.saturating_add(more);
        }
    }

    /// Asserts that every one of `cases` came up.
    fn require(&self, cases: &[&str]) {
        for case in cases {
            let count = self.0.get(case).copied().unwrap_or(0);
            assert!(count > 0, "no seed exercised `{case}`: {:?}", self.0);
        }
    }
}

/// `state.json` text with every `absent` count replaced by `_`.
fn mask_absent(bytes: &[u8]) -> Res<String> {
    let mut rest = std::str::from_utf8(bytes)?;
    let mut masked = String::with_capacity(rest.len());
    while let Some((head, tail)) = rest.split_once("\"absent\":") {
        masked.push_str(head);
        masked.push_str("\"absent\":_");
        rest = tail.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    masked.push_str(rest);
    Ok(masked)
}

/// `state.json` round trips: after every fold, `load_state(serialize(state)) == state`, serialising the loaded state
/// gives the same bytes, and the bytes are one line ending in a newline, with no empty map (`{}`) other than the
/// top-level `keys` of a state that remembers nothing.
#[test]
fn state_json_round_trips() -> TestResult {
    assert_eq!(load_state(None)?, State::default());
    let empty = serialize(&State::default());
    assert_eq!(
        String::from_utf8_lossy(&empty),
        "{\"version\":1,\"epoch\":0,\"keys\":{}}\n"
    );
    assert_eq!(load_state(Some(&empty))?, State::default());
    let tally = check_all(|sequence, tally| {
        fold_each(sequence, |fold| {
            if !fold.applied() {
                return Ok(());
            }
            let bytes = serialize(fold.after);
            let text = String::from_utf8_lossy(&bytes);
            let loaded = load_state(Some(&bytes))
                .map_err(|error| fold.fail(format_args!("load_state refused {text}: {error}")))?;
            assert_eq!(
                &loaded,
                fold.after,
                "{}",
                fold.fail(format_args!("{text} loads as another state"))
            );
            assert_eq!(
                serialize(&loaded),
                bytes,
                "{}",
                fold.fail(format_args!("{text} serialises differently once loaded"))
            );
            assert!(
                text.ends_with('\n') && text.matches('\n').count() == 1,
                "{}",
                fold.fail(format_args!("not one line and a newline: {text:?}"))
            );
            // every key and swarm is a JSON string free of braces, so an empty map shows as `{}`
            let allowed = usize::from(fold.after.keys.is_empty());
            assert_eq!(
                text.matches("{}").count(),
                allowed,
                "{}",
                fold.fail(format_args!("an empty map in {text}"))
            );
            tally.bump(if fold.after.keys.is_empty() {
                "a state that remembers nothing"
            } else {
                "a state that remembers"
            });
            Ok(())
        })
    })?;
    tally.require(&["a state that remembers"]);
    Ok(())
}

/// Rule 2, "a remembered address stays while `E - seen` is below `TTL_EPOCHS`" (3), pinned at both sides of the edge:
/// at a fold of epoch E, an address that the snapshot lists again under its key and swarm gets `seen = E`; one it
/// does not keeps its `seen` while `E - seen < 3` and is gone once `E - seen >= 3`, unless a last strike removed the
/// whole memory first. So an address last listed at `seen` is still held after the fold at `seen + 2` and gone after
/// the fold at `seen + 3`, or after the first fold past it when the epoch jumps. Nothing in memory is older, and
/// nothing appears that was not listed.
#[test]
fn address_unlisted_for_ttl_epochs_is_gone() -> TestResult {
    let tally = check_all(|sequence, tally| {
        fold_each(sequence, |fold| {
            if !fold.applied() {
                return Ok(());
            }
            let epoch = fold.epoch();
            for (key, swarm, before) in memories(fold.before) {
                let entry = listing(fold.snap, key, swarm);
                let after = memory(fold.after, key, swarm);
                let struck =
                    strikes(fold.snap, key, swarm) && before.absent.checked_add(1) >= Some(STRIKES);
                for (ip, seen) in &before.ips {
                    let age = fold.age(*seen)?;
                    let now = after.and_then(|memory| memory.ips.get(ip));
                    let at = || format!("{} {swarm} {ip} last seen {seen}", label(key));
                    if entry.is_some_and(|entry| entry.ips.contains(ip)) {
                        assert_eq!(
                            now,
                            Some(&epoch),
                            "{}",
                            fold.fail(format_args!("{}: listed at {epoch}, not refreshed", at()))
                        );
                    } else if struck {
                        assert_eq!(
                            now,
                            None,
                            "{}",
                            fold.fail(format_args!("{}: kept past its last strike", at()))
                        );
                    } else if age < TTL_EPOCHS {
                        assert_eq!(
                            now,
                            Some(seen),
                            "{}",
                            fold.fail(format_args!("{}: lost at epoch {epoch}, age {age}", at()))
                        );
                        if age.checked_add(1) == Some(TTL_EPOCHS) {
                            tally.bump("held after the fold at seen+2");
                        }
                    } else {
                        assert_eq!(
                            now,
                            None,
                            "{}",
                            fold.fail(format_args!("{}: held at epoch {epoch}, age {age}", at()))
                        );
                        tally.bump(if age == TTL_EPOCHS {
                            "gone after the fold at seen+3"
                        } else {
                            "gone after a jump past seen+3"
                        });
                    }
                }
            }
            for (key, swarm, after) in memories(fold.after) {
                for (ip, seen) in &after.ips {
                    let age = fold.age(*seen)?;
                    assert!(
                        age < TTL_EPOCHS,
                        "{}",
                        fold.fail(format_args!(
                            "{} {swarm} {ip} held at age {age}",
                            label(key)
                        ))
                    );
                    let refreshed = *seen == epoch
                        && listing(fold.snap, key, swarm)
                            .is_some_and(|entry| entry.ips.contains(ip));
                    let carried = memory(fold.before, key, swarm)
                        .and_then(|memory| memory.ips.get(ip))
                        == Some(seen);
                    assert!(
                        refreshed || carried,
                        "{}",
                        fold.fail(format_args!(
                            "{} {swarm} {ip} seen {seen} was neither listed nor remembered",
                            label(key)
                        ))
                    );
                }
            }
            Ok(())
        })
    })?;
    tally.require(&[
        "held after the fold at seen+2",
        "gone after the fold at seen+3",
        "gone after a jump past seen+3",
    ]);
    Ok(())
}

/// Rule 3, "a key that an authoritative swarm no longer lists gets an absent strike for that swarm, and once it has
/// `STRIKES` strikes its memory for that swarm is removed": the test keeps its own count, per key and swarm, of the
/// authoritative runs of that swarm that omitted the key since its last listing there. A listing in that swarm,
/// authoritative or not, resets it to 0; a run without the swarm, a non-authoritative run of it and a stale run leave
/// it alone. A remembered key's `absent` is always that count, and when the count reaches 2 the memory is gone.
#[test]
fn two_consecutive_authoritative_omissions_remove_a_key() -> TestResult {
    let tally = check_all(|sequence, tally| {
        let mut counts: BTreeMap<(BlsKey, String), u8> = BTreeMap::new();
        fold_each(sequence, |fold| {
            if !fold.applied() {
                return Ok(());
            }
            let mut next = BTreeMap::new();
            for (key, swarm, _) in memories(fold.before) {
                let at = || format!("{} {swarm}", label(key));
                let count = counts.get(&(*key, swarm.clone())).copied().ok_or_else(|| {
                    fold.fail(format_args!("{}: remembered, never counted", at()))
                })?;
                let expected = if listing(fold.snap, key, swarm).is_some() {
                    if count > 0 {
                        tally.bump("a strike reset by a listing");
                    }
                    0
                } else if authoritative(fold.snap, swarm) {
                    count.checked_add(1).ok_or("count overflow")?
                } else {
                    count
                };
                let after = memory(fold.after, key, swarm);
                if expected >= STRIKES {
                    assert!(
                        after.is_none(),
                        "{}",
                        fold.fail(format_args!(
                            "{}: omitted by {expected} authoritative runs in a row, still remembered",
                            at()
                        ))
                    );
                    tally.bump("removed on the second strike");
                } else if let Some(after) = after {
                    assert_eq!(
                        after.absent,
                        expected,
                        "{}",
                        fold.fail(format_args!("{}: absent count", at()))
                    );
                    if expected > count {
                        tally.bump("a first strike, kept");
                    }
                    next.insert((*key, swarm.clone()), expected);
                }
            }
            for (key, swarm, after) in memories(fold.after) {
                if memory(fold.before, key, swarm).is_none() {
                    assert!(
                        listing(fold.snap, key, swarm).is_some(),
                        "{}",
                        fold.fail(format_args!("{} {swarm}: remembered unlisted", label(key)))
                    );
                    assert_eq!(
                        after.absent,
                        0,
                        "{}",
                        fold.fail(format_args!("{} {swarm}: new with a strike", label(key)))
                    );
                    next.insert((*key, swarm.clone()), 0);
                }
            }
            counts = next;
            Ok(())
        })
    })?;
    tally.require(&[
        "removed on the second strike",
        "a strike reset by a listing",
        "a first strike, kept",
    ]);
    Ok(())
}

/// Rule 4, "a swarm the snapshot does not hold, or holds as non-authoritative, is additions-only": for every memory
/// under such a swarm, the fold keeps exactly the addresses with `E - seen < 3` plus those listed now (at `seen = E`),
/// leaves `absent` as it was (0 when listed), and removes the memory only when the TTL has emptied it.
#[test]
fn additions_only_swarm_never_strikes_or_removes() -> TestResult {
    let tally = check_all(|sequence, tally| {
        fold_each(sequence, |fold| {
            if !fold.applied() {
                return Ok(());
            }
            let epoch = fold.epoch();
            for (key, swarm, before) in memories(fold.before) {
                let held = fold.snap.swarms.get(swarm.as_str());
                if held.is_some_and(|held| held.authoritative) {
                    continue;
                }
                let entry = listing(fold.snap, key, swarm);
                let mut expected: BTreeMap<IpAddr, u64> = before
                    .ips
                    .iter()
                    .filter(|&(_, &seen)| epoch.saturating_sub(seen) < TTL_EPOCHS)
                    .map(|(ip, seen)| (*ip, *seen))
                    .collect();
                if let Some(entry) = entry {
                    expected.extend(entry.ips.iter().map(|ip| (*ip, epoch)));
                }
                let at = || format!("{} {swarm}", label(key));
                let Some(after) = memory(fold.after, key, swarm) else {
                    assert!(
                        expected.is_empty(),
                        "{}",
                        fold.fail(format_args!("{}: removed by an additions-only swarm", at()))
                    );
                    tally.bump("emptied by the TTL alone");
                    continue;
                };
                assert_eq!(
                    after.ips,
                    expected,
                    "{}",
                    fold.fail(format_args!("{}: addresses", at()))
                );
                let absent = if entry.is_some() { 0 } else { before.absent };
                assert_eq!(
                    after.absent,
                    absent,
                    "{}",
                    fold.fail(format_args!("{}: absent count", at()))
                );
                if entry.is_none() {
                    tally.bump(if held.is_none() {
                        "omitted by an absent swarm, kept"
                    } else {
                        "omitted by a non-authoritative swarm, kept"
                    });
                    if before.absent > 0 {
                        tally.bump("a pending strike left alone");
                    }
                }
            }
            Ok(())
        })
    })?;
    tally.require(&[
        "emptied by the TTL alone",
        "omitted by an absent swarm, kept",
        "omitted by a non-authoritative swarm, kept",
        "a pending strike left alone",
    ]);
    Ok(())
}

/// `state.epoch` is monotone: a snapshot below it is `EpochRegressed` and the state stays byte-identical; any other
/// snapshot, equal epochs included, is applied and the state's epoch becomes its epoch.
#[test]
fn epoch_is_monotone_and_a_regressed_snapshot_changes_nothing() -> TestResult {
    let tally = check_all(|sequence, tally| {
        fold_each(sequence, |fold| {
            let (epoch, applied) = (fold.epoch(), fold.before.epoch);
            match fold.result {
                Ok(_) => {
                    assert!(
                        epoch >= applied,
                        "{}",
                        fold.fail(format_args!("epoch {epoch} applied over {applied}"))
                    );
                    assert_eq!(
                        fold.after.epoch,
                        epoch,
                        "{}",
                        fold.fail("the state's epoch is not the snapshot's")
                    );
                    tally.bump(if epoch == applied {
                        "the same epoch"
                    } else {
                        "a later epoch"
                    });
                }
                Err(MemoryError::EpochRegressed) => {
                    assert!(
                        epoch < applied,
                        "{}",
                        fold.fail(format_args!("epoch {epoch} refused over {applied}"))
                    );
                    assert_eq!(
                        serialize(fold.after),
                        serialize(fold.before),
                        "{}",
                        fold.fail("a regressed snapshot changed state.json")
                    );
                    assert_eq!(
                        fold.after,
                        fold.before,
                        "{}",
                        fold.fail("a regressed snapshot changed the state")
                    );
                    tally.bump("a regressed epoch");
                }
                Err(error) => {
                    return Err(fold.fail(format_args!("apply failed: {error}")).into());
                }
            }
            Ok(())
        })
    })?;
    tally.require(&["the same epoch", "a later epoch", "a regressed epoch"]);
    Ok(())
}

/// Running the same authoritative snapshot again, as two 5-minute runs in one epoch do: the second run removes
/// exactly the memories its first run struck (each a key that an authoritative swarm of the snapshot omits, left at
/// `absent = STRIKES - 1` by the first run) and is otherwise identical to the first; a third run changes nothing.
///
/// So "only `absent` can differ" holds only when the snapshot struck nothing, and then the bytes are equal outright.
/// When it struck a key, masking `absent` does not hide the difference, because the second strike removes the memory
/// rather than counting to 2 (rule 3: "a key missing from two consecutive 5-minute runs at the same epoch is removed").
#[test]
fn reapplying_a_snapshot_only_completes_its_strikes() -> TestResult {
    let tally = check_all(|sequence, tally| {
        fold_each(sequence, |fold| {
            if !fold.applied() {
                return Ok(());
            }
            let first = fold.after;
            let mut second = first.clone();
            apply(&mut second, fold.snap)
                .map_err(|error| fold.fail(format_args!("second run failed: {error}")))?;
            let mut struck = Vec::new();
            for (key, swarm, memory) in memories(first) {
                if strikes(fold.snap, key, swarm) {
                    assert_eq!(
                        memory.absent.checked_add(1),
                        Some(STRIKES),
                        "{}",
                        fold.fail(format_args!(
                            "{} {swarm}: struck once by the first run",
                            label(key)
                        ))
                    );
                    struck.push(format!("{} {swarm}", label(key)));
                }
            }
            let mut expected = first.clone();
            for (key, swarms) in &mut expected.keys {
                swarms.retain(|swarm, _| !strikes(fold.snap, key, swarm));
            }
            expected.keys.retain(|_, swarms| !swarms.is_empty());
            assert_eq!(
                &second,
                &expected,
                "{}",
                fold.fail(format_args!(
                    "the second run did more than remove {struck:?}"
                ))
            );
            // with nothing struck, `expected` is `first`, so the two states, and with them their bytes, are equal
            if struck.is_empty() {
                tally.bump("a second run that changes nothing");
            } else {
                assert_ne!(
                    mask_absent(&serialize(&second))?,
                    mask_absent(&serialize(first))?,
                    "{}",
                    fold.fail(format_args!("{struck:?} survived a second strike"))
                );
                tally.bump("a second run that completes strikes");
            }
            let mut third = second.clone();
            apply(&mut third, fold.snap)
                .map_err(|error| fold.fail(format_args!("third run failed: {error}")))?;
            assert_eq!(
                third,
                second,
                "{}",
                fold.fail("the third run of a snapshot changed the state")
            );
            Ok(())
        })
    })?;
    tally.require(&[
        "a second run that changes nothing",
        "a second run that completes strikes",
    ]);
    Ok(())
}

/// `allow_ips` is the union of every address memory holds, under every key and swarm. Each is its own
/// `canonical_ip` and one of [`CANONICAL`]; every address the snapshot lists is in it, and every one in it was listed
/// by some snapshot of the sequence.
#[test]
fn allow_ips_is_every_remembered_address() -> TestResult {
    let pool = CANONICAL
        .iter()
        .map(|text| text.parse::<IpAddr>())
        .collect::<Result<BTreeSet<_>, _>>()?;
    let tally = check_all(|sequence, tally| {
        let mut ever = BTreeSet::new();
        fold_each(sequence, |fold| {
            if !fold.applied() {
                return Ok(());
            }
            let listed: BTreeSet<IpAddr> = fold
                .snap
                .swarms
                .values()
                .flat_map(|swarm| swarm.entries.values())
                .flat_map(|entry| entry.ips.iter().copied())
                .collect();
            ever.extend(listed.iter().copied());
            let ips = allow_ips(fold.after);
            let union: BTreeSet<IpAddr> = memories(fold.after)
                .flat_map(|(_, _, memory)| memory.ips.keys().copied())
                .collect();
            assert_eq!(ips, union, "{}", fold.fail("allow_ips is not the union"));
            for ip in &ips {
                assert_eq!(
                    canonical_ip(*ip),
                    Some(*ip),
                    "{}",
                    fold.fail(format_args!("{ip} is not canonical"))
                );
                assert!(
                    pool.contains(ip),
                    "{}",
                    fold.fail(format_args!("{ip} is no address of the pool"))
                );
            }
            assert!(
                listed.is_subset(&ips),
                "{}",
                fold.fail(format_args!("listed {listed:?}, allowed {ips:?}"))
            );
            assert!(
                ips.is_subset(&ever),
                "{}",
                fold.fail(format_args!("allowed {ips:?}, ever listed {ever:?}"))
            );
            tally.bump(if ips.len() > listed.len() {
                "an allow set wider than the snapshot"
            } else {
                "an allow set of the snapshot's own"
            });
            Ok(())
        })
    })?;
    tally.require(&[
        "an allow set wider than the snapshot",
        "an allow set of the snapshot's own",
    ]);
    Ok(())
}

/// The TTL edge, by hand: an address listed at 10 and then only listed `unresolved` is held after the folds at 11 and
/// 12 (twice at 12: runs within an epoch do not age it) and gone after the fold at 13, `seen + TTL_EPOCHS`, with its
/// emptied memory pruned. A jump from 10 straight to 12 keeps it; one to 13 or 15 drops it.
#[test]
fn ttl_boundary_is_three_epochs_after_seen() -> TestResult {
    let (k0, ip) = (key(0)?, "192.0.2.10".parse::<IpAddr>()?);
    let first = || snapshot_of(10, &[("primary", Health::Sound, vec![listed(0, &[0])])]);
    let unlisted = |epoch| snapshot_of(epoch, &[("primary", Health::Sound, vec![unresolved(0)])]);
    let mut state = State::default();
    apply(&mut state, &first()?)?;
    for (epoch, held) in [(11, true), (12, true), (12, true), (13, false)] {
        apply(&mut state, &unlisted(epoch)?)?;
        let seen = memory(&state, &k0, "primary").and_then(|memory| memory.ips.get(&ip));
        assert_eq!(seen.is_some(), held, "epoch {epoch}: seen {seen:?}");
        if let Some(seen) = seen {
            assert_eq!(*seen, 10, "epoch {epoch}: seen moved without a listing");
        }
    }
    assert!(
        state.keys.is_empty(),
        "an emptied memory is pruned: {state:?}"
    );
    for (epoch, held) in [(12, true), (13, false), (15, false)] {
        let mut state = State::default();
        apply(&mut state, &first()?)?;
        apply(&mut state, &unlisted(epoch)?)?;
        assert_eq!(
            allow_ips(&state).contains(&ip),
            held,
            "jump from 10 to {epoch}"
        );
    }
    Ok(())
}

/// Strikes count per swarm, by hand, all at epoch 5: a listing between two omissions resets the count, so the next
/// omission is a first strike again; a listing in another swarm resets nothing in this one; and two omissions in a
/// row remove the key, a stale run between them notwithstanding.
#[test]
fn a_listing_between_two_omissions_resets_the_strike() -> TestResult {
    let k1 = key(1)?;
    let both = || {
        snapshot_of(
            5,
            &[(
                "primary",
                Health::Sound,
                vec![listed(0, &[0]), listed(1, &[1])],
            )],
        )
    };
    let without = || snapshot_of(5, &[("primary", Health::Sound, vec![listed(0, &[0])])]);
    let absent = |state: &State| memory(state, &k1, "primary").map(|memory| memory.absent);
    let mut state = State::default();
    for (step, snap, expected) in [
        ("listed", both()?, Some(0)),
        ("omitted", without()?, Some(1)),
        ("listed again", both()?, Some(0)),
        ("omitted after the reset", without()?, Some(1)),
        ("omitted twice in a row", without()?, None),
    ] {
        apply(&mut state, &snap)?;
        assert_eq!(absent(&state), expected, "{step}");
    }

    // a listing by worker-0 resets the key's count in worker-0, not in the primary
    let elsewhere = snapshot_of(
        5,
        &[
            ("primary", Health::Sound, vec![listed(0, &[0])]),
            ("worker-0", Health::Unavailable, vec![listed(1, &[1])]),
        ],
    )?;
    let mut state = State::default();
    apply(&mut state, &both()?)?;
    apply(&mut state, &without()?)?;
    apply(&mut state, &elsewhere)?;
    assert_eq!(
        absent(&state),
        None,
        "a worker's listing reset the primary's strike"
    );
    assert_eq!(
        memory(&state, &k1, "worker-0").map(|memory| memory.absent),
        Some(0)
    );

    // a stale run is refused before memory, so it neither strikes nor resets
    let stale = snapshot_of(5, &[("primary", Health::Behind, vec![listed(1, &[1])])]);
    assert!(
        stale.is_err(),
        "a primary behind its window is stale: {stale:?}"
    );
    let mut state = State::default();
    apply(&mut state, &both()?)?;
    apply(&mut state, &without()?)?;
    apply(&mut state, &without()?)?;
    assert_eq!(absent(&state), None, "two omissions around a stale run");
    Ok(())
}

/// The counterexample to "a second run of the same authoritative snapshot changes only `absent`", by hand: after a
/// run that lists k0 and k1, a snapshot that omits k1 strikes it once; running that snapshot again removes k1's
/// memory, which masking `absent` does not hide; a third run changes nothing.
#[test]
fn the_second_run_of_a_snapshot_removes_what_the_first_struck() -> TestResult {
    let k1 = key(1)?;
    let both = snapshot_of(
        7,
        &[(
            "primary",
            Health::Sound,
            vec![listed(0, &[0]), listed(1, &[2])],
        )],
    )?;
    let without = snapshot_of(7, &[("primary", Health::Sound, vec![listed(0, &[0])])])?;
    let mut state = State::default();
    apply(&mut state, &both)?;
    apply(&mut state, &without)?;
    assert_eq!(
        memory(&state, &k1, "primary").map(|memory| memory.absent),
        Some(1)
    );
    let first = serialize(&state);
    apply(&mut state, &without)?;
    let second = serialize(&state);
    assert!(
        memory(&state, &k1, "primary").is_none(),
        "k1 survived its second strike"
    );
    assert_ne!(mask_absent(&second)?, mask_absent(&first)?);
    assert_eq!(
        String::from_utf8_lossy(&second),
        format!(
            "{{\"version\":1,\"epoch\":7,\"keys\":{{\"{}\":{{\"primary\":{{\"absent\":0,\"ips\":{{\"192.0.2.10\":7}}}}}}}}}}\n",
            key_text(0, false)?
        )
    );
    apply(&mut state, &without)?;
    assert_eq!(
        serialize(&state),
        second,
        "the third run changed state.json"
    );
    Ok(())
}
