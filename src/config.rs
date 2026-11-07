//! The config file the scripts write, the floor file `capture-floor` writes, and the scan that builds the floor.
//!
//! Config grammar, version 1, one `KEY=VALUE` per line:
//!
//! ```text
//! # tn-p2p-allow-sync config, written by firewall-setup.sh
//! CONFIG_VERSION=1
//! RPC_PORT=8545
//! P2P_PORTS=49590,49594
//! P2P_ALLOW_FROM=203.0.113.0/24,198.51.100.9
//! ```
//!
//! A line whose first byte is `#` is a comment and an empty line is skipped.
//! Every other line is printable ASCII without spaces or quotes, so nothing is ever unquoted.
//! Each of the four keys appears exactly once; an unknown, duplicate or missing key refuses the whole file.
//! The RPC host is fixed at 127.0.0.1 and has no key.
//!
//! Floor format: the line `# floor 1`, then one IP per line in canonical order (IPv4 first, then numeric), as std prints it.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::addr::{self, AddrClass, Cidr};
use crate::fsutil::{self, FsError, StateDir};

/// Name of the config file inside the state dir.
pub const CONFIG_FILE: &str = "config";

/// Name of the floor file inside the state dir.
pub const FLOOR_FILE: &str = "floor";

/// First line of a floor file of this format.
pub const FLOOR_HEADER: &str = "# floor 1";

/// Mode of the floor file.
pub const FLOOR_MODE: u32 = 0o600;

/// The largest input `capture-floor` reads, 16 MiB; a larger file is refused rather than cut, since a cut file could drop a peer.
pub const MAX_CAPTURE_INPUT: u64 = 16_777_216;

const KEY_VERSION: &str = "CONFIG_VERSION";
const KEY_RPC_PORT: &str = "RPC_PORT";
const KEY_P2P_PORTS: &str = "P2P_PORTS";
const KEY_ALLOW_FROM: &str = "P2P_ALLOW_FROM";

/// The settings of one run, read from the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Port of the node's JSON-RPC server on 127.0.0.1, 1 to 65535.
    pub rpc_port: u16,
    /// The P2P ports the table guards, at least one, each 1 to 65535.
    pub p2p_ports: BTreeSet<u16>,
    /// IPv4 entries of `P2P_ALLOW_FROM`: hosts as /32 and networks masked to their network address.
    ///
    /// A host was passed through [`addr::canonical_ip`] and a network through [`Cidr::unmapped`], so an IPv4-mapped IPv6 entry is here as IPv4; [`Cidr::is_host`] tells hosts from networks.
    /// No network here lies inside another one: see [`Config::covered_dropped`].
    pub static_v4: BTreeSet<Cidr>,
    /// IPv6 entries of `P2P_ALLOW_FROM`: hosts as /128 and networks masked to their network address.
    pub static_v6: BTreeSet<Cidr>,
    /// Whether `P2P_ALLOW_FROM` holds `0.0.0.0/0` or `::/0`, which admits every source; accepted, and the sync warns.
    pub catch_all: bool,
    /// How many networks of `P2P_ALLOW_FROM` were dropped because a wider network of the list covers them; the sync warns when it is not 0.
    ///
    /// nft refuses overlapping intervals in one set, and dropping a covered network never shrinks what the list admits.
    /// Hosts are never dropped: they go to the allow sets, not the interval sets.
    pub covered_dropped: usize,
}

/// Why the config or the floor cannot be used; the whole run is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The config file is missing, unreadable or breaks the grammar; token `config-invalid`.
    Config {
        /// The config file.
        path: PathBuf,
        /// The 1-based line at fault, when one is.
        line: Option<usize>,
        /// What is wrong.
        message: String,
    },
    /// The floor file is unreadable or malformed, or an input of `capture-floor` cannot be read; token `floor-invalid`.
    Floor {
        /// The floor file or the `capture-floor` input.
        path: PathBuf,
        /// The 1-based line at fault, when one is.
        line: Option<usize>,
        /// What is wrong.
        message: String,
    },
}

impl ConfigError {
    /// The reason token: `config-invalid` or `floor-invalid`.
    pub fn token(&self) -> &'static str {
        match self {
            Self::Config { .. } => "config-invalid",
            Self::Floor { .. } => "floor-invalid",
        }
    }

    /// The 1-based line at fault, when the error is about one line.
    pub fn line(&self) -> Option<usize> {
        match self {
            Self::Config { line, .. } | Self::Floor { line, .. } => *line,
        }
    }

    fn config(path: &Path, line: Option<usize>, message: String) -> Self {
        Self::Config {
            path: path.to_owned(),
            line,
            message,
        }
    }

    fn floor(path: &Path, line: Option<usize>, message: String) -> Self {
        Self::Floor {
            path: path.to_owned(),
            line,
            message,
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Self::Config {
            path,
            line,
            message,
        }
        | Self::Floor {
            path,
            line,
            message,
        }) = self;
        // the path prints through `Debug`, so a control character cannot break the log line
        match line {
            Some(line) => write!(f, "{path:?} line {line}: {message}"),
            None => write!(f, "{path:?}: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Reads the config file at `path`, never following a symlink, and checks it against the version 1 grammar.
///
/// Every refusal is [`ConfigError::Config`] and names the line at fault when there is one.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let bytes = match fsutil::read_nofollow(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Err(ConfigError::config(path, None, "does not exist".to_owned())),
        Err(e) => return Err(ConfigError::config(path, None, format!("cannot read: {e}"))),
    };
    parse_config(&bytes).map_err(|(line, message)| ConfigError::config(path, line, message))
}

/// One `KEY=VALUE` line of the config.
struct Entry<'a> {
    line: usize,
    key: &'a str,
    value: &'a str,
}

/// Checks the bytes of a config file; an error is the line at fault, if any, and the message.
fn parse_config(bytes: &[u8]) -> Result<Config, (Option<usize>, String)> {
    let mut entries = Vec::new();
    for (line, text) in numbered_lines(bytes) {
        if text.is_empty() || text.starts_with(b"#") {
            continue;
        }
        if let Some(bad) = text.iter().find(|b| !b.is_ascii_graphic()) {
            return Err((
                Some(line),
                format!(
                    "byte {bad:#04x} is not allowed: no spaces, tabs, control or non-ASCII characters"
                ),
            ));
        }
        // every byte is printable ASCII now, so the text is valid UTF-8
        let text = std::str::from_utf8(text).map_err(|e| (Some(line), e.to_string()))?;
        if text.contains(['"', '\'']) {
            return Err((Some(line), "quotes are not allowed".to_owned()));
        }
        let Some((key, value)) = text.split_once('=') else {
            return Err((Some(line), format!("{text:?} is not KEY=VALUE")));
        };
        entries.push(Entry { line, key, value });
    }

    // the version goes first: a newer file may hold keys this build does not know
    let version = entries
        .iter()
        .find(|entry| entry.key == KEY_VERSION)
        .ok_or_else(|| (None, format!("{KEY_VERSION} is missing")))?;
    if version.value != crate::cli::CONFIG_VERSION.to_string() {
        return Err((
            Some(version.line),
            format!(
                "{KEY_VERSION}={:?} is not supported; this build reads {}",
                version.value,
                crate::cli::CONFIG_VERSION
            ),
        ));
    }

    let mut seen: Vec<(&str, usize)> = Vec::new();
    let mut rpc_port = None;
    let mut p2p_ports = None;
    let mut allow_from = None;
    for entry in &entries {
        if let Some((_, first)) = seen.iter().find(|(key, _)| *key == entry.key) {
            return Err((
                Some(entry.line),
                format!("{} is set again (first on line {first})", entry.key),
            ));
        }
        let at = |message: String| (Some(entry.line), message);
        match entry.key {
            KEY_VERSION => {}
            KEY_RPC_PORT => {
                rpc_port = Some(parse_port(entry.value).ok_or_else(|| {
                    at(format!(
                        "{KEY_RPC_PORT}={:?} is not a port 1 to 65535",
                        entry.value
                    ))
                })?);
            }
            KEY_P2P_PORTS => p2p_ports = Some(parse_ports(entry.value).map_err(at)?),
            KEY_ALLOW_FROM => allow_from = Some(parse_allow_from(entry.value).map_err(at)?),
            other => return Err(at(format!("{other:?} is not a known key"))),
        }
        seen.push((entry.key, entry.line));
    }

    let missing = |key: &str| (None, format!("{key} is missing"));
    let rpc_port = rpc_port.ok_or_else(|| missing(KEY_RPC_PORT))?;
    let p2p_ports = p2p_ports.ok_or_else(|| missing(KEY_P2P_PORTS))?;
    let (mut static_v4, mut static_v6) = allow_from.ok_or_else(|| missing(KEY_ALLOW_FROM))?;
    let catch_all = static_v4
        .iter()
        .chain(&static_v6)
        .any(|cidr| cidr.prefix == 0);
    let covered_dropped = drop_covered(&mut static_v4).saturating_add(drop_covered(&mut static_v6));
    Ok(Config {
        rpc_port,
        p2p_ports,
        static_v4,
        static_v6,
        catch_all,
        covered_dropped,
    })
}

