//! Runs `nft` and reads its JSON listings of the `inet tn_p2p_allow` table.
//!
//! The tool runs exactly one program, `/usr/sbin/nft` unless `--nft PATH` names another, never through a shell and always with a cleared environment.
//! The listing parser accepts every element form nft prints and yields parsed addresses, so callers compare values and never text: inet_ntop and Rust format some IPv6 addresses differently.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The nft binary the tool runs unless `--nft PATH` names another.
pub const DEFAULT_PATH: &str = "/usr/sbin/nft";

// the whole environment nft gets; LC_ALL=C also fixes the strerror text the runner looks for
const CHILD_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

const FAMILY: &str = "inet";
const TABLE: &str = "tn_p2p_allow";
const CHAIN: &str = "input";

/// The sets the program declares, the only ones whose elements the listing reads.
const SETS: [&str; 4] = ["allow_v4", "allow_v6", "static_v4", "static_v6"];

/// Why a call to nft did not succeed.
///
/// Every variant is exit 2 for the caller, which picks the token by what it was doing: `nft-unavailable`, `nft-apply-failed` or `reload-failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NftError {
    /// nft could not be started, typically because the path does not exist; holds the path and the OS error.
    Unavailable(String),
    /// nft was not allowed to run or was refused the netlink socket: the tool must run as root.
    ///
    /// nft 1.0.2 says nothing on stderr when `-f` or `-c` is refused, so there the refusal shows up as [`NftError::Failed`] with empty `stderr`; its `list` calls do say it.
    Permission,
    /// nft ran and failed, or a signal ended it.
    Failed {
        /// The exit status, or `None` when a signal ended nft.
        code: Option<i32>,
        /// What nft printed on stderr, trailing whitespace removed.
        stderr: String,
    },
    /// nft succeeded but printed JSON the parser does not understand.
    Parse(String),
}

impl fmt::Display for NftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "nft unavailable: {why}"),
            Self::Permission => f.write_str("nft: operation not permitted, run as root"),
            Self::Failed { code, stderr } => {
                match code {
                    Some(code) => write!(f, "nft exited with status {code}")?,
                    None => f.write_str("nft was killed by a signal")?,
                }
                // nft puts the message first and then echoes the offending input with a caret line
                match stderr.lines().map(str::trim).find(|line| !line.is_empty()) {
                    Some(line) => write!(f, ": {line}"),
                    None => Ok(()),
                }
            }
            Self::Parse(why) => write!(f, "nft output not understood: {why}"),
        }
    }
}

impl std::error::Error for NftError {}

/// One element of an nft set, as nft printed it.
///
/// nft prints a single host as a bare address even in an interval set, a prefix as `{"prefix": …}` and other intervals as `{"range": […]}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Element {
    /// A single address.
    Ip(IpAddr),
    /// A network.
    Cidr {
        /// The address nft printed, which is the network address.
        net: IpAddr,
        /// The prefix length, at most 32 for IPv4 and 128 for IPv6.
        prefix: u8,
    },
    /// An inclusive range from the first address to the second, both of one family.
    Range(IpAddr, IpAddr),
}

/// A named counter's totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counter {
    /// Packets counted since the counter was created.
    pub packets: u64,
    /// Bytes counted since the counter was created.
    pub bytes: u64,
}

/// The base-chain declaration of chain `input`, as nft lists it.
///
/// A field nft did not print, as for a regular chain, or printed in a form the parser does not read is `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chain {
    /// The chain type, `filter` in the program.
    pub kind: Option<String>,
    /// The hook, `input` in the program.
    pub hook: Option<String>,
    /// The priority, -20 in the program.
    pub prio: Option<i64>,
    /// The policy, `accept` in the program.
    pub policy: Option<String>,
}

/// One expression of a rule in chain `input`, in the forms the program's rules are made of.
///
/// Every other expression, and one of these forms with another operator, field or argument, is [`Expr::Other`], so a rule holding one never equals a rule of the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// `udp dport` equal to one of these ports; nft lists a single port as a number and more as an anonymous set.
    UdpDport(BTreeSet<u16>),
    /// `ip saddr` or `ip6 saddr` looked up in a named set.
    Saddr {
        /// The protocol keyword, `ip` or `ip6`.
        protocol: String,
        /// The set's name, without the `@`.
        set: String,
    },
    /// `counter name NAME`: a named counter, by name.
    Counter(String),
    /// The `accept` verdict.
    Accept,
    /// The `drop` verdict.
    Drop,
    /// Any other expression.
    Other,
}

/// What `nft -j list table inet tn_p2p_allow` says about the table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// The program's sets `allow_v4`, `allow_v6`, `static_v4` and `static_v6` that the table holds, by name, their elements sorted; a set without elements maps to an empty list.
    ///
    /// Any other set of the table, like a map, is not read: its elements need not be addresses.
    pub sets: BTreeMap<String, Vec<Element>>,
    /// Every named counter of the table by name.
    pub counters: BTreeMap<String, Counter>,
    /// The declaration of chain `input`, or `None` when the chain does not exist.
    pub chain: Option<Chain>,
    /// The expressions of each rule of chain `input`, the rules in chain order.
    pub rules: Vec<Vec<Expr>>,
    /// Whether the table is dormant, so none of its chains sees a packet: its `table` object has a `flags` key.
    ///
    /// The program sets no table flags and nft prints the key only for a table that has some, `dormant` being the one an operator sets by hand.
    /// Only the key's presence counts: nft 1.1.3 prints `"dormant"`, but 1.0.2 and 1.0.9 print some other string of the same output in its place.
    pub dormant: bool,
}

/// Runs nft from one fixed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nft {
    path: PathBuf,
}

impl Nft {
    /// A runner for `path`, or for [`DEFAULT_PATH`] when `path` is `None`.
    ///
    /// The caller passes an absolute path and warns when it is not the default.
    pub fn new(path: Option<&Path>) -> Self {
        Self {
            path: path.map_or_else(|| PathBuf::from(DEFAULT_PATH), Path::to_path_buf),
        }
    }

    /// The program this runner starts.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether table `inet tn_p2p_allow` exists, from `nft -j list tables inet`.
    ///
    /// Listing every inet table succeeds whether or not ours is among them, so an error here means nft is broken, never that the table is absent.
    pub fn table_present(&self) -> Result<bool, NftError> {
        let output = self.run(&["-j", "list", "tables", FAMILY].map(OsStr::new), None)?;
        parse_tables(&output.stdout)
    }

    /// The sets, named counters and chain `input` of table `inet tn_p2p_allow`, from `nft -j list table inet tn_p2p_allow`.
    ///
    /// Fails with [`NftError::Failed`] when the table is absent; [`Nft::table_present`] tells the two apart.
    pub fn list_table(&self) -> Result<Listing, NftError> {
        let output = self.run(
            &["-j", "list", "table", FAMILY, TABLE].map(OsStr::new),
            None,
        )?;
        parse_listing(&output.stdout)
    }

