//! Pins `docs/contract.md` to the binary: the parts of the contract the deployment scripts and the node team read are compared with what the code prints, writes, emits and logs.
//!
//! The contract is read with [`include_str!`] and taken apart by a few markdown helpers (a section under a heading, its first fenced block, the first column of its first table).
//! The usage text, the program, the status keys and the `--version` lines come from the built binary.
//! The reason tokens and the log events come from the source text, so a token or an event the code gains without a contract row fails here.
//!
//! A failure means the contract and the code disagree: fix whichever of the two is wrong, never this test alone.

// tests build fixtures with std::fs directly
#![allow(clippy::disallowed_methods)]

mod support;

use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use support::fake_rpc::{self, FakeRpc};
use support::{FakeNft, TempDir};
use tn_p2p_allow_sync::{cli, nft};

type TestResult = Result<(), Box<dyn Error>>;
type Res<T> = Result<T, Box<dyn Error>>;

/// The contract under test.
const CONTRACT: &str = include_str!("../docs/contract.md");

/// The binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_tn-p2p-allow-sync");

/// The golden file that pins the contract's program example.
const GOLDEN: &str = include_str!("golden/program-6.6.nft");

/// The epoch info a good sync asks for after the snapshot.
const EPOCH_INFO: &str = r#"{"epochId":42,"epochDuration":86400}"#;

/// Every source file, by path, for the scans of the reason tokens and the log events.
const SOURCES: [(&str, &str); 15] = [
    ("src/addr.rs", include_str!("../src/addr.rs")),
    ("src/check.rs", include_str!("../src/check.rs")),
    ("src/cli.rs", include_str!("../src/cli.rs")),
    ("src/config.rs", include_str!("../src/config.rs")),
    ("src/fsutil.rs", include_str!("../src/fsutil.rs")),
    ("src/lib.rs", include_str!("../src/lib.rs")),
    ("src/log.rs", include_str!("../src/log.rs")),
    ("src/main.rs", include_str!("../src/main.rs")),
    ("src/memory.rs", include_str!("../src/memory.rs")),
    ("src/nft.rs", include_str!("../src/nft.rs")),
    ("src/render.rs", include_str!("../src/render.rs")),
    ("src/rpc.rs", include_str!("../src/rpc.rs")),
    ("src/snapshot.rs", include_str!("../src/snapshot.rs")),
    ("src/status.rs", include_str!("../src/status.rs")),
    ("src/sync.rs", include_str!("../src/sync.rs")),
];

/// The functions that turn an error into its reason token, by file and by the source text that opens them.
///
/// `RpcError::token` is `self.to_string()`, so its tokens are in the `Display` impl; [`the_reason_tokens_are_the_ones_the_code_emits`] checks that it still delegates.
const TOKEN_FUNCTIONS: [(&str, &str); 5] = [
    ("src/fsutil.rs", "fn token(&self)"),
    ("src/config.rs", "fn token(&self)"),
    ("src/snapshot.rs", "fn token(&self)"),
    ("src/memory.rs", "fn token(&self)"),
    ("src/rpc.rs", "impl fmt::Display for RpcError"),
];

// ---- the contract ----

/// The lines under `heading`, a whole line such as `## Command line`, up to the next heading of the same or a higher level.
///
/// A line inside a fenced block is never a heading, since the program and the config examples start with `#`.
fn section(heading: &str) -> Res<Vec<&'static str>> {
    let level = heading_level(heading);
    let mut lines = CONTRACT.lines();
    lines
        .by_ref()
        .find(|line| *line == heading)
        .ok_or_else(|| format!("docs/contract.md has no heading {heading:?}"))?;
    let mut fenced = false;
    let mut body = Vec::new();
    for line in lines {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if !fenced && (1..=level).contains(&heading_level(line)) {
            break;
        }
        body.push(line);
    }
    Ok(body)
}

/// The number of `#` that make `line` a heading, 0 when it is none.
fn heading_level(line: &str) -> usize {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if line.chars().nth(hashes) == Some(' ') {
        hashes
    } else {
        0
    }
}