/// Removes every network of `set` that a wider network of `set` covers and returns how many went; hosts stay.
///
/// The set holds one family, ordered by network then prefix, and two networks either nest or are disjoint.
/// So a covering network sorts before everything it covers, and comparing each network with the last one kept is enough.
fn drop_covered(set: &mut BTreeSet<Cidr>) -> usize {
    let mut kept: Option<Cidr> = None;
    let mut covered = Vec::new();
    for cidr in set.iter().filter(|cidr| !cidr.is_host()) {
        match kept {
            Some(wide) if covers(wide, *cidr) => covered.push(*cidr),
            _ => kept = Some(*cidr),
        }
    }
    for cidr in &covered {
        set.remove(cidr);
    }
    covered.len()
}

/// Whether the network `wide` contains every address of `narrow`.
fn covers(wide: Cidr, narrow: Cidr) -> bool {
    wide.prefix <= narrow.prefix && Cidr::new(narrow.net, wide.prefix) == Some(wide)
}

/// Lines of `bytes` split at `\n`, numbered from 1; a final newline does not start another line.
fn numbered_lines(bytes: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    // an empty file has no lines, not one empty line
    let lines = (!bytes.is_empty()).then(|| body.split(|b| *b == b'\n'));
    (1..).zip(lines.into_iter().flatten())
}

/// A port: decimal digits without a sign or a leading zero, 1 to 65535.
fn parse_port(text: &str) -> Option<u16> {
    // refusing a leading zero refuses 0 as well, and an empty text fails the parse
    let plain = text.bytes().all(|b| b.is_ascii_digit()) && !text.starts_with('0');
    plain.then(|| text.parse::<u16>().ok()).flatten()
}

/// The value of `P2P_PORTS`: one or more ports separated by commas, each once.
fn parse_ports(value: &str) -> Result<BTreeSet<u16>, String> {
    if value.is_empty() {
        // the program would carry `udp dport {  }`, which nft refuses
        return Err(format!(
            "{KEY_P2P_PORTS} is empty; at least one port is needed"
        ));
    }
    let mut ports = BTreeSet::new();
    for text in value.split(',') {
        let port = parse_port(text)
            .ok_or_else(|| format!("{KEY_P2P_PORTS} entry {text:?} is not a port 1 to 65535"))?;
        if !ports.insert(port) {
            return Err(format!("{KEY_P2P_PORTS} lists port {port} twice"));
        }
    }
    Ok(ports)
}

/// The value of `P2P_ALLOW_FROM`, split by family: empty, or addresses and networks separated by commas.
fn parse_allow_from(value: &str) -> Result<(BTreeSet<Cidr>, BTreeSet<Cidr>), String> {
    let mut v4 = BTreeSet::new();
    let mut v6 = BTreeSet::new();
    if value.is_empty() {
        return Ok((v4, v6));
    }
    for text in value.split(',') {
        let entry =
            addr::parse_cidr_or_host(text).map_err(|e| format!("{KEY_ALLOW_FROM} entry {e}"))?;
        let entry = if entry.is_host() {
            // a mapped host only matches as IPv4, and a dropped one could never match at all
            let ip = addr::canonical_ip(entry.net).ok_or_else(|| {
                format!(
                    "{KEY_ALLOW_FROM} entry {text:?} is an unspecified, multicast or link-local address, never a peer's source"
                )
            })?;
            host(ip)
        } else {
            // a mapped network, like a mapped host, only matches as the IPv4 network it names
            entry.unmapped()
        };
        let family = match entry.net {
            IpAddr::V4(_) => &mut v4,
            IpAddr::V6(_) => &mut v6,
        };
        family.insert(entry);
    }
    Ok((v4, v6))
}

