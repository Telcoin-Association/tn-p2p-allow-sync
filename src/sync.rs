//! `sync` and `render`: one locked run from the snapshot to the loaded program, and the dry run that prints the program.
//!
//! A `sync` run, in the order that decides what it may touch:
//!
//! 1. The state dir must pass [`StateDir::open`] (`state-dir-invalid`) and the lock must be free (busy, exit 4); otherwise nothing is touched.
//! 2. The config, the floor and `state.json` are read; an invalid one is a local error (`config-invalid`, `floor-invalid`, `state-invalid`, exit 2), and a missing floor is an empty one with a `floor-missing` warning.
//! 3. nft must list its tables (`nft-unavailable`, exit 2, nothing touched); a missing CAP_NET_ADMIN shows up here, before anything is applied.
//! 4. The snapshot is fetched, validated and folded into memory; a snapshot that is not authoritative, a regressed epoch and an empty dynamic set are stale (exit 3).
//! 5. The program is rendered.
//!    When it equals `current.nft` and the live table holds exactly the desired sets, chain `input` as the program declares it with the program's five rules, and both counters, nft is not run; otherwise the program is staged in `.current.nft.new`, loaded with `nft -f` and renamed to `current.nft`.
//!    A table that is gone by the time it is listed, deleted while the node was being asked, is loaded again the same way, and so is one whose listing the parser cannot read, with a `listing-unreadable` warning.
//! 6. `state.json` is written whenever its canonical bytes changed, once the program is loaded, then `status`.
//!
//! The stale path, which every exit 3 and every local error takes, lists the tables again unless the run has just loaded the program, keeps a present table as it is, reloads `current.nft` when the table is gone, never writes `state.json` or `current.nft`, and records the reason in `status` with `STALE_SINCE` set only once.
//! With `--require-authoritative` a stale run is refused instead: exit 1 with nothing written, not even `status`.
//!
//! Every run that wrote `status`, or was refused, prints one [`summary`] line on stdout; log lines go to stderr through [`Log`].

use std::collections::BTreeSet;
use std::fmt;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::addr::{self, Cidr};
use crate::cli::Exit;
use crate::config::{self, Config, ConfigError};
use crate::fsutil::{self, FsError, Lock, StateDir};
use crate::log::{Line, Log, Priority};
use crate::memory::{self, State};
use crate::nft::{Chain, Element, Expr, Listing, Nft, NftError};
use crate::render::{self, Program};
use crate::rpc::{self, RpcClient, RpcError};
use crate::snapshot::{self, Snapshot, SnapshotError, Warnings};
use crate::status::{self, RunResult, Status};

/// Name of the memory file inside the state dir.
pub const STATE_FILE: &str = "state.json";

/// Name of the program nft last loaded, inside the state dir.
pub const CURRENT_FILE: &str = "current.nft";

/// Name the program is staged under for `nft -f`; it is renamed to [`CURRENT_FILE`] once nft has loaded it.
pub const STAGED_FILE: &str = ".current.nft.new";

/// Mode of `state.json`, `current.nft` and the staged program.
pub const FILE_MODE: u32 = 0o600;

/// The JSON-RPC method that returns the snapshot.
pub const SNAPSHOT_METHOD: &str = "tn_admissionSnapshot";

/// The JSON-RPC method whose `epochDuration` gives `EPOCH_DURATION`, read best effort.
pub const EPOCH_INFO_METHOD: &str = "tn_getCurrentEpochInfo";

/// Rules chain `input` holds once the program is loaded: four accepts and the drop.
pub const INPUT_RULES: usize = 5;

/// The named counters the program declares.
pub const COUNTERS: [&str; 2] = ["tn_accept_p2p_listed", "tn_drop_p2p_unlisted"];

/// The protocol keyword and the set of each accept rule of the program, in chain order, as [`render`] writes them.
const ACCEPT_RULES: [(&str, &str); 4] = [
    ("ip", "allow_v4"),
    ("ip", "static_v4"),
    ("ip6", "allow_v6"),
    ("ip6", "static_v6"),
];

/// The token of an empty dynamic set: memory holds no address, so the program would admit only the floor and the static list.
pub const EMPTY_SET: &str = "empty-set";

/// The largest file `render --snapshot` reads, the same cap as an RPC body.
const MAX_SNAPSHOT_FILE: usize = rpc::MAX_BODY_BYTES;

/// The four sets of one program.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sets {
    /// Single IPv4 addresses: memory, the floor and the IPv4 hosts of the static list.
    pub allow_v4: BTreeSet<Ipv4Addr>,
    /// Single IPv6 addresses: memory, the floor and the IPv6 hosts of the static list.
    pub allow_v6: BTreeSet<Ipv6Addr>,
    /// The IPv4 networks of the static list.
    pub static_v4: BTreeSet<Cidr>,
    /// The IPv6 networks of the static list.
    pub static_v6: BTreeSet<Cidr>,
}

impl Sets {
    /// The sets for memory's addresses `dynamic`, the floor and the static list of `config`.
    ///
    /// The allow sets hold `dynamic`, the floor and every host entry ([`Cidr::is_host`]) of `P2P_ALLOW_FROM`; the static sets hold only its networks.
    /// Every input is already canonical: memory and the floor were checked on load, and the config passed its hosts through [`addr::canonical_ip`].
    pub fn build(dynamic: &BTreeSet<IpAddr>, floor: &BTreeSet<IpAddr>, config: &Config) -> Self {
        let hosts = config
            .static_v4
            .iter()
            .chain(&config.static_v6)
            .filter(|cidr| cidr.is_host())
            .map(|cidr| cidr.net);
        let (allow_v4, allow_v6) =
            addr::split_ips(dynamic.iter().chain(floor).copied().chain(hosts));
        let networks = |set: &BTreeSet<Cidr>| -> BTreeSet<Cidr> {
            set.iter().filter(|cidr| !cidr.is_host()).copied().collect()
        };
        Self {
            allow_v4,
            allow_v6,
            static_v4: networks(&config.static_v4),
            static_v6: networks(&config.static_v6),
        }
    }

    /// The program for these sets and `ports`, as [`render::render`] writes it.
    pub fn program(&self, ports: &BTreeSet<u16>) -> String {
        render::render(&Program {
            ports,
            allow_v4: &self.allow_v4,
            allow_v6: &self.allow_v6,
            static_v4: &self.static_v4,
            static_v6: &self.static_v6,
        })
    }