/// The body of the first fenced block in `lines`.
///
/// Every line of the body ends in `\n`, the last one included, and the fences are left out: that is the text a reader copies from the page, and the tool ends its own output the same way.
fn first_block(lines: &[&str]) -> Res<String> {
    let mut rest = lines.iter().skip_while(|line| !line.starts_with("```"));
    rest.next().ok_or("no fenced block in the section")?;
    let mut body = String::new();
    for line in rest {
        if *line == "```" {
            return Ok(body);
        }
        body.push_str(line);
        body.push('\n');
    }
    Err("the section's fenced block is not closed".into())
}

/// The first column of the first table in `lines`, without the header and separator rows, each cell trimmed.
fn first_column(lines: &[&'static str]) -> Res<Vec<&'static str>> {
    let mut table = lines
        .iter()
        .copied()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'));
    table.next().ok_or("no table in the section")?;
    let separator = table.next().unwrap_or_default();
    if !separator
        .chars()
        .all(|c| matches!(c, '|' | '-' | ':' | ' '))
    {
        return Err(format!("the table's second row is no separator: {separator:?}").into());
    }
    table
        .map(|row| {
            row.split('|')
                .nth(1)
                .map(str::trim)
                .ok_or_else(|| format!("a row without cells: {row:?}").into())
        })
        .collect()
}

/// The names a section's first table lists in its first column, in order; a cell such as `` `A`, `B` `` names two.
fn table_keys(heading: &str) -> Res<Vec<&'static str>> {
    Ok(first_column(&section(heading)?)?
        .into_iter()
        .flat_map(backticked)
        .collect())
}

/// Every span between a pair of backticks in `text`, in order.
fn backticked(text: &str) -> impl Iterator<Item = &str> {
    text.split('`').skip(1).step_by(2)
}

/// `text` without its parenthesised parts, so the backticked spans left are the ones the sentence lists.
fn outside_parentheses(text: &str) -> String {
    let mut depth = 0_usize;
    let mut outside = String::new();
    for c in text.chars() {
        match c {
            '(' => depth = depth.saturating_add(1),
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => outside.push(c),
            _ => {}
        }
    }
    outside
}

/// The first line of `lines` that starts with `start`, the rest of it after `start`.
fn line_after<'a>(lines: &[&'a str], start: &str) -> Res<&'a str> {
    lines
        .iter()
        .find_map(|line| line.strip_prefix(start))
        .ok_or_else(|| format!("no line starting with {start:?}").into())
}

/// The whole seconds `text` starts with, written `N s`.
fn seconds(text: &str) -> Res<u64> {
    let (number, _) = text
        .split_once(" s")
        .ok_or_else(|| format!("no `N s` at the start of {text:?}"))?;
    Ok(number.parse()?)
}

/// Placeholders written as `<code>` in the contract or `{code}` in a format string, as `<>`, so a token pattern compares across the two.
fn pattern(token: &str) -> String {
    let mut out = String::new();
    let mut close = None;
    for c in token.chars() {
        match (close, c) {
            (None, '<') => close = Some('>'),
            (None, '{') => close = Some('}'),
            (None, c) => out.push(c),
            (Some(end), c) if c == end => {
                out.push_str("<>");
                close = None;
            }
            (Some(_), _) => {}
        }
    }
    out
}

// ---- the source text ----

/// One token of Rust source, as far as the scans need: string literals, words and single punctuation characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    /// A string literal's text between its quotes, escapes as written.
    Str(&'a str),
    /// An identifier, a keyword or a number.
    Word(&'a str),
    /// Any other character.
    Punct(char),
}

/// The part of a source file before its `#[cfg(test)] mod tests`, the code the binary is built from.
fn non_test(src: &str) -> &str {
    src.split_once("#[cfg(test)]\nmod tests")
        .map_or(src, |(code, _)| code)
}

/// The tokens of `src`, dropping whitespace, `//` comments and char literals.
///
/// Enough of Rust for this crate: its sources hold no block comment and no raw string with a `"` in it.
fn lex(src: &str) -> Vec<Tok<'_>> {
    let mut toks = Vec::new();
    let mut rest = src.trim_start();
    while let Some(c) = rest.chars().next() {
        let tail = rest.get(c.len_utf8()..).unwrap_or_default();
        let next = if rest.starts_with("//") {
            tail.split_once('\n').map_or("", |(_, next)| next)
        } else if c == '"' {
            let (text, after) = string_literal(tail);
            toks.push(Tok::Str(text));
            after
        } else if c == '\'' {
            match char_literal(tail) {
                Some(after) => after,
                // a lifetime
                None => {
                    toks.push(Tok::Punct(c));
                    tail
                }
            }
        } else if is_word_char(c) {
            let end = rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len());
            let (word, after) = rest.split_at_checked(end).unwrap_or((rest, ""));
            toks.push(Tok::Word(word));
            after
        } else {
            toks.push(Tok::Punct(c));
            tail
        };
        rest = next.trim_start();
    }
    toks
}