/// `ip` as a host entry: /32 or /128.
fn host(ip: IpAddr) -> Cidr {
    let prefix = match ip {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    Cidr { net: ip, prefix }
}

/// Reads the floor file at `path`, never following a symlink.
///
/// A missing file is an empty floor.
/// Anything but the [`FLOOR_HEADER`] line followed by canonical IPs in strictly increasing order, each written as std prints it, is [`ConfigError::Floor`].
pub fn load_floor(path: &Path) -> Result<BTreeSet<IpAddr>, ConfigError> {
    let bytes = match fsutil::read_nofollow(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(BTreeSet::new()),
        Err(e) => return Err(ConfigError::floor(path, None, format!("cannot read: {e}"))),
    };
    parse_floor(&bytes).map_err(|(line, message)| ConfigError::floor(path, line, message))
}

/// Checks the bytes of a floor file; an error is the line at fault, if any, and the message.
fn parse_floor(bytes: &[u8]) -> Result<BTreeSet<IpAddr>, (Option<usize>, String)> {
    let mut lines = numbered_lines(bytes);
    match lines.next() {
        Some((_, header)) if header == FLOOR_HEADER.as_bytes() => {}
        Some((line, _)) => {
            return Err((
                Some(line),
                format!("the first line is not {FLOOR_HEADER:?}"),
            ));
        }
        None => {
            return Err((
                None,
                format!("is empty; the first line must be {FLOOR_HEADER:?}"),
            ));
        }
    }
    let mut floor = BTreeSet::new();
    let mut last: Option<IpAddr> = None;
    for (line, text) in lines {
        let ip = std::str::from_utf8(text)
            .ok()
            .and_then(|text| text.parse::<IpAddr>().ok())
            .filter(|ip| addr::canonical_ip(*ip) == Some(*ip) && ip.to_string().as_bytes() == text)
            .ok_or_else(|| {
                (
                    Some(line),
                    format!(
                        "{:?} is not a canonical IP address",
                        String::from_utf8_lossy(text)
                    ),
                )
            })?;
        if last.is_some_and(|last| last >= ip) {
            return Err((Some(line), format!("{ip} is out of order or repeated")));
        }
        last = Some(ip);
        floor.insert(ip);
    }
    Ok(floor)
}

/// The text of a floor file for `floor`: the header, then one IP per line in the set's order.
pub fn render_floor(floor: &BTreeSet<IpAddr>) -> String {
    let mut text = format!("{FLOOR_HEADER}\n");
    for ip in floor {
        text.push_str(&ip.to_string());
        text.push('\n');
    }
    text
}

/// Writes `floor` to the floor file of `dir`, atomically and with mode 0600; the caller holds the lock.
pub fn write_floor(dir: &StateDir, floor: &BTreeSet<IpAddr>) -> Result<(), FsError> {
    dir.write_atomic(FLOOR_FILE, render_floor(floor).as_bytes(), FLOOR_MODE)
}

/// The floor found in `files`: every IP literal that leads a multiaddr in them, canonicalised.
///
/// No YAML or JSON parser is involved.
/// Each file is split into runs of the characters a multiaddr's text form uses (letters, digits and `/.:-_%`), and each run goes through [`addr::classify_multiaddr`], the parser the snapshot's multiaddrs go through.
/// So `/ip4/A/...` and `/ip6/A/...` give `A` after [`addr::canonical_ip`]; `/dns4/...` and the rest of the file give nothing.
/// The bootstrap peers file and the genesis `committee.yaml` both work, in YAML or JSON.
///
/// A file that cannot be read, is not a regular file or is larger than [`MAX_CAPTURE_INPUT`] is [`ConfigError::Floor`]: dropping it would silently cut the floor.
/// Inputs are opened as given, symlinks included; they live outside the state dir.
pub fn capture_floor(files: &[PathBuf]) -> Result<BTreeSet<IpAddr>, ConfigError> {
    let mut floor = BTreeSet::new();
    for path in files {
        let bytes = read_capture_input(path)
            .map_err(|e| ConfigError::floor(path, None, format!("cannot read: {e}")))?;
        floor.extend(scan_ips(&bytes));
    }
    Ok(floor)
}

/// The canonical IPs leading the multiaddr tokens of `bytes`.
fn scan_ips(bytes: &[u8]) -> impl Iterator<Item = IpAddr> {
    bytes
        .split(|b| !is_multiaddr_byte(*b))
        .filter_map(|token| std::str::from_utf8(token).ok())
        .filter_map(|token| match addr::classify_multiaddr(token) {
            AddrClass::Ip(ip) => addr::canonical_ip(ip),
            AddrClass::Dns | AddrClass::Unparsed => None,
        })
}

/// Whether `b` can be part of a multiaddr's text form: the protocol names, addresses, ports and peer ids.
const fn is_multiaddr_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b':' | b'-' | b'_' | b'%')
}