    /// Each set's name with its elements as [`normalise`]d listing elements, sorted.
    fn elements(&self) -> [(&'static str, Vec<Element>); 4] {
        let v4 = self.allow_v4.iter().map(|ip| Element::Ip(IpAddr::V4(*ip)));
        let v6 = self.allow_v6.iter().map(|ip| Element::Ip(IpAddr::V6(*ip)));
        let network = |cidr: &Cidr| Element::Cidr {
            net: cidr.net,
            prefix: cidr.prefix,
        };
        [
            ("allow_v4", sorted(v4.collect())),
            ("allow_v6", sorted(v6.collect())),
            (
                "static_v4",
                sorted(self.static_v4.iter().map(network).collect()),
            ),
            (
                "static_v6",
                sorted(self.static_v6.iter().map(network).collect()),
            ),
        ]
    }
}

fn sorted(mut elements: Vec<Element>) -> Vec<Element> {
    elements.sort_unstable();
    elements
}

/// `element` in the form the idempotence check compares: a range that is exactly one network is that network, and a /32 or /128 network is its address.
///
/// nft lists a single address of an interval set as a bare address, but another nft version or a hand-added element may print it as a full-length prefix.
/// nft 1.0.2 lists a network that reaches the top of the address space (`0.0.0.0/0`, `::/0`, an upper half `/1`) as a range from its network address to all ones.
pub fn normalise(element: Element) -> Element {
    let element = match element {
        Element::Range(first, last) => range_network(first, last).unwrap_or(element),
        other => other,
    };
    match element {
        Element::Cidr { net, prefix } if (Cidr { net, prefix }).is_host() => Element::Ip(net),
        other => other,
    }
}

/// The network that holds exactly the addresses `first` to `last`, or `None` when the range is not one aligned block of one family.
fn range_network(first: IpAddr, last: IpAddr) -> Option<Element> {
    let (width, low, high) = match (first, last) {
        (IpAddr::V4(low), IpAddr::V4(high)) => (32, u32::from(low).into(), u32::from(high).into()),
        (IpAddr::V6(low), IpAddr::V6(high)) => (128, u128::from(low), u128::from(high)),
        _ => return None,
    };
    let host = low ^ high;
    // a block's host bits are a run of trailing ones, all clear in its first address and all set in its last
    if host.trailing_ones() != host.count_ones() || low & host != 0 {
        return None;
    }
    let unused = 128u32.checked_sub(width)?;
    let prefix = host.leading_zeros().checked_sub(unused)?;
    Some(Element::Cidr {
        net: first,
        prefix: u8::try_from(prefix).ok()?,
    })
}

/// The declaration of chain `input` in the program: `type filter hook input priority -20; policy accept;`.
fn program_chain() -> Chain {
    Chain {
        kind: Some("filter".to_owned()),
        hook: Some("input".to_owned()),
        prio: Some(-20),
        policy: Some("accept".to_owned()),
    }
}

/// The [`INPUT_RULES`] rules of chain `input` in the program for `ports`, in chain order, as nft lists them.
fn program_rules(ports: &BTreeSet<u16>) -> Vec<Vec<Expr>> {
    let [accept, drop] = COUNTERS;
    let mut rules: Vec<Vec<Expr>> = ACCEPT_RULES
        .iter()
        .map(|(protocol, set)| {
            vec![
                Expr::UdpDport(ports.clone()),
                Expr::Saddr {
                    protocol: (*protocol).to_owned(),
                    set: (*set).to_owned(),
                },
                Expr::Counter(accept.to_owned()),
                Expr::Accept,
            ]
        })
        .collect();
    rules.push(vec![
        Expr::UdpDport(ports.clone()),
        Expr::Counter(drop.to_owned()),
        Expr::Drop,
    ]);
    rules
}

/// Whether the live table, as `listing` shows it, is the loaded program for `sets` and `ports`.
///
/// The four sets must hold exactly the desired elements, compared as parsed values after [`normalise`] and never as text.
/// Chain `input` must have the program's type, hook, priority and policy and hold exactly its [`INPUT_RULES`] rules in order, each with its ports, its source set, its counter and its verdict; both [`COUNTERS`] must exist; the table must not be dormant.
/// A set, a rule or a counter deleted by hand, a verdict, a policy or a port list changed by hand, and a table made dormant by hand make this false, so the next run re-applies and heals it: the program declares the table without flags, which wakes it.
pub fn listing_matches(listing: &Listing, sets: &Sets, ports: &BTreeSet<u16>) -> bool {
    !listing.dormant
        && listing.chain.as_ref() == Some(&program_chain())
        && listing.rules == program_rules(ports)
        && COUNTERS
            .iter()
            .all(|name| listing.counters.contains_key(*name))
        && sets.elements().into_iter().all(|(name, desired)| {
            listing.sets.get(name).is_some_and(|live| {
                sorted(live.iter().copied().map(normalise).collect()) == desired
            })
        })
}

/// Whether `sync` may skip `nft -f` and the write of `current.nft`: the program equals `current` byte for byte and the live table matches `sets` and `ports`.
///
/// `listing` is `None` when the table is absent, which always re-applies.
pub fn is_unchanged(
    program: &str,
    current: Option<&[u8]>,
    listing: Option<&Listing>,
    sets: &Sets,
    ports: &BTreeSet<u16>,
) -> bool {
    current == Some(program.as_bytes())
        && listing.is_some_and(|listing| listing_matches(listing, sets, ports))
}

/// The summary line of a run: `result=R reason=TOKEN epoch=N v4=N v6=N static=N floor=N keys=N dns_skipped=N unparsed=N`.
///
/// The values come from `status`: `reason` is `STALE_REASON`, empty when fresh; `epoch` is `EPOCH`, empty before the first good run; `v4` and `v6` are the allow sets, `static` counts both static sets, `keys` the keys in memory.
pub fn summary(result: &str, status: &Status) -> String {
    format!(
        "result={result} reason={reason} epoch={epoch} v4={v4} v6={v6} static={statics} floor={floor} keys={keys} dns_skipped={dns} unparsed={unparsed}",
        reason = status.stale_reason,
        epoch = status
            .epoch
            .map(|epoch| epoch.to_string())
            .unwrap_or_default(),
        v4 = status.allowed_v4,
        v6 = status.allowed_v6,
        statics = status.static_v4.saturating_add(status.static_v6),
        floor = status.floor,
        keys = status.keys,
        dns = status.dns_skipped,
        unparsed = status.unparsed,
    )
}

/// The counts of a program built from `sets`, the floor, `state` and the snapshot's `warnings`, on an otherwise default status.
fn counts(sets: &Sets, floor: &BTreeSet<IpAddr>, state: &State, warnings: &Warnings) -> Status {
    Status {
        epoch: Some(state.epoch),
        allowed_v4: count(sets.allow_v4.len()),
        allowed_v6: count(sets.allow_v6.len()),
        static_v4: count(sets.static_v4.len()),
        static_v6: count(sets.static_v6.len()),
        floor: count(floor.len()),
        keys: count(state.keys.len()),
        dns_skipped: warnings.dns_skipped,
        unparsed: warnings.unparsed,
        unknown_slots: warnings.unknown_slots,
        ..Status::default()
    }
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The current time in unix seconds, 0 for a clock set before 1970.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A local error: its reason token and what went wrong.
#[derive(Debug)]
struct Failure {
    reason: &'static str,
    detail: String,
}

impl Failure {
    fn new(reason: &'static str, detail: &dyn fmt::Display) -> Self {
        Self {
            reason,
            detail: detail.to_string(),
        }
    }
}

impl From<FsError> for Failure {
    fn from(error: FsError) -> Self {
        Self::new(error.token(), &error)
    }
}

impl From<ConfigError> for Failure {
    fn from(error: ConfigError) -> Self {
        Self::new(error.token(), &error)
    }
}

/// What a run reads from the state dir before it talks to nft or the node.
struct Inputs {
    config: Config,
    floor: BTreeSet<IpAddr>,
    /// Whether the floor file is missing, which [`config::load_floor`] reads as an empty floor.
    floor_missing: bool,
    state: State,
    /// `state.json` as read, `None` when missing, so the sync rewrites it only when its bytes change.
    state_bytes: Option<Vec<u8>>,
}

fn read_inputs(dir: &StateDir) -> Result<Inputs, Failure> {
    let config = config::load_config(&dir.file(config::CONFIG_FILE)?)?;
    let floor = config::load_floor(&dir.file(config::FLOOR_FILE)?)?;
    // a symlink or another non-regular file was refused above, so only a missing file is left
    let floor_missing = dir.mtime(config::FLOOR_FILE)?.is_none();
    let state_bytes = dir.read(STATE_FILE)?;
    let state = memory::load_state(state_bytes.as_deref()).map_err(|e| {
        Failure::new(
            e.token(),
            &format_args!(
                "{STATE_FILE} is not in the form this tool writes; recover with off, then on"
            ),
        )
    })?;
    Ok(Inputs {
        config,
        floor,
        floor_missing,
        state,
        state_bytes,
    })
}

/// The warnings the config and the floor deserve, once per run.
///
/// The setup writes `floor` beside `config`, so a missing floor was deleted: the run goes on without the floor's addresses and says so.
fn warn_inputs<E: Write>(log: &mut Log<E>, inputs: &Inputs) {
    let config = &inputs.config;
    if inputs.floor_missing {
        log.line(
            Priority::Warning,
            "floor-missing",
            &[(
                "detail",
                "no floor file; the floor is empty until capture-floor writes one",
            )],
        );
    }
    if config.catch_all {
        log.line(
            Priority::Warning,
            "static-catch-all",
            &[("detail", "P2P_ALLOW_FROM admits every source")],
        );
    }
    if config.covered_dropped > 0 {
        log.line(
            Priority::Warning,
            "static-covered",
            &[("dropped", &config.covered_dropped.to_string())],
        );
    }
}

/// One warning per kind of fault the snapshot held and the parser absorbed.
fn log_absorbed<E: Write>(log: &mut Log<E>, warnings: &Warnings) {
    for (kind, n) in [
        ("unknown-slots", warnings.unknown_slots),
        ("uppercase-keys", warnings.uppercase_keys),
        ("duplicate-keys", warnings.duplicate_keys),
        ("dns-skipped", warnings.dns_skipped),
        ("unparsed", warnings.unparsed),
        ("truncated-lists", warnings.truncated_lists),
        ("pool-ahead", warnings.pool_ahead),
    ] {
        if n > 0 {
            log.line(
                Priority::Warning,
                "absorbed",
                &[("kind", kind), ("count", &n.to_string())],
            );
        }
    }
}

/// Logs an error that ends the run before anything was written, as its closing line.
fn untouched<E: Write>(log: &mut Log<E>, reason: &str, detail: &dyn fmt::Display) -> Exit {
    log.last(
        Priority::Error,
        "error",
        &[("reason", reason), ("detail", &detail.to_string())],
    );
    Exit::Error
}

/// nft could not list the table: exit 2 with nothing touched, "run as root" when it was refused.
fn nft_unavailable<E: Write>(log: &mut Log<E>, error: &NftError) -> Exit {
    untouched(log, "nft-unavailable", error)
}

/// The snapshot from the node, or the stale reason.
fn fetch_snapshot(rpc: &RpcClient) -> Result<Snapshot, String> {
    let result = rpc
        .call(SNAPSHOT_METHOD, Value::Array(Vec::new()))
        .map_err(|e| e.token())?;
    snapshot::parse(result).map_err(|e| e.token())
}

/// The epoch duration the node reports, or `None` with a warning; a failure here never makes the run stale.
fn fetch_epoch_duration<E: Write>(rpc: &RpcClient, log: &mut Log<E>) -> Option<u64> {
    match rpc.call(EPOCH_INFO_METHOD, Value::Array(Vec::new())) {
        Ok(info) => {
            let duration = info
                .get("epochDuration")
                .and_then(Value::as_u64)
                .filter(|duration| *duration > 0);
            if duration.is_none() {
                log.line(
                    Priority::Warning,
                    "epoch-info-failed",
                    &[("reason", "no-epoch-duration")],
                );
            }
            duration
        }
        Err(e) => {
            log.line(
                Priority::Warning,
                "epoch-info-failed",
                &[("reason", &e.token())],
            );
            None
        }
    }
}

/// Writes the program to [`STAGED_FILE`] with mode 0600 and syncs it, removing a leftover of a run that died first.
///
/// [`StateDir::create`] creates it with `O_CREAT|O_EXCL|O_NOFOLLOW`, which never follows a symlink, so the program cannot be written through one.
/// The caller holds the lock, which is what makes removing a leftover safe.
fn stage(dir: &StateDir, program: &[u8]) -> Result<PathBuf, FsError> {
    dir.create(STAGED_FILE, program, FILE_MODE)
}

/// Renames the loaded [`STAGED_FILE`] to [`CURRENT_FILE`] and syncs the directory, so the rename survives a crash.
fn place(dir: &StateDir) -> Result<(), FsError> {
    dir.rename(STAGED_FILE, CURRENT_FILE)
}

/// What `sync` was asked to do.
#[derive(Debug, Clone, Copy)]
pub struct SyncOptions<'a> {
    /// The state dir, `--state-dir`.
    pub state_dir: &'a Path,
    /// The nft runner.
    pub nft: &'a Nft,
    /// `--require-authoritative`: refuse a stale run with exit 1 and write nothing.
    pub require_authoritative: bool,
    /// `--wait`: how long to wait for the lock, `None` for not at all.
    pub wait: Option<Duration>,
}

/// Runs `sync` under the lock: fetches the snapshot, updates memory and loads the program, or keeps the last good one.
///
/// The summary line goes to `out` and log lines to `log`; the module documentation gives the order of the steps.
pub fn sync<O: Write, E: Write>(options: &SyncOptions<'_>, out: &mut O, log: &mut Log<E>) -> Exit {
    let dir = match StateDir::open(options.state_dir) {
        Ok(dir) => dir,
        Err(e) => return untouched(log, e.token(), &e),
    };
    let _lock = match Lock::acquire(&dir, options.wait) {
        Ok(lock) => lock,
        Err(e @ FsError::Busy { .. }) => {
            log.line(Priority::Warning, "busy", &[("detail", &e.to_string())]);
            return Exit::Busy;
        }
        Err(e) => return untouched(log, e.token(), &e),
    };
    let previous = match dir.read(status::STATUS_FILE) {
        Ok(bytes) => Status::load(bytes.as_deref()),
        Err(e) => {
            // the file is rewritten below; only STALE_SINCE and the last counts are lost
            log.line(
                Priority::Warning,
                "status-unreadable",
                &[("detail", &e.to_string())],
            );
            Status::default()
        }
    };
    Run {
        dir: &dir,
        nft: options.nft,
        require_authoritative: options.require_authoritative,
        now: unix_now(),
        previous,
        epoch_duration: None,
        out,
        log,
    }
    .sync()
}

/// One `sync` run that holds the lock.
struct Run<'a, O: Write, E: Write> {
    dir: &'a StateDir,
    nft: &'a Nft,
    require_authoritative: bool,
    now: u64,
    /// The status the last run wrote, the base of a stale run's status.
    previous: Status,
    /// `epochDuration` as this run learned it.
    epoch_duration: Option<u64>,
    out: &'a mut O,
    log: &'a mut Log<E>,
}