/// Whether `c` belongs to an identifier, a keyword or a number.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A string literal whose opening `"` came just before `tail`: its text and what follows its closing `"`.
fn string_literal(tail: &str) -> (&str, &str) {
    let mut chars = tail.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => {
                let (text, after) = tail.split_at_checked(at).unwrap_or((tail, ""));
                return (text, after.get(1..).unwrap_or_default());
            }
            _ => {}
        }
    }
    (tail, "")
}

/// What follows a char literal whose opening `'` came just before `tail`, or `None` when the `'` starts a lifetime.
fn char_literal(tail: &str) -> Option<&str> {
    let mut chars = tail.chars();
    if chars.next()? == '\\' {
        // the escaped char, then up to the closing quote, which `\u{…}` and `\x41` run on to
        chars.next()?;
        return chars.as_str().split_once('\'').map(|(_, after)| after);
    }
    chars.as_str().strip_prefix('\'')
}

/// The tokens of the non-test code of the source file at `path`.
fn source(path: &str) -> Res<Vec<Tok<'static>>> {
    SOURCES
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(_, text)| lex(non_test(text)))
        .ok_or_else(|| format!("{path} is not in SOURCES").into())
}

/// The tokens between the braces of the first block after the tokens of `start`, which open a function or an impl.
fn block_after<'a>(toks: &[Tok<'a>], start: &str) -> Res<Vec<Tok<'a>>> {
    let start = lex(start);
    let from = toks
        .windows(start.len())
        .position(|window| window == start.as_slice())
        .ok_or_else(|| format!("{start:?} not found"))?;
    let mut depth = 0_usize;
    let mut body = Vec::new();
    let opened = toks
        .get(from..)
        .unwrap_or_default()
        .iter()
        .skip_while(|tok| **tok != Tok::Punct('{'));
    for tok in opened {
        match tok {
            Tok::Punct('{') => {
                if depth > 0 {
                    body.push(*tok);
                }
                depth = depth.saturating_add(1);
            }
            Tok::Punct('}') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(body);
                }
                body.push(*tok);
            }
            _ => body.push(*tok),
        }
    }
    Err(format!("the block after {start:?} is not closed").into())
}

/// The string literals among `toks`.
fn literals<'a>(toks: &[Tok<'a>]) -> impl Iterator<Item = &'a str> {
    toks.iter().filter_map(|tok| match tok {
        Tok::Str(text) => Some(*text),
        _ => None,
    })
}

/// The reasons the call sites in `toks` hand to the log or the status file as a literal, or as a `const` of the same file.
///
/// The call sites are `("reason", R)` fields, `untouched(log, R, …)` and `error(log, R, …)` (the closing error lines of `sync` and `check`), `Failure::new(R, …)` (a local error), `.stale(R)` and `refused(R, …)`.
/// A reason computed at run time, such as `e.token()`, comes from a token function and is gathered there.
fn reason_arguments<'a>(toks: &[Tok<'a>]) -> Res<Vec<&'a str>> {
    let mut reasons = Vec::new();
    for at in 0..toks.len() {
        let argument = match toks.get(at..).unwrap_or_default() {
            [
                Tok::Punct('('),
                Tok::Str("reason"),
                Tok::Punct(','),
                arg,
                ..,
            ]
            | [
                Tok::Word("untouched" | "error"),
                Tok::Punct('('),
                Tok::Word("log"),
                Tok::Punct(','),
                arg,
                ..,
            ]
            | [
                Tok::Word("Failure"),
                Tok::Punct(':'),
                Tok::Punct(':'),
                Tok::Word("new"),
                Tok::Punct('('),
                arg,
                ..,
            ]
            | [
                Tok::Punct('.'),
                Tok::Word("stale"),
                Tok::Punct('('),
                arg,
                ..,
            ]
            | [Tok::Word("refused"), Tok::Punct('('), arg, ..] => *arg,
            _ => continue,
        };
        match argument {
            Tok::Str(text) => reasons.push(text),
            Tok::Word(name) if name.chars().all(|c| c.is_ascii_uppercase() || c == '_') => {
                reasons.push(const_value(toks, name)?);
            }
            _ => {}
        }
    }
    Ok(reasons)
}