/// Reads one `capture-floor` input: a regular file, symlinks followed, at most [`MAX_CAPTURE_INPUT`] bytes.
///
/// [`fsutil::read_bounded`] checks the type before the open and again on the file it opens non-blocking, so a special file is never opened and a FIFO swapped in for the file is refused at once instead of blocking the run.
fn read_capture_input(path: &Path) -> io::Result<Vec<u8>> {
    let bytes = fsutil::read_bounded(path, MAX_CAPTURE_INPUT)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CAPTURE_INPUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("is larger than {MAX_CAPTURE_INPUT} bytes"),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    // tests build fixtures with std::fs directly
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use crate::support::TempDir;
    use std::error::Error;
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    type TestResult = Result<(), Box<dyn Error>>;

    const EXAMPLE: &str = "\
# tn-p2p-allow-sync config, written by firewall-setup.sh
CONFIG_VERSION=1
RPC_PORT=8545
P2P_PORTS=49590,49594
P2P_ALLOW_FROM=203.0.113.0/24,198.51.100.9
";

    fn cidr(s: &str) -> Result<Cidr, Box<dyn Error>> {
        Ok(addr::parse_cidr_or_host(s)?)
    }

    fn cidrs(entries: &[&str]) -> Result<BTreeSet<Cidr>, Box<dyn Error>> {
        entries.iter().map(|s| cidr(s)).collect()
    }

    fn ip(s: &str) -> Result<IpAddr, Box<dyn Error>> {
        Ok(s.parse::<IpAddr>()?)
    }

    fn ips(entries: &[&str]) -> Result<BTreeSet<IpAddr>, Box<dyn Error>> {
        entries.iter().map(|s| ip(s)).collect()
    }

    /// A config with the three other keys fixed and `P2P_ALLOW_FROM` as given.
    fn with_allow_from(value: &str) -> Result<Config, Box<dyn Error>> {
        let text =
            format!("CONFIG_VERSION=1\nRPC_PORT=8545\nP2P_PORTS=49590\nP2P_ALLOW_FROM={value}\n");
        parse_config(text.as_bytes())
            .map_err(|(line, message)| format!("{line:?}: {message}").into())
    }

    fn refusal(text: &str) -> (Option<usize>, String) {
        parse_config(text.as_bytes())
            .map_or_else(|e| e, |config| (None, format!("accepted: {config:?}")))
    }

    /// Makes a FIFO at `path`, or returns `false` when `mkfifo` is not on PATH and the test should skip.
    fn mkfifo(path: &Path) -> Result<bool, Box<dyn Error>> {
        // std makes no FIFO without libc and the tests take no dependency, so the coreutils tool makes it
        #[allow(clippy::disallowed_methods)]
        let status = match std::process::Command::new("mkfifo").arg(path).status() {
            Ok(status) => status,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if !status.success() {
            return Err(format!("mkfifo {path:?}: {status}").into());
        }
        Ok(true)
    }

    /// Runs `f` on a thread of its own and waits for it at most 10 s, so a call that blocks fails the test instead of hanging the suite.
    fn without_blocking<T, F>(f: F) -> Result<T, Box<dyn Error>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (done, result) = mpsc::channel();
        thread::spawn(move || done.send(f()));
        Ok(result
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| format!("the call blocked: {e}"))?)
    }

    /// Writes a file of exactly `len` bytes at `path`: a hole, then `tail` as its last bytes.
    fn sparse_file(path: &Path, len: u64, tail: &[u8]) -> TestResult {
        let start = len
            .checked_sub(u64::try_from(tail.len())?)
            .ok_or("the tail is longer than the file")?;
        let mut file = fs::File::create(path)?;
        file.set_len(len)?;
        file.seek(SeekFrom::Start(start))?;
        file.write_all(tail)?;
        assert_eq!(file.metadata()?.len(), len);
        Ok(())
    }

    #[test]
    fn the_plan_example_parses() -> TestResult {
        let config = parse_config(EXAMPLE.as_bytes()).map_err(|(_, m)| m)?;
        assert_eq!(
            config,
            Config {
                rpc_port: 8545,
                p2p_ports: BTreeSet::from([49590, 49594]),
                static_v4: cidrs(&["198.51.100.9/32", "203.0.113.0/24"])?,
                static_v6: BTreeSet::new(),
                catch_all: false,
                covered_dropped: 0,
            }
        );
        Ok(())
    }

    #[test]
    fn comments_blank_lines_and_any_key_order_are_accepted() -> TestResult {
        let text = b"# a comment with spaces, \"quotes\" and \xff bytes\n\nP2P_ALLOW_FROM=\n#\nP2P_PORTS=49594,49590\n\nRPC_PORT=65535\nCONFIG_VERSION=1";
        let config = parse_config(text).map_err(|(_, m)| m)?;
        assert_eq!(config.rpc_port, 65535);
        assert_eq!(config.p2p_ports, BTreeSet::from([49590, 49594]));
        assert!(config.static_v4.is_empty() && config.static_v6.is_empty());
        assert!(!config.catch_all);
        Ok(())
    }

    #[test]
    fn every_refusal_names_its_line() {
        let base = "CONFIG_VERSION=1\nRPC_PORT=8545\nP2P_PORTS=49590\nP2P_ALLOW_FROM=\n";
        for (text, line, message) in [
            (
                format!("{base}EXTRA=1\n"),
                5,
                "\"EXTRA\" is not a known key",
            ),
            (
                format!("{base}RPC_PORT=8546\n"),
                5,
                "RPC_PORT is set again (first on line 2)",
            ),
            (
                format!("{base}CONFIG_VERSION=1\n"),
                5,
                "CONFIG_VERSION is set again (first on line 1)",
            ),
            (
                format!("{base}rpc_port=8545\n"),
                5,
                "\"rpc_port\" is not a known key",
            ),
            (format!("{base}=8545\n"), 5, "\"\" is not a known key"),
            (
                format!("{base}RPC_PORT\n"),
                5,
                "\"RPC_PORT\" is not KEY=VALUE",
            ),
            (
                format!("{base}RPC_PORT=\"8545\"\n"),
                5,
                "quotes are not allowed",
            ),
            (
                format!("{base}RPC_PORT='8545'\n"),
                5,
                "quotes are not allowed",
            ),
            (
                format!("{base}RPC_PORT = 8545\n"),
                5,
                "byte 0x20 is not allowed",
            ),
            (
                format!("{base}RPC_PORT=8545 \n"),
                5,
                "byte 0x20 is not allowed",
            ),
            (
                format!("{base}RPC_PORT=\t8545\n"),
                5,
                "byte 0x09 is not allowed",
            ),
            (
                format!("{base} # indented comment\n"),
                5,
                "byte 0x20 is not allowed",
            ),
            (format!("{base} \n"), 5, "byte 0x20 is not allowed"),
            (
                "CONFIG_VERSION=1\r\nRPC_PORT=8545\r\n".to_owned(),
                1,
                "byte 0x0d is not allowed",
            ),
            (
                "CONFIG_VERSION=1\nRPC_PORT=85\u{e9}45\n".to_owned(),
                2,
                "byte 0xc3 is not allowed",
            ),
            (
                "# header\nCONFIG_VERSION=2\nNEW_KEY=1\n".to_owned(),
                2,
                "CONFIG_VERSION=\"2\" is not supported; this build reads 1",
            ),
            (
                "CONFIG_VERSION=01\n".to_owned(),
                1,
                "CONFIG_VERSION=\"01\" is not supported",
            ),
            (
                "CONFIG_VERSION=\n".to_owned(),
                1,
                "CONFIG_VERSION=\"\" is not supported",
            ),
        ] {
            let (got_line, got) = refusal(&text);
            assert_eq!(got_line, Some(line), "{text:?}: {got}");
            assert!(got.starts_with(message), "{text:?}: {got}");
        }
    }

    #[test]
    fn missing_keys_are_refused() {
        for (text, message) in [
            ("", "CONFIG_VERSION is missing"),
            ("# only a comment\n", "CONFIG_VERSION is missing"),
            (
                "RPC_PORT=8545\nP2P_PORTS=49590\nP2P_ALLOW_FROM=\n",
                "CONFIG_VERSION is missing",
            ),
            (
                "CONFIG_VERSION=1\nP2P_PORTS=49590\nP2P_ALLOW_FROM=\n",
                "RPC_PORT is missing",
            ),
            (
                "CONFIG_VERSION=1\nRPC_PORT=8545\nP2P_ALLOW_FROM=\n",
                "P2P_PORTS is missing",
            ),
            (
                "CONFIG_VERSION=1\nRPC_PORT=8545\nP2P_PORTS=49590\n",
                "P2P_ALLOW_FROM is missing",
            ),
        ] {
            assert_eq!(refusal(text), (None, message.to_owned()), "{text:?}");
        }
    }

    #[test]
    fn ports_are_plain_decimal_1_to_65535() {
        for value in [
            "0",
            "65536",
            "08545",
            "+8545",
            "-1",
            "",
            "85a45",
            "8545,49590",
            "0x2161",
        ] {
            let text =
                format!("CONFIG_VERSION=1\nRPC_PORT={value}\nP2P_PORTS=49590\nP2P_ALLOW_FROM=\n");
            let (line, message) = refusal(&text);
            assert_eq!(line, Some(2), "{value:?}: {message}");
            assert_eq!(
                message,
                format!("RPC_PORT={value:?} is not a port 1 to 65535")
            );
        }
        for (value, message) in [
            ("", "P2P_PORTS is empty; at least one port is needed"),
            ("49590,49590", "P2P_PORTS lists port 49590 twice"),
            ("49590,49594,49590", "P2P_PORTS lists port 49590 twice"),
            (
                "49590,,49594",
                "P2P_PORTS entry \"\" is not a port 1 to 65535",
            ),
            ("49590,", "P2P_PORTS entry \"\" is not a port 1 to 65535"),
            (",49590", "P2P_PORTS entry \"\" is not a port 1 to 65535"),
            ("0", "P2P_PORTS entry \"0\" is not a port 1 to 65535"),
            (
                "70000",
                "P2P_PORTS entry \"70000\" is not a port 1 to 65535",
            ),
            (
                "49590;49594",
                "P2P_PORTS entry \"49590;49594\" is not a port 1 to 65535",
            ),
        ] {
            let text =
                format!("CONFIG_VERSION=1\nRPC_PORT=8545\nP2P_PORTS={value}\nP2P_ALLOW_FROM=\n");
            assert_eq!(refusal(&text), (Some(3), message.to_owned()), "{value:?}");
        }
    }

    #[test]
    fn a_bad_allow_from_entry_refuses_the_file() {
        for (value, message) in [
            (
                "node.example",
                "P2P_ALLOW_FROM entry \"node.example\" is not an IP address or a CIDR (hostnames are not resolved)",
            ),
            (
                "203.0.113.0/24,localhost",
                "P2P_ALLOW_FROM entry \"localhost\" is not an IP address or a CIDR (hostnames are not resolved)",
            ),
            (
                "203.0.113.0/24,",
                "P2P_ALLOW_FROM entry \"\" is not an IP address or a CIDR (hostnames are not resolved)",
            ),
            (
                "203.0.113.0/33",
                "P2P_ALLOW_FROM entry \"203.0.113.0/33\" has a prefix length longer than 32",
            ),
            (
                "203.0.113.0/024",
                "P2P_ALLOW_FROM entry \"203.0.113.0/024\" has a prefix length that is not a plain decimal number",
            ),
            (
                "0.0.0.0",
                "P2P_ALLOW_FROM entry \"0.0.0.0\" is an unspecified, multicast or link-local address, never a peer's source",
            ),
            (
                "224.0.0.1",
                "P2P_ALLOW_FROM entry \"224.0.0.1\" is an unspecified, multicast or link-local address, never a peer's source",
            ),
            (
                "169.254.1.1/32",
                "P2P_ALLOW_FROM entry \"169.254.1.1/32\" is an unspecified, multicast or link-local address, never a peer's source",
            ),
            (
                "::",
                "P2P_ALLOW_FROM entry \"::\" is an unspecified, multicast or link-local address, never a peer's source",
            ),
            (
                "fe80::1",
                "P2P_ALLOW_FROM entry \"fe80::1\" is an unspecified, multicast or link-local address, never a peer's source",
            ),
            (
                "::ffff:169.254.1.1",
                "P2P_ALLOW_FROM entry \"::ffff:169.254.1.1\" is an unspecified, multicast or link-local address, never a peer's source",
            ),
            (
                "10.0.0.0/8,::ffff:10.0.0.0/95",
                "P2P_ALLOW_FROM entry \"::ffff:10.0.0.0/95\" is an IPv4-mapped address with a prefix length shorter than 96",
            ),
            (
                "::ffff:0.0.0.0/0",
                "P2P_ALLOW_FROM entry \"::ffff:0.0.0.0/0\" is an IPv4-mapped address with a prefix length shorter than 96",
            ),
        ] {
            let text = format!(
                "CONFIG_VERSION=1\nRPC_PORT=8545\nP2P_PORTS=49590\nP2P_ALLOW_FROM={value}\n"
            );
            assert_eq!(refusal(&text), (Some(4), message.to_owned()), "{value:?}");
        }
    }

    #[test]
    fn allow_from_entries_are_masked_canonicalised_and_split() -> TestResult {
        let config = with_allow_from(
            "203.0.113.77/24,198.51.100.9,::ffff:198.51.100.10,2001:db8::1/64,2001:db8:1::7,::ffff:192.0.2.0/120,198.51.100.9/32,203.0.113.0/24,224.0.0.0/4",
        )?;
        assert_eq!(
            config.static_v4,
            cidrs(&[
                "192.0.2.0/24",
                "198.51.100.9",
                "198.51.100.10",
                "203.0.113.0/24",
                "224.0.0.0/4"
            ])?
        );
        assert_eq!(
            config.static_v6,
            cidrs(&["2001:db8::/64", "2001:db8:1::7"])?
        );
        assert!(config.static_v4.iter().filter(|c| c.is_host()).count() == 2);
        assert!(!config.catch_all);
        assert_eq!(config.covered_dropped, 0);
        Ok(())
    }

    #[test]
    fn catch_all_entries_are_flagged() -> TestResult {
        assert!(with_allow_from("0.0.0.0/0")?.catch_all);
        assert!(with_allow_from("::/0")?.catch_all);
        assert!(with_allow_from("198.51.100.9,10.0.0.0/8,::/0")?.catch_all);
        assert!(!with_allow_from("0.0.0.0/1,128.0.0.0/1,::/1")?.catch_all);
        let config = with_allow_from("10.0.0.0/8,0.0.0.0/0,192.0.2.0/24,198.51.100.9")?;
        assert_eq!(config.static_v4, cidrs(&["0.0.0.0/0", "198.51.100.9"])?);
        assert_eq!(config.covered_dropped, 2);
        Ok(())
    }

    #[test]
    fn covered_networks_are_dropped_and_counted() -> TestResult {
        let config = with_allow_from("10.3.0.0/16,10.0.0.0/8")?;
        assert_eq!(config.static_v4, cidrs(&["10.0.0.0/8"])?);
        assert_eq!(config.covered_dropped, 1);

        // only network against network: a host goes to the allow set, never into an interval set
        let config = with_allow_from("203.0.113.9,203.0.113.0/24")?;
        assert_eq!(config.static_v4, cidrs(&["203.0.113.0/24", "203.0.113.9"])?);
        assert_eq!(config.covered_dropped, 0);

        let config = with_allow_from("2001:db8:1::/48,2001:db8::/32,2001:db8:1::7")?;
        assert_eq!(
            config.static_v6,
            cidrs(&["2001:db8::/32", "2001:db8:1::7"])?
        );
        assert_eq!(config.covered_dropped, 1);

        let config = with_allow_from(
            "10.0.0.0/16,10.3.4.0/24,11.0.0.0/8,10.3.0.0/16,10.0.0.0/8,12.0.0.0/16,12.1.0.0/16,2001:db8::/48,2001:db8::/32",
        )?;
        assert_eq!(
            config.static_v4,
            cidrs(&["10.0.0.0/8", "11.0.0.0/8", "12.0.0.0/16", "12.1.0.0/16"])?
        );
        assert_eq!(config.static_v6, cidrs(&["2001:db8::/32"])?);
        assert_eq!(config.covered_dropped, 4);

        // a mapped network is the IPv4 network it names, so it covers and is covered as one
        let config = with_allow_from("::ffff:10.0.0.0/104,10.3.0.0/16")?;
        assert_eq!(config.static_v4, cidrs(&["10.0.0.0/8"])?);
        assert!(config.static_v6.is_empty());
        assert_eq!(config.covered_dropped, 1);

        let config = with_allow_from("10.0.0.0/8,::ffff:10.3.0.0/112")?;
        assert_eq!(config.static_v4, cidrs(&["10.0.0.0/8"])?);
        assert!(config.static_v6.is_empty());
        assert_eq!(config.covered_dropped, 1);

        // the same network in both spellings is one entry, deduplicated as any repeated network is, not covered
        let config = with_allow_from("10.0.0.0/8,::ffff:10.0.0.0/104")?;
        assert_eq!(config.static_v4, cidrs(&["10.0.0.0/8"])?);
        assert!(config.static_v6.is_empty());
        assert_eq!(config.covered_dropped, 0);
        Ok(())
    }

    #[test]
    fn mapped_networks_become_ipv4_networks() -> TestResult {
        let config = with_allow_from("::ffff:10.0.0.0/104")?;
        assert_eq!(config.static_v4, cidrs(&["10.0.0.0/8"])?);
        assert!(config.static_v6.is_empty());
        assert!(!config.catch_all);

        // masked as usual
        let config = with_allow_from("::ffff:10.1.2.3/104,::ffff:192.0.2.77/127")?;
        assert_eq!(config.static_v4, cidrs(&["10.0.0.0/8", "192.0.2.76/31"])?);

        // the whole mapped range is every IPv4 source
        let config = with_allow_from("::ffff:0.0.0.0/96")?;
        assert_eq!(config.static_v4, cidrs(&["0.0.0.0/0"])?);
        assert!(config.static_v6.is_empty());
        assert!(config.catch_all);

        // a /128 is a host: it goes through the host canonicalisation and on to the allow set
        let config = with_allow_from("::ffff:198.51.100.9/128,::ffff:198.51.100.10")?;
        assert_eq!(config.static_v4, cidrs(&["198.51.100.9", "198.51.100.10"])?);
        assert!(config.static_v4.iter().all(Cidr::is_host));
        assert!(config.static_v6.is_empty());

        // only the mapped form is converted: a wider IPv6 network around the mapped range stays IPv6
        // and never covers the IPv4 network a mapped entry gives
        let config = with_allow_from("::/80,::ffff:10.0.0.0/104")?;
        assert_eq!(config.static_v4, cidrs(&["10.0.0.0/8"])?);
        assert_eq!(config.static_v6, cidrs(&["::/80"])?);
        assert_eq!(config.covered_dropped, 0);
        let config = with_allow_from("::fffe:0:0/95")?;
        assert!(config.static_v4.is_empty());
        assert_eq!(config.static_v6, cidrs(&["::fffe:0:0/95"])?);
        Ok(())
    }

    #[test]
    fn load_config_reads_the_file_and_names_it() -> TestResult {
        let temp = TempDir::new("config")?;
        let path = temp.join(CONFIG_FILE);
        let missing = load_config(&path);
        assert_eq!(
            missing.as_ref().map_err(ConfigError::token).err(),
            Some("config-invalid")
        );
        assert_eq!(
            missing.as_ref().map_err(ConfigError::line).err(),
            Some(None)
        );
        assert_eq!(
            missing.map_err(|e| e.to_string()).err(),
            Some(format!("{path:?}: does not exist"))
        );

        fs::write(&path, EXAMPLE)?;
        assert_eq!(load_config(&path)?.rpc_port, 8545);

        fs::write(&path, format!("{EXAMPLE}P2P_PORTS=1\n"))?;
        let duplicate = load_config(&path).map_err(|e| (e.token(), e.line(), e.to_string()));
        assert_eq!(
            duplicate.err(),
            Some((
                "config-invalid",
                Some(6),
                format!("{path:?} line 6: P2P_PORTS is set again (first on line 4)")
            ))
        );
        Ok(())
    }

    #[test]
    fn load_config_never_follows_a_symlink() -> TestResult {
        let temp = TempDir::new("config-link")?;
        fs::write(temp.join("real"), EXAMPLE)?;
        symlink(temp.join("real"), temp.join(CONFIG_FILE))?;
        let result = load_config(&temp.join(CONFIG_FILE)).map_err(|e| e.to_string());
        assert_eq!(
            result.err(),
            Some(format!(
                "{:?}: cannot read: is a symlink",
                temp.join(CONFIG_FILE)
            ))
        );
        Ok(())
    }

    #[test]
    fn a_fifo_or_a_socket_at_config_or_floor_is_refused_without_blocking() -> TestResult {
        let temp = TempDir::new("special-files")?;
        let socket = temp.join("socket");
        let _listener = UnixListener::bind(&socket)?;
        let mut specials = vec![socket];
        let fifo = temp.join("fifo");
        if mkfifo(&fifo)? {
            specials.push(fifo);
        } else {
            println!("FIFO case skipped: mkfifo is not on PATH");
        }
        for path in specials {
            let at = path.clone();
            let config = without_blocking(move || load_config(&at))?;
            assert_eq!(
                config.map_err(|e| (e.token(), e.to_string())).err(),
                Some((
                    "config-invalid",
                    format!("{path:?}: cannot read: is not a regular file")
                ))
            );
            let at = path.clone();
            let floor = without_blocking(move || load_floor(&at))?;
            assert_eq!(
                floor.map_err(|e| (e.token(), e.to_string())).err(),
                Some((
                    "floor-invalid",
                    format!("{path:?}: cannot read: is not a regular file")
                ))
            );
        }
        Ok(())
    }

    #[test]
    fn floor_round_trips_through_the_state_dir() -> TestResult {
        let temp = TempDir::new("floor")?;
        let dir = StateDir::open(temp.path())?;
        let floor = ips(&[
            "2001:db8::7",
            "198.51.100.7",
            "192.0.2.10",
            "::1",
            "10.0.0.1",
        ])?;
        write_floor(&dir, &floor)?;
        assert_eq!(
            fs::read_to_string(temp.join(FLOOR_FILE))?,
            "# floor 1\n10.0.0.1\n192.0.2.10\n198.51.100.7\n::1\n2001:db8::7\n"
        );
        assert_eq!(fs::metadata(temp.join(FLOOR_FILE))?.mode() & 0o777, 0o600);
        assert!(!temp.join(".floor.new").exists());
        assert_eq!(load_floor(&temp.join(FLOOR_FILE))?, floor);

        write_floor(&dir, &BTreeSet::new())?;
        assert_eq!(fs::read_to_string(temp.join(FLOOR_FILE))?, "# floor 1\n");
        assert_eq!(load_floor(&temp.join(FLOOR_FILE))?, BTreeSet::new());
        Ok(())
    }

    #[test]
    fn a_missing_floor_is_empty() -> TestResult {
        let temp = TempDir::new("floor-missing")?;
        assert_eq!(load_floor(&temp.join(FLOOR_FILE))?, BTreeSet::new());
        Ok(())
    }

    #[test]
    fn the_final_newline_of_a_floor_is_optional() -> TestResult {
        assert_eq!(
            parse_floor(b"# floor 1").map_err(|(_, m)| m)?,
            BTreeSet::new()
        );
        assert_eq!(
            parse_floor(b"# floor 1\n192.0.2.10").map_err(|(_, m)| m)?,
            BTreeSet::from([IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))])
        );
        Ok(())
    }

    #[test]
    fn anything_else_is_floor_invalid() -> TestResult {
        for (text, line, message) in [
            (
                &b""[..],
                None,
                "is empty; the first line must be \"# floor 1\"",
            ),
            (b"\n", Some(1), "the first line is not \"# floor 1\""),
            (
                b"# floor 2\n192.0.2.10\n",
                Some(1),
                "the first line is not \"# floor 1\"",
            ),
            (
                b"#floor 1\n",
                Some(1),
                "the first line is not \"# floor 1\"",
            ),
            (
                b"192.0.2.10\n",
                Some(1),
                "the first line is not \"# floor 1\"",
            ),
            (
                b"# floor 1\r\n",
                Some(1),
                "the first line is not \"# floor 1\"",
            ),
            (
                b"# floor 1\n192.0.2.10\n192.0.2.9\n",
                Some(3),
                "192.0.2.9 is out of order or repeated",
            ),
            (
                b"# floor 1\n192.0.2.10\n192.0.2.10\n",
                Some(3),
                "192.0.2.10 is out of order or repeated",
            ),
            (
                b"# floor 1\n2001:db8::7\n192.0.2.10\n",
                Some(3),
                "192.0.2.10 is out of order or repeated",
            ),
            (
                b"# floor 1\n2001:DB8::7\n",
                Some(2),
                "\"2001:DB8::7\" is not a canonical IP address",
            ),
            (
                b"# floor 1\n2001:db8:0:0:0:0:0:7\n",
                Some(2),
                "\"2001:db8:0:0:0:0:0:7\" is not a canonical IP address",
            ),
            (
                b"# floor 1\n::ffff:192.0.2.10\n",
                Some(2),
                "\"::ffff:192.0.2.10\" is not a canonical IP address",
            ),
            (
                b"# floor 1\n169.254.1.1\n",
                Some(2),
                "\"169.254.1.1\" is not a canonical IP address",
            ),
            (
                b"# floor 1\n0.0.0.0\n",
                Some(2),
                "\"0.0.0.0\" is not a canonical IP address",
            ),
            (
                b"# floor 1\n192.0.2.0/24\n",
                Some(2),
                "\"192.0.2.0/24\" is not a canonical IP address",
            ),
            (
                b"# floor 1\nnode.example\n",
                Some(2),
                "\"node.example\" is not a canonical IP address",
            ),
            (
                b"# floor 1\n192.0.2.10 \n",
                Some(2),
                "\"192.0.2.10 \" is not a canonical IP address",
            ),
            (
                b"# floor 1\n192.0.2.10\r\n",
                Some(2),
                "\"192.0.2.10\\r\" is not a canonical IP address",
            ),
            (
                b"# floor 1\n\n192.0.2.10\n",
                Some(2),
                "\"\" is not a canonical IP address",
            ),
            (
                b"# floor 1\n192.0.2.10\n\n",
                Some(3),
                "\"\" is not a canonical IP address",
            ),
            (
                b"# floor 1\n# comment\n",
                Some(2),
                "\"# comment\" is not a canonical IP address",
            ),
            (
                b"# floor 1\n\xff\n",
                Some(2),
                "\"\u{fffd}\" is not a canonical IP address",
            ),
        ] {
            assert_eq!(
                parse_floor(text),
                Err((line, message.to_owned())),
                "{text:?}"
            );
        }

        let temp = TempDir::new("floor-invalid")?;
        let path = temp.join(FLOOR_FILE);
        fs::write(&path, "# floor 1\n192.0.2.10\n192.0.2.9\n")?;
        let error = load_floor(&path).map_err(|e| (e.token(), e.line(), e.to_string()));
        assert_eq!(
            error.err(),
            Some((
                "floor-invalid",
                Some(3),
                format!("{path:?} line 3: 192.0.2.9 is out of order or repeated")
            ))
        );
        fs::remove_file(&path)?;
        symlink(temp.join("elsewhere"), &path)?;
        let error = load_floor(&path).map_err(|e| e.token());
        assert_eq!(error.err(), Some("floor-invalid"));
        Ok(())
    }

    /// The bootstrap peers file as the node writes it: a map of BLS key to primary and worker addresses.
    const BOOTSTRAP_PEERS_YAML: &str = "\
---
pDmpE29YEhr93MPVPCGkWx3BsCcow4oExmxv5viJz2GTjsaXwc3YwKxE1CFdVuTVSudsFDutFSVmtgF98Abs56JZPfGQs6GzbqXDFkGA1eZx2edkwfP2Q6eRLo8coLhvTNj:
  primary:
    network_address: /ip4/35.203.41.59/udp/49590/quic-v1/p2p/12D3KooWCnizjv4ctsBTwwYJxZP3duCuoebJfbk1fUQv6Vg1dVUh
    network_key: 4XTTM47Qeb4XhWEK7ybzj5bGsnVraHKaknpMFPgpg8jHYbGFo
  workers:
    - network_address: /ip4/35.203.41.59/udp/49594/quic-v1/p2p/12D3KooWHGdbYerasxspuhZW4KqNMbbpqvLNCG6LGpS5yx6b8x2x
      network_key: 4XTTM8bKFPoKfVL1UwN1vBMjCWCFVKbKpKUha12qr2Bi86ip4