impl<O: Write, E: Write> Run<'_, O, E> {
    fn sync(mut self) -> Exit {
        let inputs = match read_inputs(self.dir) {
            Ok(inputs) => inputs,
            Err(failure) => return self.local_error(&failure, None),
        };
        warn_inputs(self.log, &inputs);
        let present = match self.nft.table_present() {
            Ok(present) => present,
            Err(e) => return nft_unavailable(self.log, &e),
        };
        let Inputs {
            config,
            floor,
            mut state,
            state_bytes,
            ..
        } = inputs;
        let rpc = RpcClient::new(config.rpc_port);
        let snap = match fetch_snapshot(&rpc) {
            Ok(snap) => snap,
            Err(reason) => return self.stale(&reason),
        };
        if let Err(e) = memory::apply(&mut state, &snap) {
            return self.stale(e.token());
        }
        log_absorbed(self.log, &snap.warnings);
        self.epoch_duration = fetch_epoch_duration(&rpc, self.log);
        let dynamic = memory::allow_ips(&state);
        if dynamic.is_empty() {
            return self.stale(EMPTY_SET);
        }
        let sets = Sets::build(&dynamic, &floor, &config);
        let program = sets.program(&config.p2p_ports);
        let current = match self.dir.read(CURRENT_FILE) {
            Ok(current) => current,
            Err(e) => return self.local_error(&e.into(), None),
        };
        // the listing is one more nft call, needed only when the program itself is unchanged
        let listing = if present && current.as_deref() == Some(program.as_bytes()) {
            match self.nft.list_table() {
                Ok(listing) => Some(listing),
                // nft listed the table but in a form the comparison cannot read: applying the
                // program again is the only way back to a table it can
                Err(NftError::Parse(detail)) => {
                    self.log.line(
                        Priority::Warning,
                        "listing-unreadable",
                        &[("detail", &detail)],
                    );
                    None
                }
                // gone while the node was being asked: no listing, so the program is applied again
                Err(e) => match self.nft.table_present() {
                    Ok(false) => None,
                    _ => return nft_unavailable(self.log, &e),
                },
            }
        } else {
            None
        };
        let result = if is_unchanged(
            &program,
            current.as_deref(),
            listing.as_ref(),
            &sets,
            &config.p2p_ports,
        ) {
            RunResult::Unchanged
        } else {
            let staged = match stage(self.dir, program.as_bytes()) {
                Ok(staged) => staged,
                Err(e) => return self.local_error(&e.into(), None),
            };
            if let Err(e) = self.nft.load_file(&staged) {
                // the transaction failed whole, so the live table is as it was
                let _ = self.dir.remove(STAGED_FILE);
                return self.local_error(&Failure::new("nft-apply-failed", &e), None);
            }
            if let Err(e) = place(self.dir) {
                return self.local_error(&e.into(), Some(true));
            }
            RunResult::Applied
        };
        // memory moves even when the program does not (a first strike, a refreshed `seen`),
        // and losing that would keep a departed key forever
        let bytes = memory::serialize(&state);
        if state_bytes.as_deref() != Some(bytes.as_slice())
            && let Err(e) = self.dir.write_atomic(STATE_FILE, &bytes, FILE_MODE)
        {
            return self.local_error(&e.into(), Some(true));
        }
        let status = Status {
            result: Some(result),
            last_run: Some(self.now),
            last_good: Some(self.now),
            epoch_duration: self.epoch_duration.unwrap_or(self.previous.epoch_duration),
            ..counts(&sets, &floor, &state, &snap.warnings)
        };
        self.finish(status, Exit::Fresh, None)
    }

    /// A stale reason: the stale path, or a refusal under `--require-authoritative`.
    fn stale(self, reason: &str) -> Exit {
        if self.require_authoritative {
            self.log
                .last(Priority::Warning, "refused", &[("reason", reason)]);
            let status = Status {
                stale_reason: reason.to_owned(),
                ..self.previous.clone()
            };
            let line = summary(RunResult::Stale.as_str(), &status);
            return self.print(&line, Exit::Refused);
        }
        // sampled again: the table may have gone while the node was being asked
        self.keep(RunResult::Stale, reason, None, None)
    }

    /// A local error: the stale path with `RESULT=error`, exit 2.
    fn local_error(self, failure: &Failure, present: Option<bool>) -> Exit {
        self.keep(
            RunResult::Error,
            failure.reason,
            Some(&failure.detail),
            present,
        )
    }

    /// The stale path: protects the table, then records `reason` with `STALE_SINCE` set once, leaving every other value of the last status as it was.
    ///
    /// `present` is the table's presence when the run knows it from a load it just made; `None` lists the tables again, since the table may have gone while the node was being asked.
    fn keep(
        mut self,
        result: RunResult,
        reason: &str,
        detail: Option<&str>,
        present: Option<bool>,
    ) -> Exit {
        let (action, reload_error) = self.protect(present);
        let (priority, event) = match result {
            RunResult::Stale => (Priority::Warning, "stale"),
            _ => (Priority::Error, "error"),
        };
        let mut fields = vec![("reason", reason), ("action", action)];
        if let Some(detail) = detail {
            fields.push(("detail", detail));
        }
        let kept = Line {
            priority,
            event,
            fields: &fields,
        };
        let reload_fields = reload_error
            .as_deref()
            .map(|error| [("reason", "reload-failed"), ("detail", error)]);
        // a failed reload ends the run, right after the line that led to it
        let (result, reason, closing) = match &reload_fields {
            Some(fields) => {
                self.log.line(kept.priority, kept.event, kept.fields);
                let failed = Line {
                    priority: Priority::Error,
                    event: "error",
                    fields,
                };
                (RunResult::Error, "reload-failed", failed)
            }
            None => (result, reason, kept),
        };
        let status = Status {
            tool_version: status::TOOL_VERSION.to_owned(),
            result: Some(result),
            last_run: Some(self.now),
            stale_since: Some(self.previous.stale_since.unwrap_or(self.now)),
            stale_reason: reason.to_owned(),
            epoch_duration: self.epoch_duration.unwrap_or(self.previous.epoch_duration),
            ..self.previous.clone()
        };
        let exit = match result {
            RunResult::Stale => Exit::Stale,
            _ => Exit::Error,
        };
        self.finish(status, exit, Some(closing))
    }

    /// The stale path's care of the table: a present table is kept as it is, and an absent one is reloaded from `current.nft` when that exists.
    ///
    /// Returns the action, `kept`, `reloaded`, `none` or `reload-failed`, and why a reload failed.
    fn protect(&mut self, present: Option<bool>) -> (&'static str, Option<String>) {
        let present = match present {
            Some(present) => present,
            None => match self.nft.table_present() {
                Ok(present) => present,
                Err(e) => {
                    self.log.line(
                        Priority::Warning,
                        "nft-unavailable",
                        &[("detail", &e.to_string())],
                    );
                    return ("none", None);
                }
            },
        };
        if present {
            return ("kept", None);
        }
        let reloaded = match self.dir.mtime(CURRENT_FILE) {
            Ok(None) => return ("none", None),
            Ok(Some(_)) => self
                .dir
                .file(CURRENT_FILE)
                .map_err(|e| e.to_string())
                .and_then(|path| self.nft.load_file(&path).map_err(|e| e.to_string())),
            Err(e) => Err(e.to_string()),
        };
        match reloaded {
            Ok(()) => ("reloaded", None),
            Err(error) => ("reload-failed", Some(error)),
        }
    }

    /// Writes `status`, logs `closing`, then prints its summary line; a failed write turns the run into `state-io-failed`, exit 2.
    ///
    /// `closing` is the line the run ends with, if it has one.
    /// When the write fails, its `error` line ends the run instead, with `closing` logged just before it.
    fn finish(self, mut status: Status, mut exit: Exit, closing: Option<Line<'_>>) -> Exit {
        match status.save(self.dir) {
            Ok(invalid) => {
                for key in invalid {
                    // a bug in the writer: the value went out as `invalid`
                    self.log
                        .line(Priority::Warning, "status-value-invalid", &[("key", key)]);
                }
                if let Some(closing) = closing {
                    self.log
                        .last(closing.priority, closing.event, closing.fields);
                }
            }
            Err(e) => {
                if let Some(closing) = closing {
                    self.log
                        .line(closing.priority, closing.event, closing.fields);
                }
                self.log.last(
                    Priority::Error,
                    "error",
                    &[("reason", e.token()), ("detail", &e.to_string())],
                );
                status.result = Some(RunResult::Error);
                e.token().clone_into(&mut status.stale_reason);
                exit = Exit::Error;
            }
        }
        let line = summary(status.result.map_or("", RunResult::as_str), &status);
        self.print(&line, exit)
    }

    fn print(self, line: &str, exit: Exit) -> Exit {
        // the decision is already on disk; a closed stdout changes nothing about it
        let _ = writeln!(self.out, "{line}").and_then(|()| self.out.flush());
        exit
    }
}