    /// Loads the program in `path` with `nft -f`.
    ///
    /// nft applies the file as one transaction, so on failure the live ruleset is unchanged.
    pub fn load_file(&self, path: &Path) -> Result<(), NftError> {
        self.run(&[OsStr::new("-f"), path.as_os_str()], None)?;
        Ok(())
    }

    /// Checks `text` with `nft -c -f -`, which parses and evaluates it against the live ruleset without loading it.
    pub fn check_program(&self, text: &str) -> Result<(), NftError> {
        self.run(&["-c", "-f", "-"].map(OsStr::new), Some(text.as_bytes()))?;
        Ok(())
    }

    /// Deletes table `inet tn_p2p_allow` with `nft delete table`; fails with [`NftError::Failed`] when the table is absent.
    ///
    /// The sync never deletes the table, even on `off`; this is here for tests that reload it from nothing.
    pub fn delete_table(&self) -> Result<(), NftError> {
        self.run(&["delete", "table", FAMILY, TABLE].map(OsStr::new), None)?;
        Ok(())
    }

    /// Runs nft with `args`, feeding it `stdin` when given and `/dev/null` otherwise, and returns its output when it exits 0.
    fn run(&self, args: &[&OsStr], stdin: Option<&[u8]>) -> Result<Output, NftError> {
        // the one place the tool starts a program: a fixed path, no shell, and a cleared
        // environment, so nothing the tool was started with reaches nft
        #[allow(clippy::disallowed_methods)]
        let mut command = Command::new(&self.path);
        command
            .args(args)
            .env_clear()
            .env("PATH", CHILD_PATH)
            .env("LC_ALL", "C")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|err| self.spawn_error(&err))?;
        // nft reads all of its input before it writes anything and gives up after ten errors,
        // so writing stdin first cannot leave both sides blocked on full pipes; the pipe is
        // dropped, and nft sees end of file, when this match ends
        let written = match (stdin, child.stdin.take()) {
            (Some(bytes), Some(mut pipe)) => pipe.write_all(bytes),
            _ => Ok(()),
        };
        // polls stdout and stderr together, the way Command::output does
        let output = child.wait_with_output().map_err(|err| NftError::Failed {
            code: None,
            stderr: format!("reading the output of nft: {err}"),
        })?;
        outcome(output, written)
    }

    fn spawn_error(&self, err: &io::Error) -> NftError {
        match err.kind() {
            io::ErrorKind::PermissionDenied => NftError::Permission,
            _ => NftError::Unavailable(format!("{}: {err}", self.path.display())),
        }
    }
}

/// Turns a finished run into its output on success or the error it amounts to.
fn outcome(output: Output, written: io::Result<()>) -> Result<Output, NftError> {
    let code = output.status.code();
    if output.status.success() {
        return match written {
            Ok(()) => Ok(output),
            // success on part of a program says nothing about the program
            Err(err) => Err(NftError::Failed {
                code,
                stderr: format!("nft exited before reading all of its input: {err}"),
            }),
        };
    }
    let stderr = String::from_utf8_lossy(&output.stderr)
        .trim_end()
        .to_owned();
    // only picks the message; a failure is exit 2 either way
    if code == Some(1) && stderr.contains("Operation not permitted") {
        return Err(NftError::Permission);
    }
    Err(NftError::Failed { code, stderr })
}

#[derive(Deserialize)]
struct TableObject {
    family: String,
    name: String,
}

#[derive(Deserialize)]
struct SetObject {
    family: String,
    table: String,
    name: String,
    // nft leaves the key out when the set is empty; read only for the program's sets, so
    // another set's elements may take any form
    #[serde(default)]
    elem: Value,
}

#[derive(Deserialize)]
struct CounterObject {
    family: String,
    table: String,
    name: String,
    packets: u64,
    bytes: u64,
}

#[derive(Deserialize)]
struct ChainObject {
    family: String,
    table: String,
    name: String,
    // a regular chain has none of these; a form the parser does not read counts as missing,
    // which makes the idempotence check re-apply rather than fail the run
    #[serde(rename = "type", default)]
    kind: Value,
    #[serde(default)]
    hook: Value,
    #[serde(default)]
    prio: Value,
    #[serde(default)]
    policy: Value,
}

#[derive(Deserialize)]
struct RuleObject {
    family: String,
    table: String,
    chain: String,
    #[serde(default)]
    expr: Vec<Value>,
}

#[derive(Deserialize)]
struct MatchObject {
    op: String,
    left: MatchLeft,
    right: Value,
}

#[derive(Deserialize)]
struct MatchLeft {
    payload: PayloadObject,
}

#[derive(Deserialize)]
struct PayloadObject {
    protocol: String,
    field: String,
}

/// The right side of a `udp dport` match: one port, or an anonymous set of ports.
#[derive(Deserialize)]
#[serde(untagged)]
enum PortsObject {
    One(u16),
    Set { set: BTreeSet<u16> },
}

#[derive(Deserialize)]
struct ElemObject {
    val: Value,
}

#[derive(Deserialize)]
struct PrefixObject {
    addr: String,
    len: u8,
}

fn ours(family: &str, table: &str) -> bool {
    family == FAMILY && table == TABLE
}

/// The objects of an nft JSON document as `(kind, body)` pairs; entries of the `nftables` array that are not objects are skipped.
fn objects(json: &[u8]) -> Result<impl Iterator<Item = (String, Value)>, NftError> {
    let document: Value =
        serde_json::from_slice(json).map_err(|err| NftError::Parse(format!("not JSON: {err}")))?;
    let Value::Object(mut top) = document else {
        return Err(NftError::Parse("not a JSON object".to_owned()));
    };
    let Some(Value::Array(entries)) = top.remove("nftables") else {
        return Err(NftError::Parse("no nftables array".to_owned()));
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry {
            Value::Object(object) => Some(object),
            _ => None,
        })
        .flatten())
}

fn typed<T: DeserializeOwned>(kind: &str, body: Value) -> Result<T, NftError> {
    serde_json::from_value(body).map_err(|err| NftError::Parse(format!("{kind} object: {err}")))
}

/// Whether `nft -j list tables` output names table `inet tn_p2p_allow`.
fn parse_tables(json: &[u8]) -> Result<bool, NftError> {
    let mut present = false;
    for (kind, body) in objects(json)? {
        if kind == "table" {
            let table: TableObject = typed(&kind, body)?;
            present |= table.family == FAMILY && table.name == TABLE;
        }
    }
    Ok(present)
}