rZdhyVGJAtz7kyFtFARWVVcw5DB3o9E4GvPN6zuKfUhUBGFRvEK2PgEV44CJt9wu9GMyssFQBFXRknrenuhJixo9KCADrZzHdtcvAwAYDmcLc7Ugh9epGc5LRZC5TK2hnYA:
  primary:
    network_address: \"/ip6/2001:db8:0:0::7/udp/49590/quic-v1/p2p/12D3KooWBRH7XpYhxCPTrw364UnmaSYDR4hCotoTHxTWTjKfrcUm\"
    network_key: 4XTTM2jxmNy1nZZX7tbVWBWgbj3Bstjgew7Qh2AsGVxwCpPFs
  workers:
    - network_address: '/ip6/::ffff:198.51.100.7/udp/49594/quic-v1'
      network_key: 4XTTM2jxmNy1nZZX7tbVWBWgbj3Bstjgew7Qh2AsGVxwCpPFs
24JaKrebALhvhXxsAabjd5ZK22k1W4zKwGepKD5pcobQwBkZyiruYyhRM3hkLap8Ssc4qUdcRUJSrs2389hUoV7G83HKYrmkz2pTzCJiQURV5vtkrh8gFRKBQWKFTWLWah9s:
  primary:
    network_address: /dns4/validator.example/udp/49590/quic-v1/p2p/12D3KooWBRH7XpYhxCPTrw364UnmaSYDR4hCotoTHxTWTjKfrcUm
    network_key: 4XTTM2jxmNy1nZZX7tbVWBWgbj3Bstjgew7Qh2AsGVxwCpPFs
  workers:
    - network_address: /dns4/192.0.2.99/udp/49594/quic-v1
      network_key: 4XTTM2jxmNy1nZZX7tbVWBWgbj3Bstjgew7Qh2AsGVxwCpPFs
    - network_address: /ip4/169.254.10.10/udp/49594/quic-v1
      network_key: 4XTTM2jxmNy1nZZX7tbVWBWgbj3Bstjgew7Qh2AsGVxwCpPFs