/// What `render` was asked to do.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions<'a> {
    /// The state dir, `--state-dir`.
    pub state_dir: &'a Path,
    /// The nft runner, used only with `--check`.
    pub nft: &'a Nft,
    /// `--snapshot FILE`: read the snapshot from this file instead of the node.
    pub snapshot: Option<&'a Path>,
    /// `--check`: have nft check the program with `nft -c -f -`.
    pub check: bool,
}

/// Runs `render`: prints the program `sync` would load on `out` and the summary line on `log`, and writes nothing.
///
/// Memory comes from `state.json` and is updated in memory only.
/// A snapshot that is not authoritative, a regressed epoch and an empty dynamic set are refused with exit 1 and no program; the summary then says `result=stale` with the reason, and its epoch and counts are those of memory as `state.json` holds it.
/// The summary of a printed program says `result=rendered`.
/// The summary is the run's closing line, so the log's cap never drops it.
/// With `--check`, nft must list its tables before anything else happens and must accept the program, or the run exits 2 without printing it.
pub fn render<O: Write, E: Write>(
    options: &RenderOptions<'_>,
    out: &mut O,
    log: &mut Log<E>,
) -> Exit {
    let dir = match StateDir::open(options.state_dir) {
        Ok(dir) => dir,
        Err(e) => return untouched(log, e.token(), &e),
    };
    let inputs = match read_inputs(&dir) {
        Ok(inputs) => inputs,
        Err(failure) => return untouched(log, failure.reason, &failure.detail),
    };
    warn_inputs(log, &inputs);
    let Inputs {
        config,
        floor,
        mut state,
        ..
    } = inputs;
    if options.check
        && let Err(e) = options.nft.table_present()
    {
        return nft_unavailable(log, &e);
    }
    let snap = match options.snapshot {
        Some(path) => match fsutil::read_bounded(path, count(MAX_SNAPSHOT_FILE)) {
            Ok(bytes) => snapshot_from_bytes(&bytes),
            Err(e) => {
                return untouched(log, "snapshot-unreadable", &format_args!("{path:?}: {e}"));
            }
        },
        None => fetch_snapshot(&RpcClient::new(config.rpc_port)),
    };
    let refused = |reason: &str, state: &State, warnings: &Warnings| {
        let sets = Sets::build(&memory::allow_ips(state), &floor, &config);
        let status = Status {
            stale_reason: reason.to_owned(),
            ..counts(&sets, &floor, state, warnings)
        };
        summary(RunResult::Stale.as_str(), &status)
    };
    let snap = match snap {
        Ok(snap) => snap,
        Err(reason) => {
            log.last_plain(&refused(&reason, &state, &Warnings::default()));
            return Exit::Refused;
        }
    };
    // a refusal reports memory as state.json holds it, as the stale path of sync keeps it
    let stored = state.clone();
    if let Err(e) = memory::apply(&mut state, &snap) {
        log.last_plain(&refused(e.token(), &stored, &snap.warnings));
        return Exit::Refused;
    }
    log_absorbed(log, &snap.warnings);
    let dynamic = memory::allow_ips(&state);
    if dynamic.is_empty() {
        log.last_plain(&refused(EMPTY_SET, &stored, &snap.warnings));
        return Exit::Refused;
    }
    let sets = Sets::build(&dynamic, &floor, &config);
    let program = sets.program(&config.p2p_ports);
    if options.check
        && let Err(e) = options.nft.check_program(&program)
    {
        return untouched(log, "nft-check-failed", &e);
    }
    if let Err(e) = out.write_all(program.as_bytes()).and_then(|()| out.flush()) {
        return untouched(log, "output-failed", &e);
    }
    log.last_plain(&summary(
        "rendered",
        &counts(&sets, &floor, &state, &snap.warnings),
    ));
    Exit::Fresh
}

