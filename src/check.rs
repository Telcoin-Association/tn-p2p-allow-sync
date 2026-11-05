//! `check`: whether the live table admits every current committee address the node's snapshot lists.
//!
//! The run takes no lock and writes nothing.
//! It reads the config for the RPC port, `status` for the applied epoch and `state.json` for the addresses the last good sync held, lists the table, then fetches the snapshot the way `sync` does.
//! `state.json` is read without the lock and before the listing: the sync renames it into place only after the table holds it, so it never names an address the later listing had no chance to show.
//! The verdict, in the order it is decided:
//!
//! 1. An unsafe state dir, an invalid config or nft failing to list the table is an error, exit 2, as for `sync`.
//! 2. No table is `absent`, exit 1; the node is not asked.
//! 3. A snapshot that is not authoritative, or whose epoch is below the applied `EPOCH`, is `stale`, exit 3: the run cannot judge.
//! 4. A snapshot newer than the applied `EPOCH`, or any snapshot before the first good sync, is `behind`, exit 3: the next sync catches up, so a gap now is no fault.
//!    So is a gap made only of addresses `state.json` does not hold, such as a record republished at a new address since the last sync.
//! 5. A current committee address of the primary swarm that the allow sets lack is `missing`, exit 1, when `state.json` holds it or cannot be read.
//! 6. Otherwise `ok`, exit 0.
//!
//! Whenever a snapshot was compared, up to [`MAX_LINES`] `missing IP` lines and up to [`MAX_LINES`] `mismatch 0x<first 16 hex>` lines come first, then one `result=` line with both totals.
//! A mismatch is a key whose signed record lists other IPs than the addresses the node dials; both are allowed, so it never changes the verdict.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;

use serde_json::Value;

use crate::cli::Exit;
use crate::config;
use crate::fsutil::StateDir;
use crate::log::{Log, Priority};
use crate::memory;
use crate::nft::{Element, Listing, Nft};
use crate::rpc::RpcClient;
use crate::snapshot::{self, BlsKey, PRIMARY_SWARM, Slot, Snapshot};
use crate::status;
use crate::sync::{self, SNAPSHOT_METHOD};

/// The most `missing` lines, and separately the most `mismatch` lines, one run prints.
pub const MAX_LINES: usize = 5;

/// The bytes of a key a `mismatch` line shows: 16 hex digits.
const SHOWN_KEY_BYTES: usize = 8;

/// The `stale` reason of a snapshot whose epoch is below the applied one, as `sync` names it.
const EPOCH_REGRESSED: &str = "epoch-regressed";

/// The word after `result=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Every current committee address is in the allow sets: `ok`, exit 0.
    Ok,
    /// A current committee address is not in the allow sets: `missing`, exit 1.
    Missing,
    /// The table does not exist: `absent`, exit 1.
    Absent,
    /// The snapshot is newer than the applied epoch, or every missing address is one the last good sync never held: `behind`, exit 3.
    Behind,
    /// No authoritative snapshot to judge by: `stale`, exit 3.
    Stale,
}

impl Verdict {
    /// The word as the `result=` line writes it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Absent => "absent",
            Self::Behind => "behind",
            Self::Stale => "stale",
        }
    }

    /// How a run with this verdict exits.
    pub const fn exit(self) -> Exit {
        match self {
            Self::Ok => Exit::Fresh,
            Self::Missing | Self::Absent => Exit::Refused,
            Self::Behind | Self::Stale => Exit::Stale,
        }
    }
}

/// What one `check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The verdict.
    pub verdict: Verdict,
    /// Every current committee address the allow sets lack, in canonical order; empty when no snapshot was compared.
    pub missing: Vec<IpAddr>,
    /// Every key with a mismatch in any swarm, in key order; empty when no snapshot was compared.
    pub mismatch: Vec<BlsKey>,
}

impl Report {
    /// A report that compared nothing.
    pub const fn bare(verdict: Verdict) -> Self {
        Self {
            verdict,
            missing: Vec::new(),
            mismatch: Vec::new(),
        }
    }

    /// The stdout of the run: the capped `missing` and `mismatch` lines, then `result=V missing_total=N mismatch_total=N`.
    pub fn render(&self) -> String {
        let mut text = String::new();
        for ip in self.missing.iter().take(MAX_LINES) {
            let _ = writeln!(text, "missing {ip}");
        }
        for key in self.mismatch.iter().take(MAX_LINES) {
            let _ = writeln!(text, "mismatch 0x{}", shown(key));
        }
        let _ = writeln!(
            text,
            "result={} missing_total={} mismatch_total={}",
            self.verdict.as_str(),
            self.missing.len(),
            self.mismatch.len()
        );
        text
    }
}