/// The value of `const NAME: &str = "…";` in `toks`.
fn const_value<'a>(toks: &[Tok<'a>], name: &str) -> Res<&'a str> {
    toks.windows(7)
        .find_map(|window| match window {
            [
                Tok::Word("const"),
                Tok::Word(found),
                Tok::Punct(':'),
                Tok::Punct('&'),
                Tok::Word("str"),
                Tok::Punct('='),
                Tok::Str(value),
            ] if *found == name => Some(*value),
            _ => None,
        })
        .ok_or_else(|| format!("no `const {name}: &str` beside its use as a reason").into())
}

/// The event names the call sites in `toks` log: `Priority::X, "event"` (the arguments of `Log::line` and `Log::last`, and the `(priority, event)` pairs) and `event: "event"` (a closing `Line` built ahead).
fn logged_events<'a>(toks: &[Tok<'a>]) -> Vec<&'a str> {
    let mut events = Vec::new();
    for at in 0..toks.len() {
        match toks.get(at..).unwrap_or_default() {
            [
                Tok::Word("Priority"),
                Tok::Punct(':'),
                Tok::Punct(':'),
                Tok::Word(_),
                Tok::Punct(','),
                Tok::Str(event),
                ..,
            ]
            | [Tok::Word("event"), Tok::Punct(':'), Tok::Str(event), ..] => events.push(*event),
            _ => {}
        }
    }
    events
}

/// A set difference as one comma-separated line, for a failure message.
fn listed(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        "none".to_owned()
    } else {
        set.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

// ---- the binary ----

/// What one run of the binary did.
#[derive(Debug)]
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs the binary with `args` and no stdin, outside the journal.
fn run<S: AsRef<OsStr>>(args: &[S]) -> Res<Ran> {
    // the binary under test is the one program these tests start
    #[allow(clippy::disallowed_methods)]
    let mut command = Command::new(BIN);
    let output = command
        .args(args)
        .env_remove("JOURNAL_STREAM")
        .stdin(Stdio::null())
        .output()?;
    Ok(Ran {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    })
}

/// A state dir after one good `sync` against a fake node and a fake nft.
struct Bed {
    _temp: TempDir,
    state: PathBuf,
    nft: FakeNft,
    _rpc: FakeRpc,
}

impl Bed {
    /// A state dir holding a config, synced once from an authoritative snapshot of one key at epoch 42.
    fn synced() -> Res<Self> {
        let temp = TempDir::new("contract-pin")?;
        let state = temp.join("state");
        fs::create_dir(&state)?;
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
        let nft = FakeNft::install(temp.path())?;
        let rpc = FakeRpc::start(vec![
            fake_rpc::result(&snapshot().to_string()),
            fake_rpc::result(EPOCH_INFO),
        ])?;
        let config = state.join("config");
        fs::write(
            &config,
            format!(
                "CONFIG_VERSION=1\nRPC_PORT={}\nP2P_PORTS=49590,49594\nP2P_ALLOW_FROM=\n",
                rpc.port()
            ),
        )?;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600))?;
        let bed = Self {
            _temp: temp,
            state,
            nft,
            _rpc: rpc,
        };
        let ran = bed.run(&["sync"])?;
        if ran.code == Some(0) && ran.stdout.starts_with("result=applied ") {
            Ok(bed)
        } else {
            Err(format!("sync did not apply: {ran:?}").into())
        }
    }

    /// Runs the binary with `--state-dir`, the fake nft and `args`.
    fn run(&self, args: &[&str]) -> Res<Ran> {
        let mut argv: Vec<OsString> = vec![
            "--state-dir".into(),
            self.state.clone().into(),
            "--nft".into(),
            self.nft.path().into(),
        ];
        argv.extend(args.iter().map(OsString::from));
        run(&argv)
    }
}

