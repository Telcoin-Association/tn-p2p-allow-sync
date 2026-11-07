//! The `status` file: flat `KEY=VALUE` lines that the deployment scripts and the `status` command read, and that command itself.
//!
//! `sync` rewrites it on every run that reached a decision, never on a busy run, a usage error or a refusal.
//! [`show`] prints it with the live facts of the table added, as text or as one JSON line, and never writes anything.
//! Every value matches `[A-Za-z0-9._:-]*`, so a bash reader matches each key with a regex and never unquotes anything.
//! Times are unix seconds, and an empty value means not known yet.
//!
//! The tool never writes the keys `enabled`, `persistent`, `timer` or `reason`: the scripts own those facts and add them to their own output.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::cli::Exit;
use crate::fsutil::{FsError, StateDir};
use crate::log::{Log, Priority};
use crate::nft::{Counter, Listing, Nft, NftError};
use crate::sync::COUNTERS;

/// Name of the status file inside the state dir.
pub const STATUS_FILE: &str = "status";

/// Mode of the status file.
pub const STATUS_MODE: u32 = 0o600;

/// The status format this build reads and writes, the value of `STATUS_VERSION`.
pub const STATUS_VERSION: u32 = 1;

/// `EPOCH_DURATION` until the node has reported one: 6 hours, in seconds.
pub const DEFAULT_EPOCH_DURATION: u64 = 21_600;

/// The version of this build, written as `TOOL_VERSION`.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a value outside the value grammar is written as; such a value is a bug in the writer.
pub const INVALID: &str = "invalid";

/// How the last run ended, the value of `RESULT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunResult {
    /// The program was loaded: `applied`.
    Applied,
    /// The program was already loaded and nft was not run: `unchanged`.
    Unchanged,
    /// The snapshot was not authoritative, so the last good program was kept: `stale`.
    Stale,
    /// A local error: the config, floor or state was invalid, a file could not be written, or nft failed: `error`.
    Error,
}

impl RunResult {
    /// The value as the file and the summary line write it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Unchanged => "unchanged",
            Self::Stale => "stale",
            Self::Error => "error",
        }
    }

    /// The result `name` stands for, or `None` for anything [`RunResult::as_str`] never writes.
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Applied, Self::Unchanged, Self::Stale, Self::Error]
            .into_iter()
            .find(|result| result.as_str() == name)
    }
}

impl fmt::Display for RunResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The content of the status file, one field per key.
///
/// The file holds the keys in this order, each on its own line: `STATUS_VERSION`, `TOOL_VERSION`, `RESULT`, `LAST_RUN`, `LAST_GOOD`, `EPOCH`, `EPOCH_DURATION`, `STALE_SINCE`, `STALE_REASON`, `ALLOWED_V4`, `ALLOWED_V6`, `STATIC_V4`, `STATIC_V6`, `FLOOR`, `KEYS`, `DNS_SKIPPED`, `UNPARSED`, `UNKNOWN_SLOTS`.
/// `STATUS_VERSION` is always [`STATUS_VERSION`] and has no field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// `TOOL_VERSION`: the version of the build that wrote the file.
    pub tool_version: String,
    /// `RESULT`: how the last run ended; empty before the first.
    pub result: Option<RunResult>,
    /// `LAST_RUN`: when the last run that reached a decision ended.
    pub last_run: Option<u64>,
    /// `LAST_GOOD`: when a run last applied the program or found it loaded.
    pub last_good: Option<u64>,
    /// `EPOCH`: the epoch of the snapshot the loaded program was built from.
    pub epoch: Option<u64>,
    /// `EPOCH_DURATION`: the epoch length the node reported, in seconds, or [`DEFAULT_EPOCH_DURATION`].
    pub epoch_duration: u64,
    /// `STALE_SINCE`: when the current run of stale or failed runs began; empty while fresh.
    pub stale_since: Option<u64>,
    /// `STALE_REASON`: the reason token of the last stale or failed run; empty while fresh.
    pub stale_reason: String,
    /// `ALLOWED_V4`: addresses in set `allow_v4`.
    pub allowed_v4: u64,
    /// `ALLOWED_V6`: addresses in set `allow_v6`.
    pub allowed_v6: u64,
    /// `STATIC_V4`: networks in set `static_v4`.
    pub static_v4: u64,
    /// `STATIC_V6`: networks in set `static_v6`.
    pub static_v6: u64,
    /// `FLOOR`: addresses in the floor file.
    pub floor: u64,
    /// `KEYS`: BLS keys in memory.
    pub keys: u64,
    /// `DNS_SKIPPED`: `dns*` multiaddrs in the snapshot, never resolved.
    pub dns_skipped: u64,
    /// `UNPARSED`: other multiaddrs in the snapshot without a leading IP.
    pub unparsed: u64,
    /// `UNKNOWN_SLOTS`: slot names in the snapshot this build does not know.
    pub unknown_slots: u64,
}