";

    /// The genesis committee: authorities, epoch and the bootstrap servers, whose addresses are the only multiaddrs.
    const COMMITTEE_YAML: &str = "\
---
authorities:
  pDmpE29YEhr93MPVPCGkWx3BsCcow4oExmxv5viJz2GTjsaXwc3YwKxE1CFdVuTVSudsFDutFSVmtgF98Abs56JZPfGQs6GzbqXDFkGA1eZx2edkwfP2Q6eRLo8coLhvTNj:
    protocol_key: pDmpE29YEhr93MPVPCGkWx3BsCcow4oExmxv5viJz2GTjsaXwc3YwKxE1CFdVuTVSudsFDutFSVmtgF98Abs56JZPfGQs6GzbqXDFkGA1eZx2edkwfP2Q6eRLo8coLhvTNj
    execution_address: \"0x0033a370616805b1fd275b7ffab83fc41d665ccb\"
  rZdhyVGJAtz7kyFtFARWVVcw5DB3o9E4GvPN6zuKfUhUBGFRvEK2PgEV44CJt9wu9GMyssFQBFXRknrenuhJixo9KCADrZzHdtcvAwAYDmcLc7Ugh9epGc5LRZC5TK2hnYA:
    protocol_key: rZdhyVGJAtz7kyFtFARWVVcw5DB3o9E4GvPN6zuKfUhUBGFRvEK2PgEV44CJt9wu9GMyssFQBFXRknrenuhJixo9KCADrZzHdtcvAwAYDmcLc7Ugh9epGc5LRZC5TK2hnYA
    execution_address: \"0x89dab9f6fdc569c1bcdbd6493f25b7040b55dc79\"