/// Reads `nft -j list table inet tn_p2p_allow` output; objects of other kinds, or of other tables, and sets the program does not declare are skipped.
fn parse_listing(json: &[u8]) -> Result<Listing, NftError> {
    let mut listing = Listing::default();
    for (kind, body) in objects(json)? {
        match kind.as_str() {
            "table" => {
                // the key of any type, null included: on nft 1.0.x its value is not the flag
                let flagged = body.get("flags").is_some();
                let table: TableObject = typed(&kind, body)?;
                if ours(&table.family, &table.name) {
                    listing.dormant |= flagged;
                }
            }
            "set" => {
                let set: SetObject = typed(&kind, body)?;
                if ours(&set.family, &set.table) && SETS.contains(&set.name.as_str()) {
                    let elements = parse_elements(set.elem)
                        .map_err(|why| NftError::Parse(format!("set {}: {why}", set.name)))?;
                    listing.sets.insert(set.name, elements);
                }
            }
            "counter" => {
                let counter: CounterObject = typed(&kind, body)?;
                if ours(&counter.family, &counter.table) {
                    let totals = Counter {
                        packets: counter.packets,
                        bytes: counter.bytes,
                    };
                    listing.counters.insert(counter.name, totals);
                }
            }
            "chain" => {
                let chain: ChainObject = typed(&kind, body)?;
                if ours(&chain.family, &chain.table) && chain.name == CHAIN {
                    listing.chain = Some(Chain {
                        kind: text(chain.kind),
                        hook: text(chain.hook),
                        prio: chain.prio.as_i64(),
                        policy: text(chain.policy),
                    });
                }
            }
            "rule" => {
                let rule: RuleObject = typed(&kind, body)?;
                if ours(&rule.family, &rule.table) && rule.chain == CHAIN {
                    listing
                        .rules
                        .push(rule.expr.into_iter().map(parse_expr).collect());
                }
            }
            _ => {}
        }
    }
    Ok(listing)
}

/// A string value, or `None` for anything else.
fn text(value: Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text),
        _ => None,
    }
}

/// One expression of a rule: an object with a single key naming its kind.
fn parse_expr(expr: Value) -> Expr {
    let Value::Object(object) = expr else {
        return Expr::Other;
    };
    let mut fields = object.into_iter();
    let (Some((kind, body)), None) = (fields.next(), fields.next()) else {
        return Expr::Other;
    };
    match (kind.as_str(), body) {
        ("match", body) => serde_json::from_value(body).map_or(Expr::Other, parse_match),
        // an anonymous counter is an object of totals, a named one only its name
        ("counter", Value::String(name)) => Expr::Counter(name),
        ("accept", Value::Null) => Expr::Accept,
        ("drop", Value::Null) => Expr::Drop,
        _ => Expr::Other,
    }
}

/// A `match` expression: `udp dport` against ports, or `ip`/`ip6` `saddr` against a named set, both with `==`.
fn parse_match(found: MatchObject) -> Expr {
    let MatchObject {
        op,
        left: MatchLeft {
            payload: PayloadObject { protocol, field },
        },
        right,
    } = found;
    if op != "==" {
        return Expr::Other;
    }
    match (protocol.as_str(), field.as_str()) {
        ("udp", "dport") => match serde_json::from_value(right) {
            Ok(PortsObject::One(port)) => Expr::UdpDport(BTreeSet::from([port])),
            Ok(PortsObject::Set { set }) => Expr::UdpDport(set),
            Err(_) => Expr::Other,
        },
        ("ip" | "ip6", "saddr") => match right.as_str().and_then(|right| right.strip_prefix('@')) {
            Some(set) => Expr::Saddr {
                set: set.to_owned(),
                protocol,
            },
            None => Expr::Other,
        },
        _ => Expr::Other,
    }
}

/// A set's `elem` array, sorted; `null`, for a key nft left out, is an empty set.
fn parse_elements(elem: Value) -> Result<Vec<Element>, String> {
    let entries = match elem {
        Value::Null => Vec::new(),
        Value::Array(entries) => entries,
        other => return Err(format!("elem is not an array: {other}")),
    };
    let mut elements = entries
        .into_iter()
        .map(parse_element)
        .collect::<Result<Vec<_>, _>>()?;
    elements.sort_unstable();
    Ok(elements)
}

/// One entry of a set's `elem` array: a value, or an `elem` object wrapping one with extras such as a comment or a counter.
fn parse_element(entry: Value) -> Result<Element, String> {
    let value = match entry {
        Value::Object(mut object) => match object.remove("elem") {
            Some(elem) => {
                serde_json::from_value::<ElemObject>(elem)
                    .map_err(|err| format!("elem object: {err}"))?
                    .val
            }
            None => Value::Object(object),
        },
        other => other,
    };
    parse_value(value)
}

/// A bare address, a `prefix` object or a `range` object.
fn parse_value(value: Value) -> Result<Element, String> {
    let Value::Object(mut object) = value else {
        return match value {
            Value::String(text) => parse_ip(&text).map(Element::Ip),
            other => Err(format!("unknown element form {other}")),
        };
    };
    if let Some(prefix) = object.remove("prefix") {
        let PrefixObject { addr, len } =
            serde_json::from_value(prefix).map_err(|err| format!("prefix object: {err}"))?;
        let net = parse_ip(&addr)?;
        let longest = if net.is_ipv4() { 32 } else { 128 };
        if len > longest {
            return Err(format!("prefix length {len} on {addr}"));
        }
        return Ok(Element::Cidr { net, prefix: len });
    }
    if let Some(range) = object.remove("range") {
        let (first, last): (String, String) =
            serde_json::from_value(range).map_err(|err| format!("range object: {err}"))?;
        let (first, last) = (parse_ip(&first)?, parse_ip(&last)?);
        if first.is_ipv4() != last.is_ipv4() {
            return Err(format!("range from {first} to {last} mixes families"));
        }
        return Ok(Element::Range(first, last));
    }
    Err(format!("unknown element form {}", Value::Object(object)))
}