/// An authoritative snapshot at epoch 42 whose primary lists one current key at 203.0.113.10.
fn snapshot() -> Value {
    json!({"version": 1, "epoch": 42, "swarms": [{
        "network": "primary",
        "worker_id": null,
        "available": true,
        "tracked_epoch": 42,
        "admission": {
            "configured": "Closed", "effective": "Closed", "epoch": 42, "fallback": null,
            "resolved_current": 4, "required_current": 3, "connected_current": 3, "contradicted": 0,
        },
        "entries": [{
            "bls_pubkey": format!("0x11{}", "0".repeat(190)),
            "slots": ["current", "eligible"],
            "peer_id": null,
            "network_pubkey": null,
            "multiaddrs": ["/ip4/203.0.113.10/udp/49590/quic-v1"],
            "record_timestamp": 1_793_750_400_u64,
            "source": "record",
            "admitted": true,
        }],
    }]})
}

/// The keys of `KEY=VALUE` lines, in order.
fn line_keys(text: &str) -> Vec<&str> {
    text.lines()
        .map(|line| line.split_once('=').map_or(line, |(key, _)| key))
        .collect()
}

/// The top-level keys of the JSON object `text`, in the order the text lists them.
///
/// `serde_json::Map` sorts its keys (the crate is built without `preserve_order`), so the order is read with a visitor of its own.
fn json_keys(text: &str) -> Res<Vec<String>> {
    struct Keys;

    impl<'de> serde::de::Visitor<'de> for Keys {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JSON object")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut keys = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                map.next_value::<serde::de::IgnoredAny>()?;
                keys.push(key);
            }
            Ok(keys)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(text);
    let keys = serde::Deserializer::deserialize_map(&mut deserializer, Keys)?;
    deserializer.end()?;
    Ok(keys)
}

/// Whether `version` is a semantic version, `MAJOR.MINOR.PATCH` with an optional `-pre` and `+build`.
fn is_semver(version: &str) -> bool {
    let (version, build) = version
        .split_once('+')
        .map_or((version, None), |(version, build)| (version, Some(build)));
    let (core, pre) = version
        .split_once('-')
        .map_or((version, None), |(core, pre)| (core, Some(pre)));
    let numeric = |id: &str| {
        !id.is_empty()
            && id.chars().all(|c| c.is_ascii_digit())
            && (id == "0" || !id.starts_with('0'))
    };
    let identifier =
        |id: &str| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    core.split('.').count() == 3
        && core.split('.').all(numeric)
        && pre.is_none_or(|pre| {
            pre.split('.').all(|id| {
                identifier(id) && (numeric(id) || !id.chars().all(|c| c.is_ascii_digit()))
            })
        })
        && build.is_none_or(|build| build.split('.').all(identifier))
}

/// Whether `text` is a full commit hash: 40 lowercase hex digits.
fn is_commit_hash(text: &str) -> bool {
    text.len() == 40 && text.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// The triple of the target this test, and so the binary beside it, was built for, spelled as the `target` line spells it.
fn this_triple() -> String {
    let vendor = if cfg!(target_vendor = "unknown") {
        "unknown"
    } else {
        "other"
    };
    let env = if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(target_env = "gnu") {
        "gnu"
    } else {
        "other"
    };
    format!(
        "{}-{vendor}-{}-{env}",
        std::env::consts::ARCH,
        std::env::consts::OS
    )
}

// ---- the tests ----

/// The usage block of §Command line is what `--help` prints on stderr, byte for byte, with exit 2 and nothing on stdout.
///
/// The block's last line ends in `\n` (see [`first_block`]), as `USAGE` does.
#[test]
fn the_usage_block_is_the_help_text() -> TestResult {
    let usage = first_block(&section("## Command line")?)?;
    let ran = run(&["--help"])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, "", "{ran:?}");
    assert_eq!(ran.stderr, usage, "the usage block of docs/contract.md");
    Ok(())
}

/// The program block of §The program is `tests/golden/program-6.6.nft` byte for byte; `render_golden` pins the golden to `render`.
///
/// The contract names no snapshot fixture for its example, so the golden is the only text it is compared with.
#[test]
fn the_program_block_is_the_golden_program() -> TestResult {
    let program = first_block(&section("## The program")?)?;
    assert_eq!(program, GOLDEN, "the program block of docs/contract.md");
    Ok(())
}