/// The first [`SHOWN_KEY_BYTES`] bytes of `key` in lowercase hex.
fn shown(key: &BlsKey) -> String {
    key.as_bytes()
        .iter()
        .take(SHOWN_KEY_BYTES)
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Judges an authoritative `snapshot` against the live table `listing`, with `applied` the `EPOCH` of `status` and `synced` the addresses `state.json` holds, `None` when it is absent or unreadable.
///
/// A snapshot below `applied` is not authoritative under rule 1 and compares nothing; every other snapshot is compared, `behind` included, so the lines show what the next sync adds.
pub fn judge(
    snapshot: &Snapshot,
    applied: Option<u64>,
    synced: Option<&BTreeSet<IpAddr>>,
    listing: &Listing,
) -> Report {
    if applied.is_some_and(|applied| snapshot.epoch < applied) {
        return Report::bare(Verdict::Stale);
    }
    let missing = missing(snapshot, listing);
    // an address the last good sync never held is the next sync's to add; one it held and the
    // table lacks is a fault
    let drifted = synced.is_none_or(|synced| missing.iter().any(|ip| synced.contains(ip)));
    let verdict = if applied.is_none_or(|applied| snapshot.epoch > applied) {
        Verdict::Behind
    } else if missing.is_empty() {
        Verdict::Ok
    } else if drifted {
        Verdict::Missing
    } else {
        Verdict::Behind
    };
    Report {
        verdict,
        missing,
        mismatch: mismatched(snapshot),
    }
}

/// The IPs of the primary swarm's current committee entries that neither allow set holds.
///
/// A set element counts only as the very address, a full-length prefix included, which is how `sync` writes them.
fn missing(snapshot: &Snapshot, listing: &Listing) -> Vec<IpAddr> {
    let live: BTreeSet<IpAddr> = ["allow_v4", "allow_v6"]
        .iter()
        .filter_map(|name| listing.sets.get(*name))
        .flatten()
        .filter_map(|element| match sync::normalise(*element) {
            Element::Ip(ip) => Some(ip),
            Element::Cidr { .. } | Element::Range(..) => None,
        })
        .collect();
    let current: BTreeSet<IpAddr> = snapshot
        .swarms
        .get(PRIMARY_SWARM)
        .into_iter()
        .flat_map(|primary| primary.entries.values())
        .filter(|entry| entry.slots.contains(&Slot::Current))
        .flat_map(|entry| entry.ips.iter().copied())
        .collect();
    current.difference(&live).copied().collect()
}

/// The keys listed with a mismatch in any swarm, each once.
fn mismatched(snapshot: &Snapshot) -> Vec<BlsKey> {
    let keys: BTreeSet<BlsKey> = snapshot
        .swarms
        .values()
        .flat_map(|swarm| swarm.entries.iter())
        .filter(|(_, entry)| entry.mismatch)
        .map(|(key, _)| *key)
        .collect();
    keys.into_iter().collect()
}

/// Runs `check` on the state dir `state_dir` with the nft runner `nft`: the report goes to `out`, errors and the reason a run cannot judge to `log`.
pub fn check<O: Write, E: Write>(
    state_dir: &Path,
    nft: &Nft,
    out: &mut O,
    log: &mut Log<E>,
) -> Exit {
    let dir = match StateDir::open(state_dir) {
        Ok(dir) => dir,
        Err(e) => return error(log, e.token(), &e),
    };
    let config = match dir.file(config::CONFIG_FILE) {
        Ok(path) => match config::load_config(&path) {
            Ok(config) => config,
            Err(e) => return error(log, e.token(), &e),
        },
        Err(e) => return error(log, e.token(), &e),
    };
    let applied = status::read_status(&dir, log).epoch;
    // read before the listing: the sync renames state.json into place only after the table holds it
    let synced = match dir.read(sync::STATE_FILE) {
        Ok(Some(bytes)) => memory::load_state(Some(&bytes))
            .ok()
            .map(|state| memory::allow_ips(&state)),
        Ok(None) | Err(_) => None,
    };
    let listing = match status::live_table(nft) {
        Ok(Some(listing)) => listing,
        Ok(None) => return print(&Report::bare(Verdict::Absent), out, log),
        Err(e) => return error(log, "nft-unavailable", &e),
    };
    let snapshot = match fetch_snapshot(&RpcClient::new(config.rpc_port)) {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            log.line(Priority::Warning, "stale", &[("reason", &reason)]);
            return print(&Report::bare(Verdict::Stale), out, log);
        }
    };
    let report = judge(&snapshot, applied, synced.as_ref(), &listing);
    let snapshot_epoch = snapshot.epoch.to_string();
    let applied_epoch = applied.map(|epoch| epoch.to_string()).unwrap_or_default();
    let epochs = [
        ("snapshot_epoch", snapshot_epoch.as_str()),
        ("applied_epoch", applied_epoch.as_str()),
    ];
    match report.verdict {
        Verdict::Stale => {
            let [shown, last] = epochs;
            log.line(
                Priority::Warning,
                "stale",
                &[("reason", EPOCH_REGRESSED), shown, last],
            );
        }
        Verdict::Behind => log.line(Priority::Warning, "behind", &epochs),
        Verdict::Ok | Verdict::Missing | Verdict::Absent => {}
    }
    print(&report, out, log)
}