impl Default for Status {
    /// The status before the first run: nothing known, the default epoch duration, every count 0.
    fn default() -> Self {
        Self {
            tool_version: TOOL_VERSION.to_owned(),
            result: None,
            last_run: None,
            last_good: None,
            epoch: None,
            epoch_duration: DEFAULT_EPOCH_DURATION,
            stale_since: None,
            stale_reason: String::new(),
            allowed_v4: 0,
            allowed_v6: 0,
            static_v4: 0,
            static_v6: 0,
            floor: 0,
            keys: 0,
            dns_skipped: 0,
            unparsed: 0,
            unknown_slots: 0,
        }
    }
}

/// Whether `value` matches the value grammar `[A-Za-z0-9._:-]*`.
pub fn is_valid_value(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

impl Status {
    /// Reads the status file's bytes; `None`, a missing file, is [`Status::default`].
    ///
    /// The file is only ever this tool's output, so reading it never fails.
    /// Bytes that are not UTF-8 or another `STATUS_VERSION` give the default; an unknown key, a line without `=` and a value outside the grammar or unreadable for its key are ignored, leaving that field's default.
    pub fn load(bytes: Option<&[u8]>) -> Self {
        let mut status = Self::default();
        let Some(text) = bytes.and_then(|bytes| std::str::from_utf8(bytes).ok()) else {
            return status;
        };
        let fields = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(_, value)| is_valid_value(value));
        let mut version = None;
        for (key, value) in fields {
            match key {
                "STATUS_VERSION" => version = Some(value),
                "TOOL_VERSION" => value.clone_into(&mut status.tool_version),
                "RESULT" => status.result = RunResult::from_name(value),
                "LAST_RUN" => status.last_run = value.parse().ok(),
                "LAST_GOOD" => status.last_good = value.parse().ok(),
                "EPOCH" => status.epoch = value.parse().ok(),
                "EPOCH_DURATION" => {
                    status.epoch_duration = value
                        .parse()
                        .ok()
                        .filter(|duration| *duration > 0)
                        .unwrap_or(DEFAULT_EPOCH_DURATION);
                }
                "STALE_SINCE" => status.stale_since = value.parse().ok(),
                "STALE_REASON" => value.clone_into(&mut status.stale_reason),
                "ALLOWED_V4" => status.allowed_v4 = count(value),
                "ALLOWED_V6" => status.allowed_v6 = count(value),
                "STATIC_V4" => status.static_v4 = count(value),
                "STATIC_V6" => status.static_v6 = count(value),
                "FLOOR" => status.floor = count(value),
                "KEYS" => status.keys = count(value),
                "DNS_SKIPPED" => status.dns_skipped = count(value),
                "UNPARSED" => status.unparsed = count(value),
                "UNKNOWN_SLOTS" => status.unknown_slots = count(value),
                _ => {}
            }
        }
        if version == Some(STATUS_VERSION.to_string().as_str()) {
            status
        } else {
            Self::default()
        }
    }

    /// Every key with its value as the file writes it, in file order, `STATUS_VERSION` first.
    ///
    /// The values are not checked here; [`Status::render`] does that.
    pub fn fields(&self) -> [(&'static str, String); 18] {
        let time = |t: Option<u64>| t.map(|t| t.to_string()).unwrap_or_default();
        [
            ("STATUS_VERSION", STATUS_VERSION.to_string()),
            ("TOOL_VERSION", self.tool_version.clone()),
            (
                "RESULT",
                self.result
                    .map(|result| result.as_str().to_owned())
                    .unwrap_or_default(),
            ),
            ("LAST_RUN", time(self.last_run)),
            ("LAST_GOOD", time(self.last_good)),
            ("EPOCH", time(self.epoch)),
            ("EPOCH_DURATION", self.epoch_duration.to_string()),
            ("STALE_SINCE", time(self.stale_since)),
            ("STALE_REASON", self.stale_reason.clone()),
            ("ALLOWED_V4", self.allowed_v4.to_string()),
            ("ALLOWED_V6", self.allowed_v6.to_string()),
            ("STATIC_V4", self.static_v4.to_string()),
            ("STATIC_V6", self.static_v6.to_string()),
            ("FLOOR", self.floor.to_string()),
            ("KEYS", self.keys.to_string()),
            ("DNS_SKIPPED", self.dns_skipped.to_string()),
            ("UNPARSED", self.unparsed.to_string()),
            ("UNKNOWN_SLOTS", self.unknown_slots.to_string()),
        ]
    }

    /// The text of the file, one `KEY=VALUE` line per key, and the keys whose value broke the grammar.
    ///
    /// Such a value is written as [`INVALID`], so the file always parses; the caller warns about each key returned.
    pub fn render(&self) -> (String, Vec<&'static str>) {
        let mut text = String::new();
        let mut invalid = Vec::new();
        for (key, value) in self.fields() {
            let value = if is_valid_value(&value) {
                value
            } else {
                invalid.push(key);
                INVALID.to_owned()
            };
            text.push_str(key);
            text.push('=');
            text.push_str(&value);
            text.push('\n');
        }
        (text, invalid)
    }

    /// Writes the file into `dir` atomically with mode 0600 and returns the keys [`Status::render`] found invalid.
    ///
    /// The caller holds the lock.
    pub fn save(&self, dir: &StateDir) -> Result<Vec<&'static str>, FsError> {
        let (text, invalid) = self.render();
        dir.write_atomic(STATUS_FILE, text.as_bytes(), STATUS_MODE)?;
        Ok(invalid)
    }
}