/// The status file's keys, the `status` lines and the `status --json` keys are in the order of the contract's tables, as the binary writes them after a good sync.
#[test]
fn the_status_keys_are_in_the_contract_order() -> TestResult {
    let bed = Bed::synced()?;
    let file_keys = table_keys("### The status file")?;
    let live_keys = table_keys("### `status`")?;
    let json_table = table_keys("### `status --json`")?;

    let file = fs::read_to_string(bed.state.join("status"))?;
    assert_eq!(line_keys(&file), file_keys, "the status file");

    let ran = bed.run(&["status"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    let shown: Vec<&str> = file_keys.iter().chain(&live_keys).copied().collect();
    assert_eq!(line_keys(&ran.stdout), shown, "the lines of `status`");

    let ran = bed.run(&["status", "--json"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        json_keys(&ran.stdout)?,
        json_table,
        "the keys of `status --json`"
    );
    Ok(())
}

/// The reason tokens of §Reason tokens, the table and the log-only sentence, are exactly the tokens the code emits.
///
/// The code's tokens are the literals of the token functions ([`TOKEN_FUNCTIONS`]) and the literal reasons of the call sites ([`reason_arguments`]), in the non-test code.
/// `rpc-http-<code>`, `rpc-error-<code>` and `snapshot-version-<n>` compare as patterns (see [`pattern`]).
#[test]
fn the_reason_tokens_are_the_ones_the_code_emits() -> TestResult {
    let tokens = section("### Reason tokens")?;
    let log_only = line_after(&tokens, "Log-only tokens, never written to `status`: ")?;
    let doc: BTreeSet<String> = first_column(&tokens)?
        .into_iter()
        .flat_map(backticked)
        .map(pattern)
        .chain(backticked(&outside_parentheses(log_only)).map(pattern))
        .collect();

    let rpc_token = block_after(&source("src/rpc.rs")?, "pub fn token(&self) -> String")?;
    assert_eq!(
        rpc_token,
        lex("self.to_string()"),
        "RpcError::token no longer delegates to Display; scan its tokens where they are now"
    );
    let mut code = BTreeSet::new();
    for (path, start) in TOKEN_FUNCTIONS {
        let body = block_after(&source(path)?, start)?;
        let found: Vec<String> = literals(&body).map(pattern).collect();
        assert!(!found.is_empty(), "{path}: `{start}` holds no token");
        code.extend(found);
    }
    for (path, _) in SOURCES {
        code.extend(reason_arguments(&source(path)?)?.into_iter().map(pattern));
    }

    let missing_in_code: BTreeSet<String> = doc.difference(&code).cloned().collect();
    let missing_in_doc: BTreeSet<String> = code.difference(&doc).cloned().collect();
    assert!(
        missing_in_code.is_empty() && missing_in_doc.is_empty(),
        "reason tokens differ\n  in docs/contract.md but not emitted by the code (or emitted in a way the scan misses): {}\n  emitted by the code but not in docs/contract.md: {}",
        listed(&missing_in_code),
        listed(&missing_in_doc),
    );
    Ok(())
}

/// The events of §Log lines are exactly the events the code logs.
///
/// The code's events are the `Priority::X, "event"` and `event: "event"` literals of the non-test code ([`logged_events`]), and the one line `Log` writes itself, `log-capped`, whose format string starts with the priority prefix's `{}`.
#[test]
fn the_log_events_are_the_ones_the_code_logs() -> TestResult {
    let events = line_after(&section("### Log lines")?, "- The events: ")?;
    let doc: BTreeSet<String> = backticked(events)
        .filter_map(|event| event.split_whitespace().next())
        .map(str::to_owned)
        .collect();

    let mut code: BTreeSet<String> = BTreeSet::new();
    for (path, _) in SOURCES {
        code.extend(logged_events(&source(path)?).into_iter().map(str::to_owned));
    }
    let own: Vec<&str> = literals(&source("src/log.rs")?)
        .filter_map(|text| text.strip_prefix("{}"))
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(own, ["log-capped"], "the lines src/log.rs formats itself");
    code.extend(own.into_iter().map(str::to_owned));

    let missing_in_code: BTreeSet<String> = doc.difference(&code).cloned().collect();
    let missing_in_doc: BTreeSet<String> = code.difference(&doc).cloned().collect();
    assert!(
        missing_in_code.is_empty() && missing_in_doc.is_empty(),
        "log events differ\n  in docs/contract.md but not logged by the code (or logged in a way the scan misses): {}\n  logged by the code but not in docs/contract.md: {}",
        listed(&missing_in_code),
        listed(&missing_in_doc),
    );
    Ok(())
}

/// The five `--version` lines have the contract's shape.
///
/// Line 1 is compared by shape: the contract's example carries the version of the coming release while `Cargo.toml` moves to it only at release prep, so both are checked to be semantic versions and the binary's to be the `Cargo.toml` one.
/// Line 2 is a commit hash or `unknown`, line 3 the triple of this build when the contract lists it and `unknown` otherwise, and lines 4 and 5 are the example's.
#[test]
fn the_version_lines_have_the_contract_shape() -> TestResult {
    let version = section("### `--version`")?;
    let example = first_block(&version)?;
    let triples: BTreeSet<&str> = backticked(line_after(&version, "- Line 3 is ")?).collect();
    let ran = run(&["--version"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stderr, "", "{ran:?}");
    assert!(ran.stdout.ends_with('\n'), "{ran:?}");

    let doc: Vec<&str> = example.lines().collect();
    let bin: Vec<&str> = ran.stdout.lines().collect();
    let ([doc1, doc2, doc3, doc4, doc5], [bin1, bin2, bin3, bin4, bin5]) =
        (doc.as_slice(), bin.as_slice())
    else {
        return Err(
            format!("not five lines each: the contract {doc:?}, the binary {bin:?}").into(),
        );
    };

    let (name, doc_version) = doc1.split_once(' ').ok_or("line 1 has no space")?;
    assert_eq!(name, env!("CARGO_PKG_NAME"), "line 1 of the contract");
    assert!(
        is_semver(doc_version),
        "line 1 of the contract: {doc_version:?}"
    );
    assert!(
        is_semver(env!("CARGO_PKG_VERSION")),
        "the Cargo.toml version"
    );
    assert_eq!(
        *bin1,
        format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        "line 1 of the binary"
    );

    let doc_commit = doc2
        .strip_prefix("commit ")
        .ok_or("line 2 of the contract")?;
    assert!(
        is_commit_hash(doc_commit),
        "line 2 of the contract: {doc2:?}"
    );
    let bin_commit = bin2.strip_prefix("commit ").ok_or("line 2 of the binary")?;
    assert!(
        is_commit_hash(bin_commit) || bin_commit == "unknown",
        "line 2 of the binary: {bin2:?}"
    );

    assert!(
        triples.contains("unknown"),
        "the line 3 bullet: {triples:?}"
    );
    let doc_target = doc3
        .strip_prefix("target ")
        .ok_or("line 3 of the contract")?;
    assert!(
        triples.contains(doc_target),
        "line 3 of the contract: {doc3:?}"
    );
    let this = this_triple();
    let expected = if triples.contains(this.as_str()) {
        this.as_str()
    } else {
        "unknown"
    };
    assert_eq!(
        bin3.strip_prefix("target "),
        Some(expected),
        "line 3 of the binary"
    );

    assert_eq!(bin4, doc4, "line 4");
    assert_eq!(bin5, doc5, "line 5");
    Ok(())
}

/// The defaults the contract states in prose are the code's constants: the state dir, the nft path and the capture-floor wait.
#[test]
fn the_defaults_are_the_contract_ones() -> TestResult {
    let command_line = section("## Command line")?;
    let state_dir = line_after(&command_line, "- `--state-dir` defaults to ")?;
    assert_eq!(
        backticked(state_dir).next(),
        Some(cli::DEFAULT_STATE_DIR),
        "the --state-dir default"
    );
    let nft_path = line_after(&command_line, "- `--nft` defaults to ")?;
    assert_eq!(
        backticked(nft_path).next(),
        Some(nft::DEFAULT_PATH),
        "the --nft default"
    );

    let wait = cli::CAPTURE_FLOOR_WAIT;
    assert_eq!(wait.subsec_nanos(), 0, "{wait:?}");
    let lock = line_after(&section("### The lock")?, "- `capture-floor` waits up to ")?;
    assert_eq!(seconds(lock)?, wait.as_secs(), "§The lock");
    let floor = section("### The floor file")?;
    let fixed = floor
        .iter()
        .find_map(|line| line.split_once("with a fixed ").map(|(_, rest)| rest))
        .ok_or("§The floor file states no fixed wait")?;
    assert_eq!(seconds(fixed)?, wait.as_secs(), "§The floor file");
    Ok(())
}