/// The snapshot from the node, or the stale reason, exactly as `sync` fetches it.
fn fetch_snapshot(rpc: &RpcClient) -> Result<Snapshot, String> {
    let result = rpc
        .call(SNAPSHOT_METHOD, Value::Array(Vec::new()))
        .map_err(|e| e.token())?;
    snapshot::parse(result).map_err(|e| e.token())
}

/// Logs an error that ends the run with nothing judged: exit 2.
fn error<E: Write>(log: &mut Log<E>, reason: &str, detail: &dyn std::fmt::Display) -> Exit {
    log.line(
        Priority::Error,
        "error",
        &[("reason", reason), ("detail", &detail.to_string())],
    );
    Exit::Error
}

/// Prints `report` and exits as its verdict says, or exit 2 when stdout cannot be written.
fn print<O: Write, E: Write>(report: &Report, out: &mut O, log: &mut Log<E>) -> Exit {
    match out
        .write_all(report.render().as_bytes())
        .and_then(|()| out.flush())
    {
        Ok(()) => report.verdict.exit(),
        Err(e) => error(log, "output-failed", &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::error::Error;

    use serde_json::json;

    type TestResult = Result<(), Box<dyn Error>>;

    /// A `bls_pubkey` made of the byte `n` and zeros.
    fn key(n: u8) -> String {
        format!("0x{n:02x}{}", "0".repeat(190))
    }

    fn quic(ip: &str) -> String {
        let family = if ip.contains(':') { "ip6" } else { "ip4" };
        format!("/{family}/{ip}/udp/49590/quic-v1")
    }

    /// An entry for key `n` holding `slots`, dialled at `ips`, with the signed record's own `record` IPs when given.
    fn entry(n: u8, slots: &[&str], ips: &[&str], record: Option<&[&str]>) -> Value {
        json!({
            "bls_pubkey": key(n),
            "slots": slots,
            "peer_id": null,
            "network_pubkey": null,
            "multiaddrs": ips.iter().map(|ip| quic(ip)).collect::<Vec<_>>(),
            "record_multiaddrs": record.map(|ips| ips.iter().map(|ip| quic(ip)).collect::<Vec<_>>()),
            "record_timestamp": null,
            "source": "record",
            "admitted": true,
        })
    }

    fn swarm(network: &str, epoch: u64, entries: Vec<Value>) -> Value {
        json!({
            "network": network,
            "worker_id": network.strip_prefix("worker-").and_then(|id| id.parse::<u16>().ok()),
            "available": true,
            "tracked_epoch": epoch,
            "admission": {
                "configured": "Closed", "effective": "Closed", "epoch": epoch, "fallback": null,
                "resolved_current": 4, "required_current": 3, "connected_current": 3, "contradicted": 0,
            },
            "entries": entries,
        })
    }

    fn snapshot(epoch: u64, swarms: Vec<Value>) -> Result<Snapshot, Box<dyn Error>> {
        Ok(snapshot::parse(
            json!({"version": 1, "epoch": epoch, "swarms": swarms}),
        )?)
    }

    /// A snapshot at epoch 42 whose primary lists `entries`.
    fn primary(entries: Vec<Value>) -> Result<Snapshot, Box<dyn Error>> {
        snapshot(42, vec![swarm(PRIMARY_SWARM, 42, entries)])
    }

    fn ip(text: &str) -> Result<IpAddr, Box<dyn Error>> {
        Ok(text.parse()?)
    }

    /// A listing whose allow sets hold `elements`, by family.
    fn listing(elements: &[Element]) -> Listing {
        let (v4, v6): (Vec<Element>, Vec<Element>) =
            elements.iter().partition(|element| match element {
                Element::Ip(ip) | Element::Cidr { net: ip, .. } | Element::Range(ip, _) => {
                    ip.is_ipv4()
                }
            });
        Listing {
            sets: BTreeMap::from([("allow_v4".to_owned(), v4), ("allow_v6".to_owned(), v6)]),
            ..Listing::default()
        }
    }

    fn hosts(texts: &[&str]) -> Result<Listing, Box<dyn Error>> {
        let elements = texts
            .iter()
            .map(|text| Ok(Element::Ip(ip(text)?)))
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        Ok(listing(&elements))
    }

    #[test]
    fn a_listed_committee_is_ok() -> TestResult {
        let snap = primary(vec![
            entry(1, &["current", "eligible"], &["203.0.113.10"], None),
            entry(2, &["eligible"], &["198.51.100.7"], None),
        ])?;
        let report = judge(&snap, Some(42), None, &hosts(&["203.0.113.10"])?);
        assert_eq!(report, Report::bare(Verdict::Ok));
        assert_eq!(
            report.render(),
            "result=ok missing_total=0 mismatch_total=0\n"
        );
        Ok(())
    }

    #[test]
    fn only_current_addresses_of_the_primary_can_be_missing() -> TestResult {
        let snap = snapshot(
            42,
            vec![
                swarm(
                    PRIMARY_SWARM,
                    42,
                    vec![
                        entry(
                            1,
                            &["previous", "current"],
                            &["203.0.113.10", "2001:db8::a"],
                            None,
                        ),
                        entry(2, &["next", "eligible"], &["198.51.100.7"], None),
                    ],
                ),
                swarm(
                    "worker-0",
                    42,
                    vec![entry(1, &["current"], &["192.0.2.50"], None)],
                ),
            ],
        )?;
        let report = judge(&snap, Some(42), None, &hosts(&[])?);
        assert_eq!(report.verdict, Verdict::Missing);
        assert_eq!(report.missing, [ip("203.0.113.10")?, ip("2001:db8::a")?]);
        assert_eq!(
            report.render(),
            "missing 203.0.113.10\nmissing 2001:db8::a\nresult=missing missing_total=2 mismatch_total=0\n"
        );
        let report = judge(
            &snap,
            Some(42),
            None,
            &hosts(&["203.0.113.10", "2001:db8::a"])?,
        );
        assert_eq!(report.verdict, Verdict::Ok);
        Ok(())
    }

    #[test]
    fn only_the_address_itself_counts_as_listed() -> TestResult {
        let snap = primary(vec![entry(1, &["current"], &["203.0.113.10"], None)])?;
        let host = listing(&[Element::Cidr {
            net: ip("203.0.113.10")?,
            prefix: 32,
        }]);
        assert_eq!(judge(&snap, Some(42), None, &host).verdict, Verdict::Ok);
        let covering = [
            Element::Cidr {
                net: ip("203.0.113.0")?,
                prefix: 24,
            },
            Element::Range(ip("203.0.113.1")?, ip("203.0.113.20")?),
        ];
        assert_eq!(
            judge(&snap, Some(42), None, &listing(&covering)).verdict,
            Verdict::Missing
        );
        Ok(())
    }

    #[test]
    fn the_applied_epoch_decides_behind_and_stale() -> TestResult {
        let snap = primary(vec![entry(
            1,
            &["current"],
            &["203.0.113.10"],
            Some(&["192.0.2.1"]),
        )])?;
        let empty = hosts(&[])?;
        // behind still compares, so the lines show what the next sync adds
        for applied in [None, Some(41)] {
            let report = judge(&snap, applied, None, &empty);
            assert_eq!(report.verdict, Verdict::Behind, "{applied:?}");
            assert_eq!(report.missing.len(), 2, "{applied:?}");
            assert_eq!(report.mismatch.len(), 1, "{applied:?}");
        }
        assert_eq!(
            judge(&snap, Some(42), None, &empty).verdict,
            Verdict::Missing
        );
        assert_eq!(
            judge(&snap, Some(43), None, &empty),
            Report::bare(Verdict::Stale)
        );
        Ok(())
    }

    #[test]
    fn a_gap_of_addresses_the_last_sync_never_held_is_behind() -> TestResult {
        let snap = primary(vec![
            entry(1, &["current"], &["203.0.113.10"], None),
            entry(2, &["current"], &["198.51.100.99"], None),
        ])?;
        let held = |texts: &[&str]| -> Result<BTreeSet<IpAddr>, Box<dyn Error>> {
            texts.iter().map(|text| ip(text)).collect()
        };
        let live = hosts(&["203.0.113.10"])?;
        // key 2 republished its record at a new address after the last sync
        let before = held(&["203.0.113.10", "198.51.100.7"])?;
        let report = judge(&snap, Some(42), Some(&before), &live);
        assert_eq!(report.verdict, Verdict::Behind);
        assert_eq!(report.missing, [ip("198.51.100.99")?]);
        // the last sync held it, so the table lost it
        let after = held(&["203.0.113.10", "198.51.100.99"])?;
        assert_eq!(
            judge(&snap, Some(42), Some(&after), &live).verdict,
            Verdict::Missing
        );
        // one lost address is a fault, whatever else the next sync adds
        let report = judge(&snap, Some(42), Some(&before), &hosts(&[])?);
        assert_eq!(report.verdict, Verdict::Missing);
        assert_eq!(report.missing.len(), 2);
        // the epoch rules come first
        assert_eq!(
            judge(&snap, Some(41), Some(&after), &live).verdict,
            Verdict::Behind
        );
        assert_eq!(
            judge(&snap, Some(43), Some(&before), &live),
            Report::bare(Verdict::Stale)
        );
        assert_eq!(
            judge(
                &snap,
                Some(42),
                Some(&before),
                &hosts(&["203.0.113.10", "198.51.100.99"])?
            ),
            Report::bare(Verdict::Ok)
        );
        Ok(())
    }

    #[test]
    fn a_mismatch_is_listed_once_and_never_changes_the_verdict() -> TestResult {
        let snap = snapshot(
            42,
            vec![
                swarm(
                    PRIMARY_SWARM,
                    42,
                    vec![
                        entry(0xab, &["current"], &["203.0.113.10"], Some(&["192.0.2.1"])),
                        // the same IPs in another order are no mismatch
                        entry(
                            2,
                            &["current"],
                            &["192.0.2.2", "192.0.2.3"],
                            Some(&["192.0.2.3", "192.0.2.2"]),
                        ),
                    ],
                ),
                swarm(
                    "worker-0",
                    42,
                    vec![entry(
                        0xab,
                        &["current"],
                        &["203.0.113.11"],
                        Some(&["192.0.2.1"]),
                    )],
                ),
            ],
        )?;
        let live = hosts(&["203.0.113.10", "192.0.2.1", "192.0.2.2", "192.0.2.3"])?;
        let report = judge(&snap, Some(42), None, &live);
        assert_eq!(report.verdict, Verdict::Ok);
        assert_eq!(report.mismatch, [key(0xab).parse::<BlsKey>()?]);
        assert_eq!(
            report.render(),
            "mismatch 0xab00000000000000\nresult=ok missing_total=0 mismatch_total=1\n"
        );
        Ok(())
    }

    #[test]
    fn lines_stop_at_five_and_the_totals_count_everything() -> TestResult {
        let entries = (1..=7_u8)
            .map(|n| {
                let dialled = format!("203.0.113.{n}");
                let record = format!("198.51.100.{n}");
                entry(n, &["current"], &[&dialled], Some(&[&record]))
            })
            .collect();
        let report = judge(&primary(entries)?, Some(42), None, &hosts(&[])?);
        assert_eq!(report.missing.len(), 14);
        assert_eq!(report.mismatch.len(), 7);
        let expected = [
            "missing 198.51.100.1",
            "missing 198.51.100.2",
            "missing 198.51.100.3",
            "missing 198.51.100.4",
            "missing 198.51.100.5",
            "mismatch 0x0100000000000000",
            "mismatch 0x0200000000000000",
            "mismatch 0x0300000000000000",
            "mismatch 0x0400000000000000",
            "mismatch 0x0500000000000000",
            "result=missing missing_total=14 mismatch_total=7",
        ];
        assert_eq!(report.render().lines().collect::<Vec<_>>(), expected);
        Ok(())
    }

    #[test]
    fn verdict_words_and_exits() {
        let table = [
            (Verdict::Ok, "ok", 0),
            (Verdict::Missing, "missing", 1),
            (Verdict::Absent, "absent", 1),
            (Verdict::Behind, "behind", 3),
            (Verdict::Stale, "stale", 3),
        ];
        for (verdict, word, code) in table {
            assert_eq!(verdict.as_str(), word);
            assert_eq!(verdict.exit().code(), code, "{word}");
        }
        assert_eq!(
            Report::bare(Verdict::Absent).render(),
            "result=absent missing_total=0 mismatch_total=0\n"
        );
    }
}