/// The live table: its listing, `None` when the table is absent, or the error nft gave for either listing call.
///
/// `status` and `check` read the table this way and never change it.
pub fn live_table(nft: &Nft) -> Result<Option<Listing>, NftError> {
    if nft.table_present()? {
        nft.list_table().map(Some)
    } else {
        Ok(None)
    }
}

/// What the live table shows now: the facts `status` adds to the file's keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Live {
    /// `LOADED`: whether the table exists, or `None` when nft could not list it.
    pub loaded: Option<bool>,
    /// The accept counter, or `None` when nft could not list it, the table is absent or the counter is gone.
    pub accept: Option<Counter>,
    /// The drop counter, or `None` in the same cases as `accept`.
    pub drop: Option<Counter>,
}

impl Live {
    /// The live facts from what [`live_table`] returned; an error leaves every fact unknown.
    pub fn from_listing(listing: Result<Option<&Listing>, &NftError>) -> Self {
        let [accept, drop] = COUNTERS;
        match listing {
            Ok(Some(listing)) => Self {
                loaded: Some(true),
                accept: listing.counters.get(accept).copied(),
                drop: listing.counters.get(drop).copied(),
            },
            Ok(None) => Self {
                loaded: Some(false),
                ..Self::default()
            },
            Err(_) => Self::default(),
        }
    }
}