/// The snapshot in `bytes`, either a `result` object or a whole JSON-RPC response that holds one, or the stale reason.
///
/// A body past the RPC cap is `rpc-body-too-large` and bytes that are not JSON are `snapshot-malformed`, as from the node.
fn snapshot_from_bytes(bytes: &[u8]) -> Result<Snapshot, String> {
    if bytes.len() > MAX_SNAPSHOT_FILE {
        return Err(RpcError::BodyTooLarge.token());
    }
    let value =
        serde_json::from_slice::<Value>(bytes).map_err(|_| SnapshotError::Malformed.token())?;
    let result = match value {
        Value::Object(mut fields) => match fields.remove("result") {
            Some(result) => result,
            None => Value::Object(fields),
        },
        other => other,
    };
    snapshot::parse(result).map_err(|e| e.token())
}

#[cfg(test)]
mod tests {
    // tests build fixtures with std::fs directly
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use crate::nft::Counter;
    use std::collections::BTreeMap;
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use std::str::FromStr;

    fn ip(text: &str) -> IpAddr {
        IpAddr::from_str(text).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    }

    fn cidr(text: &str) -> Cidr {
        crate::addr::parse_cidr_or_host(text).unwrap_or(Cidr {
            net: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            prefix: 0,
        })
    }

    fn config(entries: &[&str]) -> Config {
        let (v4, v6): (Vec<Cidr>, Vec<Cidr>) = entries
            .iter()
            .map(|entry| cidr(entry))
            .partition(|cidr| cidr.net.is_ipv4());
        Config {
            rpc_port: 8545,
            p2p_ports: BTreeSet::from([49590, 49594]),
            static_v4: v4.into_iter().collect(),
            static_v6: v6.into_iter().collect(),
            catch_all: false,
            covered_dropped: 0,
        }
    }

    fn ips(texts: &[&str]) -> BTreeSet<IpAddr> {
        texts.iter().map(|text| ip(text)).collect()
    }

    fn sample() -> (Sets, Config) {
        let config = config(&[
            "198.51.100.9",
            "203.0.113.0/24",
            "2001:db8::1",
            "2001:db8:1::/48",
        ]);
        let sets = Sets::build(
            &ips(&["192.0.2.10", "2001:db8::10", "192.0.2.1"]),
            &ips(&["192.0.2.20", "192.0.2.10"]),
            &config,
        );
        (sets, config)
    }

    /// The P2P ports of [`config`].
    fn ports() -> BTreeSet<u16> {
        BTreeSet::from([49590, 49594])
    }

    /// The listing nft would print for `sets` on [`ports`], in the forms nft uses.
    fn listing_of(sets: &Sets) -> Listing {
        let mut listing = Listing {
            sets: BTreeMap::new(),
            counters: COUNTERS
                .iter()
                .map(|name| ((*name).to_owned(), Counter::default()))
                .collect(),
            chain: Some(program_chain()),
            rules: program_rules(&ports()),
            dormant: false,
        };
        for (name, elements) in sets.elements() {
            listing.sets.insert(name.to_owned(), elements);
        }
        listing
    }