epoch: 0
bootstrap_servers:
  pDmpE29YEhr93MPVPCGkWx3BsCcow4oExmxv5viJz2GTjsaXwc3YwKxE1CFdVuTVSudsFDutFSVmtgF98Abs56JZPfGQs6GzbqXDFkGA1eZx2edkwfP2Q6eRLo8coLhvTNj:
    primary:
      network_address: /ip4/35.203.41.59/udp/49590/quic-v1/p2p/12D3KooWCnizjv4ctsBTwwYJxZP3duCuoebJfbk1fUQv6Vg1dVUh
      network_key: 4XTTM47Qeb4XhWEK7ybzj5bGsnVraHKaknpMFPgpg8jHYbGFo
    workers:
      - network_address: /ip4/35.203.41.60/udp/49594/quic-v1/p2p/12D3KooWHGdbYerasxspuhZW4KqNMbbpqvLNCG6LGpS5yx6b8x2x
        network_key: 4XTTM8bKFPoKfVL1UwN1vBMjCWCFVKbKpKUha12qr2Bi86ip4
  rZdhyVGJAtz7kyFtFARWVVcw5DB3o9E4GvPN6zuKfUhUBGFRvEK2PgEV44CJt9wu9GMyssFQBFXRknrenuhJixo9KCADrZzHdtcvAwAYDmcLc7Ugh9epGc5LRZC5TK2hnYA:
    primary:
      network_address: /ip6/2001:db8::34/udp/49590/quic-v1/p2p/12D3KooWBRH7XpYhxCPTrw364UnmaSYDR4hCotoTHxTWTjKfrcUm
      network_key: 4XTTM2jxmNy1nZZX7tbVWBWgbj3Bstjgew7Qh2AsGVxwCpPFs
    workers:
      - network_address: /dns4/worker.example/udp/49594/quic-v1
        network_key: 4XTTM2jxmNy1nZZX7tbVWBWgbj3Bstjgew7Qh2AsGVxwCpPFs