/// Seconds from `LAST_GOOD` to `now`, 0 when the clock went back, or `None` before the first good run.
pub fn age_secs(status: &Status, now: u64) -> Option<u64> {
    status.last_good.map(|good| now.saturating_sub(good))
}

/// Whether the last good run is more than two epochs old, or `None` before the first good run.
pub fn is_stale(status: &Status, now: u64) -> Option<bool> {
    age_secs(status, now).map(|age| age > status.epoch_duration.saturating_mul(2))
}

/// The output of `status`: the file's keys as [`Status::render`] writes them, then the live keys `LOADED`, `AGE_SECS`, `STALE` and `COUNTER_{ACCEPT,DROP}_{PACKETS,BYTES}`.
///
/// A live value that is not known is empty: `LOADED` and the counters when nft could not list the table, the counters when the table or a counter is absent, `AGE_SECS` and `STALE` before the first good run.
/// `AGE_SECS` and `STALE` come from the file and the clock, so a failing nft never empties them.
pub fn text(status: &Status, live: &Live, now: u64) -> String {
    let (mut text, _) = status.render();
    for (key, value) in [
        ("LOADED", known(live.loaded)),
        ("AGE_SECS", known(age_secs(status, now))),
        ("STALE", known(is_stale(status, now))),
        (
            "COUNTER_ACCEPT_PACKETS",
            known(live.accept.map(|c| c.packets)),
        ),
        ("COUNTER_ACCEPT_BYTES", known(live.accept.map(|c| c.bytes))),
        ("COUNTER_DROP_PACKETS", known(live.drop.map(|c| c.packets))),
        ("COUNTER_DROP_BYTES", known(live.drop.map(|c| c.bytes))),
    ] {
        text.push_str(key);
        text.push('=');
        text.push_str(&value);
        text.push('\n');
    }
    text
}

/// A live value as the text writes it: empty when it is not known.
fn known<T: ToString>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

/// The object `status --json` prints, its fields in the order the deployment scripts document.
#[derive(Debug, Serialize)]
struct JsonStatus<'a> {
    loaded: Option<bool>,
    epoch: Option<u64>,
    allowed_v4: u64,
    allowed_v6: u64,
    #[serde(rename = "static")]
    statics: u64,
    floor: u64,
    keys: u64,
    dns_skipped: u64,
    last_good: Option<u64>,
    age_secs: Option<u64>,
    stale: Option<bool>,
    stale_since: Option<u64>,
    stale_reason: Option<&'a str>,
    counters: JsonCounters,
}

/// The packet counts of the two counters.
#[derive(Debug, Serialize)]
struct JsonCounters {
    accepted: Option<u64>,
    dropped: Option<u64>,
}

/// The output of `status --json`: one line, a JSON object, then a newline.
///
/// The keys are `loaded`, `epoch`, `allowed_v4`, `allowed_v6`, `static` (both static sets), `floor`, `keys`, `dns_skipped`, `last_good`, `age_secs`, `stale`, `stale_since`, `stale_reason` and `counters`, an object of `accepted` and `dropped` packet counts.
/// A value that is not known is `null`, as is an empty `stale_reason`.
/// The keys `enabled`, `persistent`, `timer` and `reason` are never present: the scripts own those facts and add them in front.
pub fn json(status: &Status, live: &Live, now: u64) -> Result<String, serde_json::Error> {
    let object = JsonStatus {
        loaded: live.loaded,
        epoch: status.epoch,
        allowed_v4: status.allowed_v4,
        allowed_v6: status.allowed_v6,
        statics: status.static_v4.saturating_add(status.static_v6),
        floor: status.floor,
        keys: status.keys,
        dns_skipped: status.dns_skipped,
        last_good: status.last_good,
        age_secs: age_secs(status, now),
        stale: is_stale(status, now),
        stale_since: status.stale_since,
        stale_reason: Some(status.stale_reason.as_str()).filter(|reason| !reason.is_empty()),
        counters: JsonCounters {
            accepted: live.accept.map(|counter| counter.packets),
            dropped: live.drop.map(|counter| counter.packets),
        },
    };
    let mut line = serde_json::to_string(&object)?;
    line.push('\n');
    Ok(line)
}