    fn set_mut<'a>(listing: &'a mut Listing, name: &str) -> &'a mut Vec<Element> {
        listing.sets.entry(name.to_owned()).or_default()
    }

    #[test]
    fn hosts_go_to_the_allow_sets_and_networks_to_the_static_sets() {
        let (sets, _) = sample();
        assert_eq!(
            sets,
            Sets {
                allow_v4: ["192.0.2.1", "192.0.2.10", "192.0.2.20", "198.51.100.9"]
                    .iter()
                    .filter_map(|text| Ipv4Addr::from_str(text).ok())
                    .collect(),
                allow_v6: ["2001:db8::1", "2001:db8::10"]
                    .iter()
                    .filter_map(|text| Ipv6Addr::from_str(text).ok())
                    .collect(),
                static_v4: BTreeSet::from([cidr("203.0.113.0/24")]),
                static_v6: BTreeSet::from([cidr("2001:db8:1::/48")]),
            }
        );
    }

    #[test]
    fn the_program_holds_the_sets() {
        let (sets, config) = sample();
        let program = sets.program(&config.p2p_ports);
        assert!(program.starts_with(render::HEADER), "{program}");
        assert!(
            program.contains("add element inet tn_p2p_allow static_v4 { 203.0.113.0/24 }\n"),
            "{program}"
        );
        assert!(
            program.contains("add element inet tn_p2p_allow allow_v4 {\n    192.0.2.1,\n    192.0.2.10,\n    192.0.2.20,\n    198.51.100.9\n}\n"),
            "{program}"
        );
        assert!(!program.contains("198.51.100.9/32"), "{program}");
        assert!(program.contains("udp dport { 49590, 49594 }"), "{program}");
    }

    #[test]
    fn a_loaded_program_with_matching_sets_is_unchanged() {
        let (sets, config) = sample();
        let program = sets.program(&config.p2p_ports);
        let listing = listing_of(&sets);
        assert!(listing_matches(&listing, &sets, &config.p2p_ports));
        assert!(is_unchanged(
            &program,
            Some(program.as_bytes()),
            Some(&listing),
            &sets,
            &config.p2p_ports
        ));

        // empty sets list as empty element arrays
        let bare = Sets {
            allow_v4: sets.allow_v4.clone(),
            ..Sets::default()
        };
        assert!(listing_matches(&listing_of(&bare), &bare, &ports()));
    }

    #[test]
    fn anything_else_re_applies() {
        let (sets, config) = sample();
        let program = sets.program(&config.p2p_ports);
        let listing = listing_of(&sets);
        let current = program.as_bytes();

        let ports = &config.p2p_ports;

        // the table is absent, or current.nft is missing or differs by one byte
        assert!(!is_unchanged(&program, Some(current), None, &sets, ports));
        assert!(!is_unchanged(&program, None, Some(&listing), &sets, ports));
        let mut edited = current.to_vec();
        edited.push(b'\n');
        assert!(!is_unchanged(
            &program,
            Some(&edited),
            Some(&listing),
            &sets,
            ports
        ));
        // the configured ports are not the ports the live rules match
        assert!(!listing_matches(&listing, &sets, &BTreeSet::from([49590])));

        let mut broken: Vec<Listing> = Vec::new();
        // an element deleted by hand, an element added by hand, a set missing
        let mut l = listing.clone();
        set_mut(&mut l, "allow_v4").retain(|e| *e != Element::Ip(ip("192.0.2.10")));
        broken.push(l);
        let mut l = listing.clone();
        set_mut(&mut l, "allow_v6").push(Element::Ip(ip("2001:db8::99")));
        broken.push(l);
        let mut l = listing.clone();
        l.sets.remove("static_v6");
        broken.push(l);
        // a static network listed with another prefix, or as a range that is not that network
        let mut l = listing.clone();
        *set_mut(&mut l, "static_v4") = vec![Element::Cidr {
            net: ip("203.0.113.0"),
            prefix: 25,
        }];
        broken.push(l);
        let mut l = listing.clone();
        *set_mut(&mut l, "static_v4") =
            vec![Element::Range(ip("203.0.113.0"), ip("203.0.113.254"))];
        broken.push(l);
        // the chain lost a rule or gained one, lost them all, or is gone
        let mut l = listing.clone();
        l.rules.pop();
        broken.push(l);
        let mut l = listing.clone();
        l.rules.push(l.rules.first().cloned().unwrap_or_default());
        broken.push(l);
        let mut l = listing.clone();
        l.rules.clear();
        broken.push(l);
        let mut l = listing.clone();
        l.chain = None;
        broken.push(l);
        // the chain declared with another type, hook, priority or policy, or as a regular chain
        let declared = [
            Chain {
                kind: Some("nat".to_owned()),
                ..program_chain()
            },
            Chain {
                hook: Some("output".to_owned()),
                ..program_chain()
            },
            Chain {
                prio: Some(0),
                ..program_chain()
            },
            Chain {
                policy: Some("drop".to_owned()),
                ..program_chain()
            },
            Chain::default(),
        ];
        for chain in declared {
            let mut l = listing.clone();
            l.chain = Some(chain);
            broken.push(l);
        }
        // a rule edited by hand: the drop rule's verdict, an accept rule's verdict, set, counter
        // or ports, an expression added, two rules swapped, and every rule on other ports
        let edited = |index: usize, edit: &dyn Fn(&mut Vec<Expr>)| {
            let mut l = listing.clone();
            if let Some(rule) = l.rules.get_mut(index) {
                edit(rule);
            }
            l
        };
        broken.push(edited(4, &|rule| {
            if let Some(verdict) = rule.last_mut() {
                *verdict = Expr::Accept;
            }
        }));
        broken.push(edited(0, &|rule| {
            if let Some(verdict) = rule.last_mut() {
                *verdict = Expr::Drop;
            }
        }));
        broken.push(edited(1, &|rule| {
            if let Some(Expr::Saddr { set, .. }) = rule.get_mut(1) {
                "allow_v4".clone_into(set);
            }
        }));
        broken.push(edited(2, &|rule| {
            if let Some(Expr::Counter(name)) = rule.get_mut(2) {
                "tn_drop_p2p_unlisted".clone_into(name);
            }
        }));
        broken.push(edited(3, &|rule| {
            if let Some(Expr::UdpDport(ports)) = rule.get_mut(0) {
                ports.insert(22);
            }
        }));
        broken.push(edited(4, &|rule| rule.insert(0, Expr::Other)));
        broken.push(edited(0, &|rule| rule.clear()));
        let mut l = listing.clone();
        l.rules.swap(0, 1);
        broken.push(l);
        let mut l = listing.clone();
        l.rules = program_rules(&BTreeSet::from([1]));
        broken.push(l);
        // a counter is gone
        for name in COUNTERS {
            let mut l = listing.clone();
            l.counters.remove(name);
            broken.push(l);
        }
        for (n, l) in broken.iter().enumerate() {
            assert_ne!(*l, listing, "case {n} changed nothing");
            assert!(!listing_matches(l, &sets, ports), "case {n}: {l:?}");
            assert!(
                !is_unchanged(&program, Some(current), Some(l), &sets, ports),
                "case {n}"
            );
        }
    }

    #[test]
    fn a_dormant_table_re_applies_even_when_everything_else_matches() {
        let (sets, config) = sample();
        let program = sets.program(&config.p2p_ports);
        let ports = &config.p2p_ports;
        let awake = listing_of(&sets);
        assert!(is_unchanged(
            &program,
            Some(program.as_bytes()),
            Some(&awake),
            &sets,
            ports
        ));
        // the same sets, rules and counters, counters with traffic included: the table enforces nothing
        let mut dormant = Listing {
            dormant: true,
            ..awake
        };
        for counter in dormant.counters.values_mut() {
            *counter = Counter {
                packets: 1,
                bytes: 29,
            };
        }
        assert!(!listing_matches(&dormant, &sets, ports));
        assert!(!is_unchanged(
            &program,
            Some(program.as_bytes()),
            Some(&dormant),
            &sets,
            ports
        ));
    }

    #[test]
    fn full_length_prefixes_compare_as_addresses_in_any_order() {
        let (sets, _) = sample();
        let mut listing = listing_of(&sets);
        for elements in listing.sets.values_mut() {
            elements.reverse();
        }
        for (name, prefix) in [("allow_v4", 32), ("allow_v6", 128)] {
            for element in set_mut(&mut listing, name).iter_mut() {
                if let Element::Ip(net) = *element {
                    *element = Element::Cidr { net, prefix };
                }
            }
        }
        assert!(listing_matches(&listing, &sets, &ports()));

        assert_eq!(
            normalise(Element::Cidr {
                net: ip("192.0.2.1"),
                prefix: 32
            }),
            Element::Ip(ip("192.0.2.1"))
        );
        assert_eq!(
            normalise(Element::Cidr {
                net: ip("2001:db8::1"),
                prefix: 128
            }),
            Element::Ip(ip("2001:db8::1"))
        );
        // a /32 written for an IPv6 address is a network, not a host
        let v6_32 = Element::Cidr {
            net: ip("2001:db8::"),
            prefix: 32,
        };
        assert_eq!(normalise(v6_32), v6_32);
    }

    #[test]
    fn a_range_that_is_one_network_compares_as_that_network() {
        let range = |first: &str, last: &str| Element::Range(ip(first), ip(last));
        let network = |net: &str, prefix: u8| Element::Cidr {
            net: ip(net),
            prefix,
        };
        let all_ones = "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff";
        for (listed, compared) in [
            // nft 1.0.2 lists a network reaching the top of the address space this way
            (range("0.0.0.0", "255.255.255.255"), network("0.0.0.0", 0)),
            (
                range("128.0.0.0", "255.255.255.255"),
                network("128.0.0.0", 1),
            ),
            (range("::", all_ones), network("::", 0)),
            (range("8000::", all_ones), network("8000::", 1)),
            // any other aligned block, and a block of one address
            (
                range("203.0.113.0", "203.0.113.255"),
                network("203.0.113.0", 24),
            ),
            (
                range("2001:db8:1::", "2001:db8:1:ffff:ffff:ffff:ffff:ffff"),
                network("2001:db8:1::", 48),
            ),
            (
                range("192.0.2.7", "192.0.2.7"),
                Element::Ip(ip("192.0.2.7")),
            ),
            (
                range("2001:db8::7", "2001:db8::7"),
                Element::Ip(ip("2001:db8::7")),
            ),
        ] {
            assert_eq!(normalise(listed), compared, "{listed:?}");
        }

        // not aligned, not a power of two, reversed, or two families: still a range
        for listed in [
            range("0.0.0.1", "255.255.255.255"),
            range("128.0.0.0", "255.255.255.254"),
            range("203.0.113.0", "203.0.113.254"),
            range("203.0.113.128", "203.0.114.127"),
            range("203.0.113.255", "203.0.113.0"),
            range("::1", all_ones),
            range("8000::", "ffff::"),
            range("0.0.0.0", all_ones),
        ] {
            assert_eq!(normalise(listed), listed, "{listed:?}");
        }

        // the live table lists the static networks of the sample as ranges and still matches
        let (sets, _) = sample();
        let mut listing = listing_of(&sets);
        *set_mut(&mut listing, "static_v4") = vec![range("203.0.113.0", "203.0.113.255")];
        *set_mut(&mut listing, "static_v6") =
            vec![range("2001:db8:1::", "2001:db8:1:ffff:ffff:ffff:ffff:ffff")];
        assert!(listing_matches(&listing, &sets, &ports()));
    }

    #[test]
    fn the_captured_listing_of_the_reference_program_matches_its_sets_and_ports()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixtures = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nft"));
        // the fixture fake answers the listing call with what nft 1.0.9 printed for this program
        let listing = Nft::new(Some(&fixtures.join("fake-nft.sh"))).list_table()?;
        let config = config(&["203.0.113.0/24"]);
        let sets = Sets::build(
            &ips(&["192.0.2.10", "198.51.100.7"]),
            &BTreeSet::new(),
            &config,
        );
        assert_eq!(
            sets.program(&config.p2p_ports),
            fs::read_to_string(fixtures.join("capture/program.nft"))?
        );
        assert_eq!(program_rules(&config.p2p_ports).len(), INPUT_RULES);
        assert!(listing_matches(&listing, &sets, &config.p2p_ports));
        assert!(!listing_matches(&listing, &sets, &BTreeSet::from([49590])));
        Ok(())
    }

    #[test]
    fn the_summary_line_format() {
        let status = Status {
            result: Some(RunResult::Applied),
            epoch: Some(42),
            allowed_v4: 4,
            allowed_v6: 2,
            static_v4: 1,
            static_v6: 1,
            floor: 2,
            keys: 3,
            dns_skipped: 5,
            unparsed: 6,
            unknown_slots: 7,
            ..Status::default()
        };
        assert_eq!(
            summary("applied", &status),
            "result=applied reason= epoch=42 v4=4 v6=2 static=2 floor=2 keys=3 dns_skipped=5 unparsed=6"
        );
        let stale = Status {
            stale_reason: "rpc-unreachable".to_owned(),
            ..Status::default()
        };
        assert_eq!(
            summary("stale", &stale),
            "result=stale reason=rpc-unreachable epoch= v4=0 v6=0 static=0 floor=0 keys=0 dns_skipped=0 unparsed=0"
        );
    }

    #[test]
    fn counts_describe_the_program() {
        let (sets, _) = sample();
        let state = State {
            epoch: 9,
            ..State::default()
        };
        let warnings = Warnings {
            dns_skipped: 1,
            unparsed: 2,
            unknown_slots: 3,
            ..Warnings::default()
        };
        let status = counts(
            &sets,
            &ips(&["192.0.2.20", "192.0.2.10"]),
            &state,
            &warnings,
        );
        assert_eq!(
            (
                status.epoch,
                status.allowed_v4,
                status.allowed_v6,
                status.static_v4,
                status.static_v6,
                status.floor,
                status.keys,
                status.dns_skipped,
                status.unparsed,
                status.unknown_slots,
            ),
            (Some(9), 4, 2, 1, 1, 2, 0, 1, 2, 3)
        );
    }

    const GOOD: &str = include_str!("../tests/fixtures/snapshot/good.json");

    #[test]
    fn a_snapshot_file_holds_a_result_or_a_whole_response() {
        let bare = snapshot_from_bytes(GOOD.as_bytes());
        let wrapped = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{GOOD}}}");
        let wrapped = snapshot_from_bytes(wrapped.as_bytes());
        assert!(bare.is_ok(), "{bare:?}");
        assert_eq!(bare, wrapped);

        for (bytes, reason) in [
            (&b"not json"[..], "snapshot-malformed"),
            (b"[]", "snapshot-malformed"),
            (b"{\"result\":[]}", "snapshot-malformed"),
            (b"{\"result\":{\"version\":2}}", "snapshot-version-2"),
            (
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"no\"}}",
                "snapshot-malformed",
            ),
        ] {
            assert_eq!(
                snapshot_from_bytes(bytes).map(|_| ()),
                Err(reason.to_owned()),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
        let huge = vec![b' '; MAX_SNAPSHOT_FILE.saturating_add(1)];
        assert_eq!(
            snapshot_from_bytes(&huge).map(|_| ()),
            Err("rpc-body-too-large".to_owned())
        );
    }

    #[test]
    fn a_pool_ahead_of_its_window_is_logged_once_with_the_other_kinds() {
        let logged = |warnings: &Warnings| {
            let mut out = Vec::new();
            log_absorbed(&mut Log::new(&mut out, false), warnings);
            String::from_utf8_lossy(&out).into_owned()
        };
        let ahead = Warnings {
            pool_ahead: 1,
            ..Warnings::default()
        };
        assert_eq!(logged(&ahead), "absorbed kind=pool-ahead count=1\n");

        let both = Warnings {
            pool_ahead: 1,
            dns_skipped: 2,
            ..Warnings::default()
        };
        let text = logged(&both);
        assert_eq!(text.matches("kind=pool-ahead").count(), 1, "{text}");
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "absorbed kind=dns-skipped count=2",
                "absorbed kind=pool-ahead count=1"
            ],
        );
        assert_eq!(logged(&Warnings::default()), "");
    }

    #[test]
    fn staging_replaces_a_leftover_and_placing_renames_it() -> Result<(), Box<dyn std::error::Error>>
    {
        use crate::support::TempDir;

        let temp = TempDir::new("sync-stage")?;
        let dir = StateDir::open(temp.path())?;
        std::fs::write(temp.join(STAGED_FILE), "leftover")?;
        let staged = stage(&dir, b"program\n")?;
        assert_eq!(staged, temp.join(STAGED_FILE));
        assert_eq!(std::fs::read(&staged)?, b"program\n");
        assert_eq!(
            std::fs::metadata(&staged)?.permissions().mode() & 0o7777,
            FILE_MODE
        );
        place(&dir)?;
        assert_eq!(std::fs::read(temp.join(CURRENT_FILE))?, b"program\n");
        assert!(!staged.exists());

        // a symlink planted under the staged name is removed, never written through
        let target = temp.join("target");
        std::fs::write(&target, "untouched")?;
        std::os::unix::fs::symlink(&target, temp.join(STAGED_FILE))?;
        stage(&dir, b"again\n")?;
        assert_eq!(std::fs::read(&target)?, b"untouched");
        assert!(
            !std::fs::symlink_metadata(temp.join(STAGED_FILE))?
                .file_type()
                .is_symlink()
        );
        Ok(())
    }

    #[test]
    fn staging_removes_an_empty_directory_in_the_way_and_names_a_full_one()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::support::TempDir;

        let temp = TempDir::new("sync-stage-dir")?;
        let dir = StateDir::open(temp.path())?;
        let leftover = temp.join(STAGED_FILE);
        std::fs::create_dir(&leftover)?;
        assert_eq!(stage(&dir, b"program\n")?, leftover);
        assert_eq!(std::fs::read(&leftover)?, b"program\n");

        std::fs::remove_file(&leftover)?;
        std::fs::create_dir(&leftover)?;
        std::fs::write(leftover.join("kept"), "kept")?;
        let error = match stage(&dir, b"program\n") {
            Err(e) => e,
            Ok(path) => return Err(format!("staged {path:?} past a full directory").into()),
        };
        assert_eq!(error.token(), "state-io-failed");
        let message = error.to_string();
        assert!(
            message.starts_with(&format!(
                "{leftover:?}: cannot remove the leftover: a directory is in the way: "
            )),
            "{message}"
        );
        assert_eq!(std::fs::read(leftover.join("kept"))?, b"kept");
        Ok(())
    }

    /// One `sync` of `state` through `nft`, with `rpc` serving the good snapshot: the exit, stdout and stderr.
    fn sync_once(
        state: &Path,
        nft: &Nft,
        rpc: &mut crate::support::fake_rpc::FakeRpc,
    ) -> Result<(Exit, String, String), Box<dyn std::error::Error>> {
        use crate::support::fake_rpc;

        let snapshot = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/snapshot/good.json"
        ))?;
        rpc.set(vec![
            fake_rpc::result(&snapshot),
            fake_rpc::result(r#"{"epochId":42,"epochDuration":86400}"#),
        ])?;
        let options = SyncOptions {
            state_dir: state,
            nft,
            require_authoritative: false,
            wait: None,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut log = Log::new(&mut err, false);
        let exit = sync(&options, &mut out, &mut log);
        log.finish();
        Ok((exit, String::from_utf8(out)?, String::from_utf8(err)?))
    }

    #[test]
    fn a_listing_the_parser_cannot_read_re_applies_the_program()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::support::{FakeNft, TempDir, fake_rpc::FakeRpc};

        let temp = TempDir::new("sync-unreadable")?;
        let state = temp.join("state");
        std::fs::create_dir(&state)?;
        std::fs::set_permissions(&state, Permissions::from_mode(0o700))?;
        let fake = FakeNft::install(temp.path())?;
        let nft = Nft::new(Some(&fake.path()));
        let mut rpc = FakeRpc::start(Vec::new())?;
        let config = state.join(config::CONFIG_FILE);
        std::fs::write(
            &config,
            format!(
                "CONFIG_VERSION=1\nRPC_PORT={}\nP2P_PORTS=49590,49594\nP2P_ALLOW_FROM=\n",
                rpc.port()
            ),
        )?;
        std::fs::set_permissions(&config, Permissions::from_mode(0o600))?;
        let result = |out: &str| out.split(' ').next().unwrap_or_default().to_owned();

        let (exit, out, err) = sync_once(&state, &nft, &mut rpc)?;
        assert_eq!(
            (exit, result(&out)),
            (Exit::Fresh, "result=applied".to_owned()),
            "{err}"
        );
        let loaded = fake.table()?.ok_or("no table after the first run")?;

        // a set the program does not declare, whatever it holds, is not read and changes nothing
        fake.set_table(&format!("{loaded}set ports inet_service\nelem ports 80\n"))?;
        let (exit, out, err) = sync_once(&state, &nft, &mut rpc)?;
        assert_eq!(
            (exit, result(&out)),
            (Exit::Fresh, "result=unchanged".to_owned()),
            "{err}"
        );

        // an element of one of the program's sets that the parser cannot read
        fake.set_table(&format!("{loaded}elem allow_v4 192.0.2.300\n"))?;
        let (exit, out, err) = sync_once(&state, &nft, &mut rpc)?;
        assert_eq!(
            (exit, result(&out)),
            (Exit::Fresh, "result=applied".to_owned()),
            "{err}"
        );
        let unreadable: Vec<&str> = err
            .lines()
            .filter(|line| line.starts_with("listing-unreadable "))
            .collect();
        assert_eq!(
            unreadable,
            [r#"listing-unreadable detail="set allow_v4: \"192.0.2.300\" is not an IP address""#]
        );
        assert_eq!(fake.loaded()?.len(), 2);
        // the re-apply replaced the table, so the next run finds it as the program left it
        assert_eq!(fake.table()?.as_deref(), Some(loaded.as_str()));
        let (exit, out, err) = sync_once(&state, &nft, &mut rpc)?;
        assert_eq!(
            (exit, result(&out)),
            (Exit::Fresh, "result=unchanged".to_owned()),
            "{err}"
        );
        assert!(!err.contains("listing-unreadable"), "{err}");
        Ok(())
    }
}