fn parse_ip(text: &str) -> Result<IpAddr, String> {
    text.parse()
        .map_err(|_| format!("{text:?} is not an IP address"))
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;

    type TestResult = Result<(), Box<dyn Error>>;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nft");
    const VERSIONS: [&str; 3] = [
        "debian-13-1.1.3",
        "ubuntu-24.04-1.0.9",
        "ubuntu-22.04-1.0.2",
    ];
    /// The version every other one is compared with.
    const REFERENCE: &str = "ubuntu-24.04-1.0.9";
    const LISTINGS: [&str; 7] = [
        "program",
        "ipv6",
        "empty",
        "traffic",
        "forms",
        "adjacent",
        "single-port",
    ];

    fn fixture(name: &str) -> io::Result<Vec<u8>> {
        std::fs::read(Path::new(FIXTURES).join(name))
    }

    fn listing(version: &str, case: &str) -> Result<Listing, Box<dyn Error>> {
        Ok(parse_listing(&fixture(&format!("{version}-{case}.json"))?)?)
    }

    fn fake() -> Nft {
        Nft::new(Some(&Path::new(FIXTURES).join("fake-nft.sh")))
    }

    fn os(args: &[&'static str]) -> Vec<&'static OsStr> {
        args.iter().map(|arg| OsStr::new(*arg)).collect()
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn v6(segments: [u16; 8]) -> IpAddr {
        IpAddr::V6(Ipv6Addr::from(segments))
    }

    fn ip4(a: u8, b: u8, c: u8, d: u8) -> Element {
        Element::Ip(v4(a, b, c, d))
    }

    /// The listing of the reference table with the given sets, both counters at the given totals and the five rules.
    fn table(
        [allow_v4, allow_v6, static_v4, static_v6]: [Vec<Element>; 4],
        accept: (u64, u64),
        drop: (u64, u64),
    ) -> Listing {
        let sets = [
            ("allow_v4", allow_v4),
            ("allow_v6", allow_v6),
            ("static_v4", static_v4),
            ("static_v6", static_v6),
        ];
        let counters = [
            ("tn_accept_p2p_listed", accept),
            ("tn_drop_p2p_unlisted", drop),
        ];
        Listing {
            sets: sets
                .into_iter()
                .map(|(name, elements)| (name.to_owned(), elements))
                .collect(),
            counters: counters
                .into_iter()
                .map(|(name, (packets, bytes))| (name.to_owned(), Counter { packets, bytes }))
                .collect(),
            chain: Some(program_chain()),
            rules: program_rules(),
            dormant: false,
        }
    }

    /// The declaration of the reference program's chain `input`.
    fn program_chain() -> Chain {
        Chain {
            kind: Some("filter".to_owned()),
            hook: Some("input".to_owned()),
            prio: Some(-20),
            policy: Some("accept".to_owned()),
        }
    }

    /// The five rules of the reference program, on ports 49590 and 49594.
    fn program_rules() -> Vec<Vec<Expr>> {
        rules_on(&[49590, 49594])
    }

    /// The five rules of the reference program on `ports`.
    fn rules_on(ports: &[u16]) -> Vec<Vec<Expr>> {
        let ports = || Expr::UdpDport(ports.iter().copied().collect());
        let accept = |protocol: &str, set: &str| {
            vec![
                ports(),
                saddr(protocol, set),
                Expr::Counter("tn_accept_p2p_listed".to_owned()),
                Expr::Accept,
            ]
        };
        vec![
            accept("ip", "allow_v4"),
            accept("ip", "static_v4"),
            accept("ip6", "allow_v6"),
            accept("ip6", "static_v6"),
            vec![
                ports(),
                Expr::Counter("tn_drop_p2p_unlisted".to_owned()),
                Expr::Drop,
            ],
        ]
    }

    fn saddr(protocol: &str, set: &str) -> Expr {
        Expr::Saddr {
            protocol: protocol.to_owned(),
            set: set.to_owned(),
        }
    }

    /// The listing of a document holding only chain `input` and one rule with `exprs`.
    fn one_rule(chain: &str, exprs: &str) -> Result<Listing, NftError> {
        let json = format!(
            r#"{{"nftables": [{{"chain": {{"family": "inet", "table": "tn_p2p_allow", "name": "input"{chain}}}}}, {{"rule": {{"family": "inet", "table": "tn_p2p_allow", "chain": "input", "expr": [{exprs}]}}}}]}}"#
        );
        parse_listing(json.as_bytes())
    }

    /// The sets of `capture/program.nft`: two `allow_v4` addresses and one `static_v4` network.
    fn program_sets() -> [Vec<Element>; 4] {
        let static_v4 = Element::Cidr {
            net: v4(203, 0, 113, 0),
            prefix: 24,
        };
        [
            vec![ip4(192, 0, 2, 10), ip4(198, 51, 100, 7)],
            vec![],
            vec![static_v4],
            vec![],
        ]
    }

    fn program_listing() -> Listing {
        table(program_sets(), (0, 0), (0, 0))
    }

    fn status(code: i32) -> ExitStatus {
        // a wait status holds the exit code in its second byte
        ExitStatus::from_raw(code.saturating_mul(256))
    }

    fn finished(code: i32, stderr: &[u8]) -> Output {
        Output {
            status: status(code),
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn the_reference_program_lists_as_its_sets_counters_and_rules() -> TestResult {
        for version in VERSIONS {
            assert_eq!(listing(version, "program")?, program_listing(), "{version}");
        }
        Ok(())
    }

    #[test]
    fn an_ipv6_only_table_lists_its_v6_sets() -> TestResult {
        let expected = table(
            [
                vec![],
                vec![
                    Element::Ip(v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1])),
                    Element::Ip(v6([0x2001, 0xdb8, 0, 1, 0, 0, 0, 7])),
                ],
                vec![],
                vec![Element::Cidr {
                    net: v6([0x2001, 0xdb8, 0x100, 0, 0, 0, 0, 0]),
                    prefix: 48,
                }],
            ],
            (0, 0),
            (0, 0),
        );
        for version in VERSIONS {
            assert_eq!(listing(version, "ipv6")?, expected, "{version}");
        }
        Ok(())
    }

    #[test]
    fn empty_sets_list_as_empty_and_still_exist() -> TestResult {
        let expected = table([vec![], vec![], vec![], vec![]], (0, 0), (0, 0));
        for version in VERSIONS {
            assert_eq!(listing(version, "empty")?, expected, "{version}");
        }
        Ok(())
    }

    #[test]
    fn counters_carry_the_traffic_they_saw() -> TestResult {
        // three 1-byte datagrams from the listed address, two 2-byte ones from 127.0.0.1
        let expected = table(
            [
                vec![ip4(192, 0, 2, 10)],
                vec![],
                vec![Element::Cidr {
                    net: v4(203, 0, 113, 0),
                    prefix: 24,
                }],
                vec![],
            ],
            (3, 87),
            (2, 60),
        );
        for version in VERSIONS {
            assert_eq!(listing(version, "traffic")?, expected, "{version}");
        }
        Ok(())
    }

    #[test]
    fn every_element_form_parses_to_values() -> TestResult {
        let v4_compatible = v6([0, 0, 0, 0, 0, 0, 0xc000, 0x0201]);
        let expected = table(
            [
                vec![ip4(192, 0, 2, 10), ip4(198, 51, 100, 7)],
                vec![
                    Element::Ip(v4_compatible),
                    Element::Ip(v6([0, 0, 0, 0, 0, 0xffff, 0xc633, 0x6407])),
                    Element::Ip(v6([0x2001, 0xdb8, 0, 1, 1, 1, 1, 1])),
                ],
                vec![
                    ip4(198, 51, 100, 9),
                    Element::Cidr {
                        net: v4(172, 16, 0, 0),
                        prefix: 12,
                    },
                    Element::Cidr {
                        net: v4(203, 0, 113, 0),
                        prefix: 24,
                    },
                    Element::Range(v4(10, 0, 0, 1), v4(10, 0, 0, 5)),
                ],
                vec![
                    Element::Ip(v6([0x2001, 0xdb8, 0x300, 0, 0, 0, 0, 5])),
                    Element::Cidr {
                        net: v6([0x2001, 0xdb8, 0x100, 0, 0, 0, 0, 0]),
                        prefix: 48,
                    },
                    Element::Range(
                        v6([0x2001, 0xdb8, 0x200, 0, 0, 0, 0, 1]),
                        v6([0x2001, 0xdb8, 0x200, 0, 0, 0, 0, 9]),
                    ),
                ],
            ],
            (0, 0),
            (0, 0),
        );
        for version in VERSIONS {
            let json = String::from_utf8(fixture(&format!("{version}-forms.json"))?)?;
            // the reason values are compared: nft and Rust print this address differently
            assert!(json.contains("\"::192.0.2.1\""), "{version}");
            assert!(!json.contains(&format!("\"{v4_compatible}\"")), "{version}");
            assert_eq!(listing(version, "forms")?, expected, "{version}");
        }
        Ok(())
    }

    #[test]
    fn every_nft_version_lists_the_same_table() -> TestResult {
        for version in VERSIONS {
            for case in LISTINGS {
                assert_eq!(
                    listing(version, case)?,
                    listing(REFERENCE, case)?,
                    "{version} {case}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn adjacent_networks_list_as_separate_prefixes() -> TestResult {
        let net = |net: IpAddr, prefix: u8| Element::Cidr { net, prefix };
        let expected = table(
            [
                vec![],
                vec![],
                vec![
                    net(v4(10, 0, 0, 0), 24),
                    net(v4(10, 0, 1, 0), 24),
                    net(v4(10, 0, 3, 0), 24),
                ],
                vec![
                    net(v6([0x2001, 0xdb8, 1, 0, 0, 0, 0, 0]), 48),
                    net(v6([0x2001, 0xdb8, 2, 0, 0, 0, 0, 0]), 48),
                    net(v6([0x2001, 0xdb8, 4, 0, 0, 0, 0, 0]), 48),
                ],
            ],
            (0, 0),
            (0, 0),
        );
        for version in VERSIONS {
            assert_eq!(listing(version, "adjacent")?, expected, "{version}");
        }
        Ok(())
    }

    #[test]
    fn a_single_port_lists_as_the_one_port_rule_form() -> TestResult {
        let expected = Listing {
            rules: rules_on(&[49590]),
            ..program_listing()
        };
        for version in VERSIONS {
            let json = String::from_utf8(fixture(&format!("{version}-single-port.json"))?)?;
            // a bare number, never an anonymous set of one
            assert!(json.contains(r#""right": 49590}"#), "{version}");
            assert!(!json.contains(r#""set": [49590]"#), "{version}");
            assert_eq!(listing(version, "single-port")?, expected, "{version}");
        }
        Ok(())
    }

    #[test]
    fn list_tables_tells_whether_ours_exists() -> TestResult {
        for version in VERSIONS {
            for (case, present) in [
                ("tables-none", false),
                ("tables-other", false),
                ("tables-present", true),
                ("tables-both", true),
                ("dormant-tables", true),
            ] {
                let json = fixture(&format!("{version}-{case}.json"))?;
                assert_eq!(parse_tables(&json)?, present, "{version} {case}");
            }
        }
        Ok(())
    }

    #[test]
    fn a_dormant_table_lists_as_dormant_whatever_its_flags_value() -> TestResult {
        // one datagram from an unlisted source before the flag, two while dormant: only the first was counted
        let expected = Listing {
            dormant: true,
            ..table(program_sets(), (0, 0), (1, 29))
        };
        for version in VERSIONS {
            let json = String::from_utf8(fixture(&format!("{version}-dormant.json"))?)?;
            // only 1.1.3 prints the flag itself; 1.0.x prints another string of the output
            let named = json.contains(r#""flags": "dormant""#);
            assert_eq!(named, version.ends_with("-1.1.3"), "{version}");
            assert_eq!(listing(version, "dormant")?, expected, "{version}");
        }
        Ok(())
    }

    #[test]
    fn re_applying_the_program_wakes_a_dormant_table() -> TestResult {
        // two more datagrams after the reload, both counted again
        let expected = table(program_sets(), (0, 0), (3, 87));
        for version in VERSIONS {
            assert_eq!(
                listing(version, "dormant-reapplied")?,
                expected,
                "{version}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_dormant_listing_cut_short_by_nft_is_a_parse_error() -> TestResult {
        // nft 1.0.9 stops right after `"flags": ` for this table and still exits 0
        let json = fixture("ubuntu-24.04-1.0.9-dormant-forms.json")?;
        assert!(
            String::from_utf8(json.clone())?
                .trim_end()
                .ends_with(r#""flags":"#)
        );
        let result = parse_listing(&json);
        assert!(
            matches!(&result, Err(NftError::Parse(why)) if why.starts_with("not JSON: ")),
            "{result:?}"
        );
        // the versions that print it whole list the forms table, dormant
        for version in ["debian-13-1.1.3", "ubuntu-22.04-1.0.2"] {
            assert_eq!(
                listing(version, "dormant-forms")?,
                Listing {
                    dormant: true,
                    ..listing(REFERENCE, "forms")?
                },
                "{version}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_flags_key_of_any_type_on_our_table_is_dormant() -> TestResult {
        let document = |table: &str| {
            format!(r#"{{"nftables": [{{"metainfo": {{"version": "1.0.9"}}}}, {table}]}}"#)
        };
        for flags in [
            r#""dormant""#,
            r#""Old Doc Yak #3""#,
            r#"["dormant"]"#,
            "[]",
            "1",
            "null",
            "{}",
        ] {
            let json = document(&format!(
                r#"{{"table": {{"family": "inet", "name": "tn_p2p_allow", "handle": 13, "flags": {flags}}}}}"#
            ));
            assert!(parse_listing(json.as_bytes())?.dormant, "{flags}");
            assert!(parse_tables(json.as_bytes())?, "{flags}");
        }
        for table in [
            r#"{"table": {"family": "inet", "name": "tn_p2p_allow", "handle": 13}}"#,
            r#"{"table": {"family": "ip", "name": "tn_p2p_allow", "handle": 2, "flags": "dormant"}}"#,
            r#"{"table": {"family": "inet", "name": "filter", "handle": 3, "flags": "dormant"}}"#,
        ] {
            assert!(
                !parse_listing(document(table).as_bytes())?.dormant,
                "{table}"
            );
        }
        // a table object nft would never print is a parse error, as for every known kind
        let nameless = document(r#"{"table": {"family": "inet", "flags": "dormant"}}"#);
        assert!(matches!(
            parse_listing(nameless.as_bytes()),
            Err(NftError::Parse(_))
        ));
        Ok(())
    }

    #[test]
    fn a_table_of_our_name_in_another_family_is_not_ours() -> TestResult {
        let json =
            br#"{"nftables": [{"table": {"family": "ip", "name": "tn_p2p_allow", "handle": 1}}]}"#;
        assert!(!parse_tables(json)?);
        Ok(())
    }

    #[test]
    fn unknown_objects_and_other_tables_are_skipped() -> TestResult {
        let json = br#"{"nftables": [
            {"metainfo": {"version": "9.9.9", "json_schema_version": 2}},
            "not an object",
            {"map": {"family": "inet", "table": "tn_p2p_allow", "name": "m", "elem": [[1, 2]]}},
            {"flowtable": {"family": "inet", "table": "tn_p2p_allow", "name": "f"}},
            {"set": {"family": "inet", "table": "tn_p2p_allow", "name": "allow_v4", "elem": ["192.0.2.10"]}},
            {"set": {"family": "inet", "table": "filter", "name": "allow_v6", "elem": ["2001:db8::1"]}},
            {"set": {"family": "ip", "table": "tn_p2p_allow", "name": "static_v4"}},
            {"counter": {"family": "inet", "table": "filter", "name": "tn_drop_p2p_unlisted", "packets": 1, "bytes": 2}},
            {"chain": {"family": "inet", "table": "tn_p2p_allow", "name": "output"}},
            {"chain": {"family": "inet", "table": "filter", "name": "input"}},
            {"rule": {"family": "inet", "table": "tn_p2p_allow", "chain": "output", "expr": []}},
            {"rule": {"family": "inet", "table": "filter", "chain": "input", "expr": []}},
            {"rule": {"family": "inet", "table": "tn_p2p_allow", "chain": "input", "expr": [{"new": "thing"}]}}
        ]}"#;
        let mut expected = Listing::default();
        expected
            .sets
            .insert("allow_v4".to_owned(), vec![ip4(192, 0, 2, 10)]);
        expected.rules = vec![vec![Expr::Other]];
        assert_eq!(parse_listing(json)?, expected);
        Ok(())
    }

    #[test]
    fn sets_the_program_does_not_declare_are_skipped_whatever_they_hold() -> TestResult {
        // what nft prints for `set ports { type inet_service; elements = { 80 } }`, an ifname
        // set, a concat set and others an operator could add to the table by hand
        let json = br#"{"nftables": [
            {"set": {"family": "inet", "name": "allow_v4", "table": "tn_p2p_allow", "type": "ipv4_addr", "handle": 3, "elem": ["192.0.2.10"]}},
            {"set": {"family": "inet", "name": "ports", "table": "tn_p2p_allow", "type": "inet_service", "handle": 9, "elem": [80]}},
            {"set": {"family": "inet", "name": "links", "table": "tn_p2p_allow", "type": "ifname", "handle": 10, "elem": ["eth0", "lo"]}},
            {"set": {"family": "inet", "name": "pairs", "table": "tn_p2p_allow", "type": ["ipv4_addr", "inet_service"], "handle": 11, "elem": [{"concat": ["192.0.2.1", 80]}]}},
            {"set": {"family": "inet", "name": "odd", "table": "tn_p2p_allow", "type": "mark", "handle": 12, "elem": "not an array"}},
            {"set": {"family": "inet", "name": "allow_v4_old", "table": "tn_p2p_allow", "type": "ipv4_addr", "handle": 13, "elem": [null]}}
        ]}"#;
        let mut expected = Listing::default();
        expected
            .sets
            .insert("allow_v4".to_owned(), vec![ip4(192, 0, 2, 10)]);
        assert_eq!(parse_listing(json)?, expected);
        Ok(())
    }

    #[test]
    fn rule_expressions_parse_to_the_forms_of_the_program() -> TestResult {
        let dport = |right: &str| {
            format!(
                r#"{{"match": {{"op": "==", "left": {{"payload": {{"protocol": "udp", "field": "dport"}}}}, "right": {right}}}}}"#
            )
        };
        let source = |protocol: &str, right: &str| {
            format!(
                r#"{{"match": {{"op": "==", "left": {{"payload": {{"protocol": "{protocol}", "field": "saddr"}}}}, "right": {right}}}}}"#
            )
        };
        let read = [
            // nft lists one port as a number and more as an anonymous set
            (dport("49590"), Expr::UdpDport(BTreeSet::from([49590]))),
            (
                dport(r#"{"set": [49594, 49590]}"#),
                Expr::UdpDport(BTreeSet::from([49590, 49594])),
            ),
            (source("ip", r#""@allow_v4""#), saddr("ip", "allow_v4")),
            (source("ip6", r#""@static_v6""#), saddr("ip6", "static_v6")),
            (
                r#"{"counter": "tn_drop_p2p_unlisted"}"#.to_owned(),
                Expr::Counter("tn_drop_p2p_unlisted".to_owned()),
            ),
            (r#"{"accept": null}"#.to_owned(), Expr::Accept),
            (r#"{"drop": null}"#.to_owned(), Expr::Drop),
        ];
        for (json, expr) in read {
            assert_eq!(one_rule("", &json)?.rules, [vec![expr]], "{json}");
        }
        let other = [
            dport("70000"),
            dport(r#""49590""#),
            dport(r#"{"set": [49590, {"range": [49591, 49593]}]}"#),
            dport(r#"{"set": "49590"}"#),
            source("ip", r#""192.0.2.10""#),
            source("ip", r#"{"set": ["192.0.2.10"]}"#),
            source("ether", r#""@allow_v4""#),
            r#"{"match": {"op": "!=", "left": {"payload": {"protocol": "udp", "field": "dport"}}, "right": 49590}}"#.to_owned(),
            r#"{"match": {"op": "==", "left": {"payload": {"protocol": "tcp", "field": "dport"}}, "right": 49590}}"#.to_owned(),
            r#"{"match": {"op": "==", "left": {"payload": {"protocol": "ip", "field": "daddr"}}, "right": "@allow_v4"}}"#.to_owned(),
            r#"{"match": {"op": "==", "left": {"meta": {"key": "iifname"}}, "right": "lo"}}"#.to_owned(),
            r#"{"match": {"left": {"payload": {"protocol": "udp", "field": "dport"}}, "right": 49590}}"#.to_owned(),
            r#"{"counter": {"packets": 0, "bytes": 0}}"#.to_owned(),
            r#"{"counter": null}"#.to_owned(),
            r#"{"accept": 1}"#.to_owned(),
            r#"{"jump": {"target": "elsewhere"}}"#.to_owned(),
            r#"{"reject": null}"#.to_owned(),
            r#"{"log": {"prefix": "x"}}"#.to_owned(),
            r#"{"accept": null, "drop": null}"#.to_owned(),
            "{}".to_owned(),
            r#""accept""#.to_owned(),
            "null".to_owned(),
        ];
        for json in other {
            assert_eq!(one_rule("", &json)?.rules, [vec![Expr::Other]], "{json}");
        }
        // the rule keeps its expressions in order, unknown ones included
        let rule = one_rule(
            "",
            &format!(r#"{}, {{"notrack": null}}, {{"accept": null}}"#, dport("1")),
        )?;
        assert_eq!(
            rule.rules,
            [vec![
                Expr::UdpDport(BTreeSet::from([1])),
                Expr::Other,
                Expr::Accept
            ]]
        );
        Ok(())
    }

    #[test]
    fn the_chain_declaration_keeps_type_hook_priority_and_policy() -> TestResult {
        let declared = one_rule(
            r#", "type": "filter", "hook": "input", "prio": -20, "policy": "drop""#,
            "",
        )?;
        assert_eq!(
            declared.chain,
            Some(Chain {
                policy: Some("drop".to_owned()),
                ..program_chain()
            })
        );
        assert_eq!(declared.rules, [Vec::<Expr>::new()]);
        // a regular chain has no declaration, and a form the parser does not read is missing
        assert_eq!(one_rule("", "")?.chain, Some(Chain::default()));
        let odd = one_rule(
            r#", "type": 1, "hook": null, "prio": "-20", "policy": ["accept"]"#,
            "",
        )?;
        assert_eq!(odd.chain, Some(Chain::default()));
        Ok(())
    }

    #[test]
    fn a_malformed_known_object_is_a_parse_error() {
        for body in [
            r#"{"set": {"family": "inet", "table": "tn_p2p_allow"}}"#,
            r#"{"set": {"family": "inet", "table": "tn_p2p_allow", "name": "allow_v4", "elem": "192.0.2.10"}}"#,
            r#"{"set": {"family": "inet", "table": "tn_p2p_allow", "name": "static_v6", "elem": {}}}"#,
            r#"{"counter": {"family": "inet", "table": "tn_p2p_allow", "name": "c", "packets": -1, "bytes": 0}}"#,
            r#"{"counter": {"family": "inet", "table": "tn_p2p_allow", "name": "c", "bytes": 0}}"#,
            r#"{"chain": {"family": "inet", "name": "input"}}"#,
            r#"{"rule": {"family": "inet", "table": "tn_p2p_allow"}}"#,
            r#"{"rule": {"family": "inet", "table": "tn_p2p_allow", "chain": "input", "expr": {}}}"#,
        ] {
            let json = format!(r#"{{"nftables": [{body}]}}"#);
            let result = parse_listing(json.as_bytes());
            assert!(
                matches!(result, Err(NftError::Parse(_))),
                "{body}: {result:?}"
            );
        }
    }

    #[test]
    fn an_element_the_parser_cannot_read_is_a_parse_error() {
        for element in [
            "42",
            "null",
            r#""192.0.2.300""#,
            r#""2001:db8::/32""#,
            r#"{"concat": ["192.0.2.1", 80]}"#,
            r#"{"elem": {"val": {"elem": {"val": "192.0.2.1"}}}}"#,
            r#"{"elem": {"comment": "no val"}}"#,
            r#"{"prefix": {"addr": "192.0.2.0", "len": 33}}"#,
            r#"{"prefix": {"addr": "2001:db8::", "len": 129}}"#,
            r#"{"prefix": {"addr": "192.0.2.0", "len": 256}}"#,
            r#"{"prefix": {"addr": "192.0.2.0"}}"#,
            r#"{"range": ["192.0.2.1", "2001:db8::1"]}"#,
            r#"{"range": ["192.0.2.1", "192.0.2.2", "192.0.2.3"]}"#,
            r#"{"range": ["192.0.2.1"]}"#,
        ] {
            // the program's sets stay strict: a form the comparison cannot read fails the listing
            for set in SETS {
                let json = format!(
                    r#"{{"nftables": [{{"set": {{"family": "inet", "table": "tn_p2p_allow", "name": "{set}", "elem": ["192.0.2.10", {element}]}}}}]}}"#
                );
                let result = parse_listing(json.as_bytes());
                assert!(
                    matches!(&result, Err(NftError::Parse(why)) if why.starts_with(&format!("set {set}: "))),
                    "{set} {element}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn output_that_is_not_an_nft_document_is_a_parse_error() {
        for json in [
            "",
            "not json",
            "[]",
            "{}",
            r#"{"nftables": {}}"#,
            r#"{"nftables": null}"#,
        ] {
            for result in [
                parse_listing(json.as_bytes()).map(drop),
                parse_tables(json.as_bytes()).map(drop),
            ] {
                assert!(
                    matches!(result, Err(NftError::Parse(_))),
                    "{json:?}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn the_default_path_is_usr_sbin_nft() {
        assert_eq!(Nft::new(None).path(), Path::new("/usr/sbin/nft"));
        assert_eq!(DEFAULT_PATH, "/usr/sbin/nft");
        let other = Path::new("/opt/nft");
        assert_eq!(Nft::new(Some(other)).path(), other);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nft_gets_only_path_and_lc_all_and_a_null_stdin() -> TestResult {
        let output = fake().run(&os(&["argv", "two words", ""]), None)?;
        let stdout = String::from_utf8(output.stdout)?;
        let values = |key: &str| -> Vec<String> {
            stdout
                .lines()
                .filter_map(|line| line.strip_prefix(key))
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(values("arg="), ["argv", "two words", ""]);
        assert_eq!(values("stdin="), ["/dev/null"]);
        let mut env = values("env=");
        env.sort_unstable();
        assert_eq!(env, ["LC_ALL=C", "PATH=/usr/sbin:/usr/bin:/sbin:/bin"]);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn given_stdin_arrives_through_a_pipe() -> TestResult {
        let output = fake().run(&os(&["argv"]), Some(b""))?;
        let stdout = String::from_utf8(output.stdout)?;
        let stdin: Vec<&str> = stdout
            .lines()
            .filter_map(|line| line.strip_prefix("stdin="))
            .collect();
        assert!(
            matches!(stdin.as_slice(), [pipe] if pipe.starts_with("pipe:")),
            "{stdin:?}"
        );
        Ok(())
    }

    #[test]
    fn a_program_larger_than_a_pipe_reaches_nft_whole() -> TestResult {
        let text = "add element inet tn_p2p_allow allow_v4 { 192.0.2.10 }\n".repeat(8192);
        let output = fake().run(&os(&["-c", "-f", "-"]), Some(text.as_bytes()))?;
        assert_eq!(output.stdout.len(), text.len());
        assert!(output.stdout == text.as_bytes());
        fake().check_program(&text)?;
        Ok(())
    }

    #[test]
    fn the_listing_calls_parse_what_nft_prints() -> TestResult {
        let nft = fake();
        assert!(nft.table_present()?);
        assert_eq!(nft.list_table()?, program_listing());
        Ok(())
    }

    #[test]
    fn load_check_and_delete_succeed_when_nft_does() -> TestResult {
        let nft = fake();
        nft.load_file(&Path::new(FIXTURES).join("capture/program.nft"))?;
        nft.check_program("table inet tn_p2p_allow {\n}\n")?;
        nft.delete_table()?;
        Ok(())
    }

    #[test]
    fn a_failing_nft_reports_its_status_and_stderr() -> TestResult {
        let nft = fake();
        let missing = nft.load_file(Path::new("/nonexistent/current.nft"));
        assert_eq!(
            missing,
            Err(NftError::Failed {
                code: Some(1),
                stderr: "internal:0:0-0: Error: Could not open file \"/nonexistent/current.nft\": No such file or directory".to_owned(),
            })
        );
        let syntax = String::from_utf8(fixture("ubuntu-24.04-1.0.9-syntax-error.stderr")?)?;
        assert_eq!(
            nft.check_program("table inet tn_p2p_allow { bogus }\n"),
            Err(NftError::Failed {
                code: Some(1),
                stderr: syntax.trim_end().to_owned(),
            })
        );
        assert!(matches!(
            nft.check_program(""),
            Err(NftError::Failed { code: Some(1), .. })
        ));
        assert!(matches!(
            nft.run(&os(&["no", "such", "call"]), None),
            Err(NftError::Failed { code: Some(64), .. })
        ));
        Ok(())
    }

    #[test]
    fn a_signal_is_a_failure_without_a_status() {
        let result = fake().run(&os(&["kill"]), None);
        assert!(
            matches!(&result, Err(NftError::Failed { code: None, .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_missing_program_is_unavailable() {
        let path = Path::new(FIXTURES).join("no-such-nft");
        let result = Nft::new(Some(&path)).table_present();
        assert!(
            matches!(&result, Err(NftError::Unavailable(why)) if why.starts_with(&format!("{}: ", path.display()))),
            "{result:?}"
        );
    }

    #[test]
    fn a_program_that_may_not_be_executed_is_a_permission_error() {
        // a fixture without execute bits, which exec refuses with EACCES even to root
        let path = Path::new(FIXTURES).join("README.md");
        assert_eq!(
            Nft::new(Some(&path)).table_present(),
            Err(NftError::Permission)
        );
    }

    #[test]
    fn operation_not_permitted_is_a_permission_error() {
        assert_eq!(
            fake().run(&os(&["eperm"]), None).map(drop),
            Err(NftError::Permission)
        );
    }

    #[test]
    fn real_eperm_and_failure_stderr_classify_by_version() -> TestResult {
        for version in VERSIONS {
            let list = fixture(&format!("{version}-eperm-list.stderr"))?;
            assert_eq!(
                outcome(finished(1, &list), Ok(())).map(drop),
                Err(NftError::Permission),
                "{version}"
            );
            let absent = fixture(&format!("{version}-absent.stderr"))?;
            let result = outcome(finished(1, &absent), Ok(())).map(drop);
            assert!(
                matches!(&result, Err(NftError::Failed { code: Some(1), stderr }) if stderr.starts_with("Error: No such file or directory")),
                "{version}: {result:?}"
            );
        }
        for newer in ["debian-13-1.1.3", "ubuntu-24.04-1.0.9"] {
            let load = fixture(&format!("{newer}-eperm-load.stderr"))?;
            assert_eq!(
                outcome(finished(1, &load), Ok(())).map(drop),
                Err(NftError::Permission),
                "{newer}"
            );
        }
        // nft 1.0.2 refuses -f and -c without a word
        let older = fixture("ubuntu-22.04-1.0.2-eperm-load.stderr")?;
        assert!(older.is_empty());
        assert_eq!(
            outcome(finished(1, &older), Ok(())).map(drop),
            Err(NftError::Failed {
                code: Some(1),
                stderr: String::new(),
            })
        );
        // the text alone is not enough: only status 1 is nft's EPERM exit
        assert!(matches!(
            outcome(
                finished(3, b"Operation not permitted (you must be root)\n"),
                Ok(())
            ),
            Err(NftError::Failed { code: Some(3), .. })
        ));
        Ok(())
    }

    #[test]
    fn success_on_a_short_read_of_stdin_is_a_failure() {
        let broken = io::Error::from(io::ErrorKind::BrokenPipe);
        assert!(matches!(
            outcome(finished(0, b""), Err(broken)),
            Err(NftError::Failed { code: Some(0), .. })
        ));
        // when nft failed too, its own failure is the one reported
        let broken = io::Error::from(io::ErrorKind::BrokenPipe);
        assert_eq!(
            outcome(finished(1, b"Error: bad\n"), Err(broken)).map(drop),
            Err(NftError::Failed {
                code: Some(1),
                stderr: "Error: bad".to_owned(),
            })
        );
    }

    #[test]
    fn display_gives_one_line() {
        let failed = NftError::Failed {
            code: Some(1),
            stderr: "\n/dev/stdin:1:27-31: Error: syntax error, unexpected string\ntable inet tn_p2p_allow { bogus }\n    ^^^^^".to_owned(),
        };
        assert_eq!(
            failed.to_string(),
            "nft exited with status 1: /dev/stdin:1:27-31: Error: syntax error, unexpected string"
        );
        let killed = NftError::Failed {
            code: None,
            stderr: String::new(),
        };
        assert_eq!(killed.to_string(), "nft was killed by a signal");
        assert_eq!(
            NftError::Permission.to_string(),
            "nft: operation not permitted, run as root"
        );
        assert_eq!(
            NftError::Unavailable(
                "/usr/sbin/nft: No such file or directory (os error 2)".to_owned()
            )
            .to_string(),
            "nft unavailable: /usr/sbin/nft: No such file or directory (os error 2)"
        );
        assert_eq!(
            NftError::Parse("no nftables array".to_owned()).to_string(),
            "nft output not understood: no nftables array"
        );
    }
}