/// `status [--json]`: prints the status file's keys and the live facts, as [`text`] or [`json`] lays them out.
///
/// It takes no lock and writes nothing, so it answers while a sync runs.
/// It exits 0 whenever the state dir opens: a missing or unreadable status file reads as the status before the first run, and nft failing to list the table leaves the live facts unknown, each with a warning on `log`.
/// An unsafe state dir is exit 2, as is a failed write to `out`.
pub fn show<O: Write, E: Write>(
    state_dir: &Path,
    nft: &Nft,
    as_json: bool,
    out: &mut O,
    log: &mut Log<E>,
) -> Exit {
    let dir = match StateDir::open(state_dir) {
        Ok(dir) => dir,
        Err(e) => {
            log.line(
                Priority::Error,
                "error",
                &[("reason", e.token()), ("detail", &e.to_string())],
            );
            return Exit::Error;
        }
    };
    let status = read_status(&dir, log);
    let listing = live_table(nft);
    if let Err(e) = &listing {
        log.line(
            Priority::Warning,
            "nft-unavailable",
            &[("detail", &e.to_string())],
        );
    }
    let live = Live::from_listing(listing.as_ref().map(Option::as_ref));
    let now = unix_now();
    let output = if as_json {
        match json(&status, &live, now) {
            Ok(line) => line,
            Err(e) => {
                log.line(
                    Priority::Error,
                    "error",
                    &[("reason", "output-failed"), ("detail", &e.to_string())],
                );
                return Exit::Error;
            }
        }
    } else {
        text(&status, &live, now)
    };
    match out.write_all(output.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Exit::Fresh,
        Err(e) => {
            log.line(
                Priority::Error,
                "error",
                &[("reason", "output-failed"), ("detail", &e.to_string())],
            );
            Exit::Error
        }
    }
}

/// The status file in `dir`, or the status before the first run with a warning when it cannot be read.
///
/// Readers that never write the file use this; a missing file is no warning.
pub fn read_status<E: Write>(dir: &StateDir, log: &mut Log<E>) -> Status {
    match dir.read(STATUS_FILE) {
        Ok(bytes) => Status::load(bytes.as_deref()),
        Err(e) => {
            log.line(
                Priority::Warning,
                "status-unreadable",
                &[("detail", &e.to_string())],
            );
            Status::default()
        }
    }
}