num_workers: 1
";

    #[test]
    fn capture_floor_reads_a_bootstrap_peers_file() -> TestResult {
        let temp = TempDir::new("capture-peers")?;
        let path = temp.join("bootstrap-peers.yaml");
        fs::write(&path, BOOTSTRAP_PEERS_YAML)?;
        // the dns4 names are never resolved, even one that looks like an address,
        // the mapped address becomes IPv4 and the link-local one is dropped
        assert_eq!(
            capture_floor(&[path])?,
            ips(&["35.203.41.59", "198.51.100.7", "2001:db8::7"])?
        );
        Ok(())
    }

    #[test]
    fn capture_floor_reads_a_genesis_committee() -> TestResult {
        let temp = TempDir::new("capture-committee")?;
        let path = temp.join("committee.yaml");
        fs::write(&path, COMMITTEE_YAML)?;
        assert_eq!(
            capture_floor(&[path])?,
            ips(&["35.203.41.59", "35.203.41.60", "2001:db8::34"])?
        );
        Ok(())
    }

    #[test]
    fn capture_floor_reads_the_json_form_and_unites_files() -> TestResult {
        let temp = TempDir::new("capture-json")?;
        let json = temp.join("bootstrap-peers.json");
        fs::write(
            &json,
            r#"{"0xab":{"primary":{"network_address":"/ip4/192.0.2.10/udp/49590/quic-v1","network_key":"k"},"workers":[{"network_address":"/ip6/2001:db8::10/udp/49594/quic-v1","network_key":"k"}]},"n":["/ip4/192.0.2.11",'/ip4/192.0.2.12',{/ip4/192.0.2.13}]}"#,
        )?;
        let yaml = temp.join("committee.yaml");
        fs::write(&yaml, COMMITTEE_YAML)?;
        let empty = temp.join("empty.yaml");
        fs::write(&empty, "{}\n")?;
        assert_eq!(
            capture_floor(&[json, yaml, empty.clone()])?,
            ips(&[
                "35.203.41.59",
                "35.203.41.60",
                "192.0.2.10",
                "192.0.2.11",
                "192.0.2.12",
                "192.0.2.13",
                "2001:db8::10",
                "2001:db8::34",
            ])?
        );
        assert_eq!(capture_floor(&[empty])?, BTreeSet::new());
        Ok(())
    }

    #[test]
    fn capture_floor_takes_only_leading_ip_segments() {
        let text = b"key:/ip4/192.0.2.1 x/ip4/192.0.2.2 /dns4/a.example/ip4/192.0.2.3 /ip4/192.0.2.4/ip4/192.0.2.5 /ip6/fe80::1%eth0/udp/1 /ip6zone/eth0/ip6/fe80::2 /IP4/192.0.2.6 /ip4/192.0.2.300 /ip4/192.0.2.7\n/ip6/::ffff:224.0.0.1";
        assert_eq!(
            scan_ips(text).collect::<Vec<_>>(),
            [
                IpAddr::V4(Ipv4Addr::new(192, 0, 2, 4)),
                IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7))
            ]
        );
        assert_eq!(
            scan_ips(b"/ip6/2001:db8::1,/ip6/::1").collect::<Vec<_>>(),
            [
                IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            ]
        );
    }

    #[test]
    fn capture_floor_refuses_an_input_it_cannot_read() -> TestResult {
        let temp = TempDir::new("capture-missing")?;
        let good = temp.join("committee.yaml");
        fs::write(&good, COMMITTEE_YAML)?;
        let missing = temp.join("bootstrap-peers.yaml");
        for bad in [missing, temp.path().to_owned()] {
            let error = capture_floor(&[good.clone(), bad.clone()])
                .map_err(|e| (e.token(), e.line(), e.to_string()));
            let Err((token, line, message)) = error else {
                return Err(format!("{bad:?} was accepted").into());
            };
            assert_eq!((token, line), ("floor-invalid", None));
            assert!(
                message.starts_with(&format!("{bad:?}: cannot read: ")),
                "{message}"
            );
        }
        Ok(())
    }

    #[test]
    fn capture_floor_refuses_a_fifo_without_blocking() -> TestResult {
        let temp = TempDir::new("capture-fifo")?;
        let fifo = temp.join("bootstrap-peers.yaml");
        if !mkfifo(&fifo)? {
            println!("skipped: mkfifo is not on PATH");
            return Ok(());
        }
        // a symlink is followed, and what it reaches is checked
        let link = temp.join("committee.yaml");
        symlink(&fifo, &link)?;
        for bad in [fifo, link] {
            let input = bad.clone();
            let error = without_blocking(move || capture_floor(&[input]))?;
            assert_eq!(
                error.map_err(|e| (e.token(), e.to_string())).err(),
                Some((
                    "floor-invalid",
                    format!("{bad:?}: cannot read: is not a regular file")
                ))
            );
        }
        Ok(())
    }

    #[test]
    fn capture_floor_refuses_a_socket_file() -> TestResult {
        let temp = TempDir::new("capture-socket")?;
        let socket = temp.join("bootstrap-peers.yaml");
        let _listener = UnixListener::bind(&socket)?;
        let input = socket.clone();
        let error = without_blocking(move || capture_floor(&[input]))?
            .map_err(|e| (e.token(), e.line(), e.to_string()));
        // refused by the check before the open, so the socket is never opened
        let Err((token, line, message)) = error else {
            return Err("the socket was accepted".into());
        };
        assert_eq!((token, line), ("floor-invalid", None));
        assert_eq!(
            message,
            format!("{socket:?}: cannot read: is not a regular file")
        );
        Ok(())
    }

    #[test]
    fn capture_floor_follows_a_symlinked_input() -> TestResult {
        let temp = TempDir::new("capture-link")?;
        let real = temp.join("committee.yaml");
        fs::write(&real, COMMITTEE_YAML)?;
        let link = temp.join("link.yaml");
        symlink(&real, &link)?;
        assert_eq!(
            capture_floor(&[link])?,
            ips(&["35.203.41.59", "35.203.41.60", "2001:db8::34"])?
        );

        let dangling = temp.join("dangling.yaml");
        symlink(temp.join("nowhere.yaml"), &dangling)?;
        let error = capture_floor(&[dangling]).map_err(|e| e.token());
        assert_eq!(error.err(), Some("floor-invalid"));
        Ok(())
    }

    #[test]
    fn capture_floor_reads_16_mib_and_refuses_one_byte_more() -> TestResult {
        let temp = TempDir::new("capture-size")?;
        let tail = b"the last bytes\n";
        let exact = temp.join("exact.yaml");
        sparse_file(&exact, MAX_CAPTURE_INPUT, tail)?;
        // read directly: scanning 16 MiB of zeros takes seconds in a debug build, and the scan has tests of its own
        let bytes = read_capture_input(&exact)?;
        assert_eq!(u64::try_from(bytes.len())?, MAX_CAPTURE_INPUT);
        assert!(bytes.ends_with(tail));

        let larger = temp.join("larger.yaml");
        let len = MAX_CAPTURE_INPUT.checked_add(1).ok_or("overflow")?;
        sparse_file(&larger, len, tail)?;
        let error = capture_floor(std::slice::from_ref(&larger))
            .map_err(|e| (e.token(), e.line(), e.to_string()));
        assert_eq!(
            error.err(),
            Some((
                "floor-invalid",
                None,
                format!("{larger:?}: cannot read: is larger than 16777216 bytes")
            ))
        );
        Ok(())
    }
}