/// The current time in unix seconds, 0 for a clock set before 1970.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A count as the file writes it, or 0 when it does not parse.
fn count(value: &str) -> u64 {
    value.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    // tests build fixtures with std::fs directly
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use crate::support::TempDir;

    fn full() -> Status {
        Status {
            tool_version: "0.1.0".to_owned(),
            result: Some(RunResult::Stale),
            last_run: Some(1_762_300_000),
            last_good: Some(1_762_299_700),
            epoch: Some(42),
            epoch_duration: 86_400,
            stale_since: Some(1_762_299_800),
            stale_reason: "rpc-error--32001".to_owned(),
            allowed_v4: 120,
            allowed_v6: 7,
            static_v4: 2,
            static_v6: 1,
            floor: 9,
            keys: 64,
            dns_skipped: 3,
            unparsed: 1,
            unknown_slots: 4,
        }
    }

    #[test]
    fn the_default_pins_the_file_format() {
        let (text, invalid) = Status::default().render();
        assert!(invalid.is_empty());
        let expected = format!(
            "STATUS_VERSION=1\nTOOL_VERSION={TOOL_VERSION}\nRESULT=\nLAST_RUN=\nLAST_GOOD=\nEPOCH=\nEPOCH_DURATION=21600\nSTALE_SINCE=\nSTALE_REASON=\nALLOWED_V4=0\nALLOWED_V6=0\nSTATIC_V4=0\nSTATIC_V6=0\nFLOOR=0\nKEYS=0\nDNS_SKIPPED=0\nUNPARSED=0\nUNKNOWN_SLOTS=0\n"
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn every_field_round_trips() {
        for status in [
            Status::default(),
            full(),
            Status {
                result: Some(RunResult::Applied),
                stale_since: None,
                stale_reason: String::new(),
                ..full()
            },
            Status {
                result: Some(RunResult::Unchanged),
                ..full()
            },
            Status {
                result: Some(RunResult::Error),
                last_good: None,
                epoch: None,
                stale_reason: "nft-apply-failed".to_owned(),
                ..full()
            },
            Status {
                last_run: Some(u64::MAX),
                allowed_v4: u64::MAX,
                ..full()
            },
        ] {
            let (text, invalid) = status.render();
            assert!(invalid.is_empty(), "{invalid:?}");
            assert_eq!(Status::load(Some(text.as_bytes())), status, "{text}");
        }
    }

    #[test]
    fn the_four_script_owned_keys_are_never_written() {
        for status in [Status::default(), full()] {
            let (text, _) = status.render();
            for line in text.lines() {
                let (key, _) = line.split_once('=').unwrap_or((line, ""));
                let key = key.to_ascii_lowercase();
                assert!(
                    !["enabled", "persistent", "timer", "reason"].contains(&key.as_str()),
                    "{line}"
                );
            }
            for (key, _) in status.fields() {
                assert!(
                    !["enabled", "persistent", "timer", "reason"]
                        .contains(&key.to_ascii_lowercase().as_str()),
                    "{key}"
                );
            }
        }
    }

    #[test]
    fn a_value_outside_the_grammar_is_written_as_invalid() {
        let status = Status {
            tool_version: "0.1.0 beta".to_owned(),
            stale_reason: "bad\nREASON=injected".to_owned(),
            ..full()
        };
        let (text, invalid) = status.render();
        assert_eq!(invalid, ["TOOL_VERSION", "STALE_REASON"]);
        assert!(text.contains("\nTOOL_VERSION=invalid\n"), "{text}");
        assert!(text.contains("\nSTALE_REASON=invalid\n"), "{text}");
        assert_eq!(text.lines().count(), 18, "{text}");
        for line in text.lines() {
            let (_, value) = line.split_once('=').unwrap_or(("", "="));
            assert!(is_valid_value(value), "{line}");
        }
    }

    #[test]
    fn the_value_grammar() {
        for good in ["", "applied", "rpc-error--32601", "0.1.0", "a_b:c.D-9"] {
            assert!(is_valid_value(good), "{good}");
        }
        for bad in [
            " ",
            "a b",
            "a=b",
            "a/b",
            "\"x\"",
            "x\n",
            "caf\u{e9}",
            "+1",
            "$x",
        ] {
            assert!(!is_valid_value(bad), "{bad}");
        }
    }

    #[test]
    fn loading_is_tolerant() {
        assert_eq!(Status::load(None), Status::default());
        assert_eq!(Status::load(Some(b"")), Status::default());
        assert_eq!(Status::load(Some(b"\xff\xfe")), Status::default());
        assert_eq!(Status::load(Some(b"garbage\n")), Status::default());

        // another format version is not read at all
        let (text, _) = full().render();
        let v2 = text.replace("STATUS_VERSION=1", "STATUS_VERSION=2");
        assert_eq!(Status::load(Some(v2.as_bytes())), Status::default());
        let unversioned = text.replace("STATUS_VERSION=1\n", "");
        assert_eq!(
            Status::load(Some(unversioned.as_bytes())),
            Status::default()
        );

        // unknown keys, lines without `=` and bad values leave the defaults
        let odd = "STATUS_VERSION=1\nNEW_KEY=1\nnoise\nRESULT=bogus\nEPOCH=x\nEPOCH_DURATION=0\nKEYS=-1\nFLOOR=two words\nSTALE_SINCE=17\nSTALE_REASON=a/b\nLAST_GOOD=12\n";
        let loaded = Status::load(Some(odd.as_bytes()));
        assert_eq!(
            loaded,
            Status {
                stale_since: Some(17),
                last_good: Some(12),
                ..Status::default()
            }
        );
    }

    #[test]
    fn save_writes_mode_0600_atomically() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new("status-save")?;
        let dir = StateDir::open(temp.path())?;
        let invalid = full().save(&dir)?;
        assert!(invalid.is_empty());
        let meta = std::fs::metadata(temp.join(STATUS_FILE))?;
        assert_eq!(meta.permissions().mode() & 0o7777, STATUS_MODE);
        let bytes = dir.read(STATUS_FILE)?;
        assert_eq!(Status::load(bytes.as_deref()), full());
        let mut names = std::fs::read_dir(temp.path())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        names.sort();
        assert_eq!(names, [STATUS_FILE]);
        Ok(())
    }

    #[test]
    fn results_parse_back() {
        for result in [
            RunResult::Applied,
            RunResult::Unchanged,
            RunResult::Stale,
            RunResult::Error,
        ] {
            assert_eq!(RunResult::from_name(result.as_str()), Some(result));
            assert_eq!(result.to_string(), result.as_str());
        }
        assert_eq!(RunResult::from_name(""), None);
        assert_eq!(RunResult::from_name("Applied"), None);
    }

    fn counter(packets: u64, bytes: u64) -> Counter {
        Counter { packets, bytes }
    }

    fn loaded() -> Live {
        Live {
            loaded: Some(true),
            accept: Some(counter(12, 3456)),
            drop: Some(counter(7, 420)),
        }
    }

    /// `full()` 100 seconds after its `LAST_GOOD`.
    const NOW: u64 = 1_762_299_800;

    const SCRIPT_KEYS: [&str; 4] = ["enabled", "persistent", "timer", "reason"];

    #[test]
    fn the_json_line_holds_every_key_in_order_and_none_of_the_script_keys()
    -> Result<(), Box<dyn std::error::Error>> {
        let line = json(&full(), &loaded(), NOW)?;
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with("}\n"), "{line}");
        assert_eq!(
            line,
            concat!(
                r#"{"loaded":true,"epoch":42,"allowed_v4":120,"allowed_v6":7,"static":3,"floor":9,"keys":64,"#,
                r#""dns_skipped":3,"last_good":1762299700,"age_secs":100,"stale":false,"stale_since":1762299800,"#,
                r#""stale_reason":"rpc-error--32001","counters":{"accepted":12,"dropped":7}}"#,
                "\n"
            )
        );
        let value: serde_json::Value = serde_json::from_str(&line)?;
        let object = value.as_object().ok_or("not an object")?;
        let keys: Vec<&str> = object.keys().map(String::as_str).collect();
        let mut expected = vec![
            "loaded",
            "epoch",
            "allowed_v4",
            "allowed_v6",
            "static",
            "floor",
            "keys",
            "dns_skipped",
            "last_good",
            "age_secs",
            "stale",
            "stale_since",
            "stale_reason",
            "counters",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
        let counters = object
            .get("counters")
            .and_then(serde_json::Value::as_object)
            .ok_or("no counters object")?;
        for key in SCRIPT_KEYS {
            assert!(!object.contains_key(key), "{key}");
            assert!(!counters.contains_key(key), "{key}");
        }
        Ok(())
    }

    #[test]
    fn unknown_json_values_are_null() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            json(&Status::default(), &Live::default(), NOW)?,
            concat!(
                r#"{"loaded":null,"epoch":null,"allowed_v4":0,"allowed_v6":0,"static":0,"floor":0,"keys":0,"#,
                r#""dns_skipped":0,"last_good":null,"age_secs":null,"stale":null,"stale_since":null,"#,
                r#""stale_reason":null,"counters":{"accepted":null,"dropped":null}}"#,
                "\n"
            )
        );
        // a table without its counters: loaded, the counts unknown
        let live = Live {
            loaded: Some(false),
            ..Live::default()
        };
        assert!(json(&Status::default(), &live, NOW)?.starts_with(r#"{"loaded":false,"#));
        Ok(())
    }

    #[test]
    fn age_and_staleness_follow_last_good_and_two_epochs() {
        let status = Status {
            last_good: Some(1_000),
            epoch_duration: 100,
            ..Status::default()
        };
        for (now, age, stale) in [
            (1_000, 0, false),
            (1_200, 200, false),
            (1_201, 201, true),
            // a clock that went back reads as fresh
            (900, 0, false),
        ] {
            assert_eq!(age_secs(&status, now), Some(age), "{now}");
            assert_eq!(is_stale(&status, now), Some(stale), "{now}");
        }
        assert_eq!(age_secs(&Status::default(), NOW), None);
        assert_eq!(is_stale(&Status::default(), NOW), None);
    }

    #[test]
    fn the_text_is_the_file_then_the_live_keys() {
        let (file, _) = full().render();
        assert_eq!(
            text(&full(), &loaded(), NOW),
            format!(
                "{file}LOADED=true\nAGE_SECS=100\nSTALE=false\nCOUNTER_ACCEPT_PACKETS=12\nCOUNTER_ACCEPT_BYTES=3456\nCOUNTER_DROP_PACKETS=7\nCOUNTER_DROP_BYTES=420\n"
            )
        );
        let absent = Live {
            loaded: Some(false),
            ..Live::default()
        };
        assert_eq!(
            text(&full(), &absent, NOW),
            format!(
                "{file}LOADED=false\nAGE_SECS=100\nSTALE=false\nCOUNTER_ACCEPT_PACKETS=\nCOUNTER_ACCEPT_BYTES=\nCOUNTER_DROP_PACKETS=\nCOUNTER_DROP_BYTES=\n"
            )
        );
        let (file, _) = Status::default().render();
        let unknown = text(&Status::default(), &Live::default(), NOW);
        assert_eq!(
            unknown,
            format!(
                "{file}LOADED=\nAGE_SECS=\nSTALE=\nCOUNTER_ACCEPT_PACKETS=\nCOUNTER_ACCEPT_BYTES=\nCOUNTER_DROP_PACKETS=\nCOUNTER_DROP_BYTES=\n"
            )
        );
        for line in unknown.lines() {
            let value = line.split_once('=').map(|(_, value)| value);
            assert!(value.is_some_and(is_valid_value), "{line}");
        }
    }

    #[test]
    fn live_facts_come_from_the_listing() {
        let [accept, drop] = COUNTERS;
        let mut listing = Listing {
            sets: std::collections::BTreeMap::new(),
            counters: std::collections::BTreeMap::from([
                (accept.to_owned(), counter(12, 3456)),
                (drop.to_owned(), counter(7, 420)),
            ]),
            ..Listing::default()
        };
        assert_eq!(Live::from_listing(Ok(Some(&listing))), loaded());
        listing.counters.remove(drop);
        assert_eq!(
            Live::from_listing(Ok(Some(&listing))),
            Live {
                drop: None,
                ..loaded()
            }
        );
        assert_eq!(
            Live::from_listing(Ok(None)),
            Live {
                loaded: Some(false),
                accept: None,
                drop: None,
            }
        );
        assert_eq!(
            Live::from_listing(Err(&NftError::Permission)),
            Live::default()
        );
    }
}
