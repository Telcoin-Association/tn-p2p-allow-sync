//! Log lines on stderr: one line per event, the event name followed by `key=value` fields, at most [`MAX_LINES`] per run.
//!
//! Under systemd each line starts with a `<3>` (error) or `<4>` (warning) prefix, which journald reads as the line's priority.
//! The prefix appears only when `JOURNAL_STREAM` names the device and inode of fd 2, systemd's documented check, so interactive output stays plain.
//!
//! A value that is not plain printable ASCII is written in Rust's quoted, escaped form, so every event stays on one line whatever it carries.
//!
//! A run's closing line, the one that says how it ended, is logged with [`Log::last`]: it is never dropped and is always the last line written.
//! The other lines share the first [`MAX_LINES`]` - 1` slots; once more of them come than fit, the last of those slots is spent on one `log-capped dropped=N` line instead.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;

/// The most lines one run writes to stderr, the `log-capped` line and the closing line included.
pub const MAX_LINES: usize = 8;

/// The slots of the lines before the closing line, the `log-capped` line included.
const ORDINARY_LINES: usize = MAX_LINES.saturating_sub(1);

/// Ordinary lines written as they come; the one after them is held until it is known whether the cap is reached.
const IMMEDIATE_LINES: usize = ORDINARY_LINES.saturating_sub(1);

/// The variable systemd sets to `<device>:<inode>` of the journal stream it connected to stdout and stderr.
const JOURNAL_STREAM: &str = "JOURNAL_STREAM";

/// fd 2 as the kernel shows it; its metadata is that of the stream it points at.
const FD2: &str = "/proc/self/fd/2";

/// How serious a log line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// The run failed: syslog priority 3.
    Error,
    /// The run went on, or kept the last good program, but something needs a look: syslog priority 4.
    Warning,
}

impl Priority {
    /// The journald prefix of this priority: `<3>` or `<4>`.
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Error => "<3>",
            Self::Warning => "<4>",
        }
    }
}

/// The prefix a line of `priority` starts with: [`Priority::prefix`] when stderr is the journal, nothing otherwise.
pub const fn prefix_for(priority: Priority, journal: bool) -> &'static str {
    if journal { priority.prefix() } else { "" }
}

/// Whether `stream`, the value of `JOURNAL_STREAM`, names `fd2`, the `(device, inode)` of fd 2.
///
/// The value is two decimal numbers joined by `:`; anything else, or a missing value or inode, is no match.
pub fn journal_matches(stream: Option<&str>, fd2: Option<(u64, u64)>) -> bool {
    let (Some(stream), Some((dev, ino))) = (stream, fd2) else {
        return false;
    };
    let Some((stream_dev, stream_ino)) = stream.split_once(':') else {
        return false;
    };
    decimal(stream_dev) == Some(dev) && decimal(stream_ino) == Some(ino)
}

/// A number written with decimal digits only.
fn decimal(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Whether stderr is the journal: `JOURNAL_STREAM` matches fd 2, as [`journal_matches`] checks.
pub fn stderr_is_journal() -> bool {
    // the only read of the environment in the crate: systemd's way of saying that stderr is
    // the journal; nothing else the environment holds is trusted
    #[allow(clippy::disallowed_methods)]
    let stream = std::env::var(JOURNAL_STREAM).ok();
    if stream.is_none() {
        return false;
    }
    let fd2 = fs::metadata(FD2).ok().map(|meta| (meta.dev(), meta.ino()));
    journal_matches(stream.as_deref(), fd2)
}

/// A log line kept as its parts, for a caller that learns only later whether it closes the run.
#[derive(Debug, Clone, Copy)]
pub struct Line<'a> {
    /// How serious the line is.
    pub priority: Priority,
    /// The event name the line starts with.
    pub event: &'a str,
    /// The `key=value` fields after the event, in order.
    pub fields: &'a [(&'a str, &'a str)],
}

/// The log of one run, written to `out`.
///
/// Lines logged with [`Log::line`] or [`Log::plain`] share the first [`MAX_LINES`]` - 1` slots, and all but the last of those slots are written as the lines come.
/// The line after them is held: if the run logs no other such line, it is written when the log is finished; if it does, it and every later one are dropped, and that slot says how many went.
/// The closing line, logged with [`Log::last`] or [`Log::last_plain`], is held until the log is finished and then written after all of them, so it is never dropped and always last.
/// A run has one closing line; a second one is a bug and is logged as an ordinary line instead, written in a free slot or dropped and counted, so the first stays last.
/// Finishing happens on [`Log::finish`] or on drop, whichever comes first.
#[derive(Debug)]
pub struct Log<W: Write> {
    out: W,
    journal: bool,
    written: usize,
    held: Option<String>,
    dropped: u64,
    closing: Option<String>,
    finished: bool,
}

impl Log<io::Stderr> {
    /// The log of this process: stderr, with priority prefixes when [`stderr_is_journal`].
    pub fn stderr() -> Self {
        Self::new(io::stderr(), stderr_is_journal())
    }
}

impl<W: Write> Log<W> {
    /// A log writing to `out`, with priority prefixes when `journal` is true.
    pub fn new(out: W, journal: bool) -> Self {
        Self {
            out,
            journal,
            written: 0,
            held: None,
            dropped: 0,
            closing: None,
            finished: false,
        }
    }

    /// Logs one line: the prefix, `event`, then ` key=value` for each field in order.
    ///
    /// An empty value is written as `key=`; a value that holds anything but printable ASCII other than `"` and `\` is written quoted and escaped.
    pub fn line(&mut self, priority: Priority, event: &str, fields: &[(&str, &str)]) {
        let text = self.format(priority, event, fields);
        self.emit(text);
    }

    /// Logs `text` as it is, with no prefix: a summary line the caller formatted, which must not hold a newline.
    pub fn plain(&mut self, text: &str) {
        self.emit(text.to_owned());
    }

    /// Logs the run's closing line, formatted as [`Log::line`] formats one: it is written when the log is finished, after every other line, and is never dropped.
    pub fn last(&mut self, priority: Priority, event: &str, fields: &[(&str, &str)]) {
        let text = self.format(priority, event, fields);
        self.close_with(text);
    }

    /// Logs `text` as it is, with no prefix, as the run's closing line: a summary line the caller formatted, which must not hold a newline.
    pub fn last_plain(&mut self, text: &str) {
        self.close_with(text.to_owned());
    }

    /// Writes the held line, or the `log-capped` line when lines were dropped, then the closing line, and flushes; later lines are dropped.
    pub fn finish(mut self) {
        self.close();
    }

    fn format(&self, priority: Priority, event: &str, fields: &[(&str, &str)]) -> String {
        let mut text = String::from(prefix_for(priority, self.journal));
        text.push_str(event);
        for (key, value) in fields {
            text.push(' ');
            text.push_str(key);
            text.push('=');
            push_value(&mut text, value);
        }
        text
    }

    fn close_with(&mut self, text: String) {
        if self.finished || self.closing.is_some() {
            // a second closing line is a bug; as an ordinary line it cannot take the first one's place
            self.emit(text);
        } else {
            self.closing = Some(text);
        }
    }

    fn emit(&mut self, text: String) {
        if self.finished || self.dropped > 0 {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        if self.written < IMMEDIATE_LINES {
            self.write(&text);
            self.written = self.written.saturating_add(1);
            return;
        }
        // the last ordinary slot: held until the run logs another ordinary line or ends
        if self.held.take().is_some() {
            self.dropped = 2;
        } else {
            self.held = Some(text);
        }
    }

    fn close(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.dropped > 0 {
            let text = format!(
                "{}log-capped dropped={}",
                prefix_for(Priority::Warning, self.journal),
                self.dropped
            );
            self.write(&text);
        } else if let Some(text) = self.held.take() {
            self.write(&text);
        }
        if let Some(text) = self.closing.take() {
            self.write(&text);
        }
        // nothing is left to report a failed flush to
        let _ = self.out.flush();
    }

    fn write(&mut self, text: &str) {
        // nothing is left to report a failed write to
        let _ = writeln!(self.out, "{text}");
    }
}

impl<W: Write> Drop for Log<W> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Appends `value` bare when it is printable ASCII without `"` or `\`, and quoted with Rust's escapes otherwise.
fn push_value(text: &mut String, value: &str) {
    let bare = value
        .bytes()
        .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
    if bare {
        text.push_str(value);
    } else {
        // writing to a String cannot fail
        let _ = write!(text, "{value:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logged(journal: bool, lines: usize) -> String {
        let mut out = Vec::new();
        let mut log = Log::new(&mut out, journal);
        for n in 0..lines {
            log.line(Priority::Warning, "event", &[("n", &n.to_string())]);
        }
        drop(log);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// `lines` ordinary lines, then the closing line `stale reason=r`, finished by drop.
    fn closed(lines: usize) -> String {
        let mut out = Vec::new();
        let mut log = Log::new(&mut out, false);
        for n in 0..lines {
            log.line(Priority::Warning, "event", &[("n", &n.to_string())]);
        }
        log.last(Priority::Warning, "stale", &[("reason", "r")]);
        drop(log);
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn journal_stream_must_name_fd2_exactly() {
        assert!(journal_matches(Some("8:12345"), Some((8, 12345))));
        assert!(journal_matches(Some("0:0"), Some((0, 0))));
        for (stream, fd2) in [
            (Some("8:12345"), Some((8, 12346))),
            (Some("8:12345"), Some((9, 12345))),
            (None, Some((8, 12345))),
            (Some("8:12345"), None),
            (Some(""), Some((8, 12345))),
            (Some("8"), Some((8, 12345))),
            (Some("8:"), Some((8, 0))),
            (Some(":12345"), Some((0, 12345))),
            (Some("8:12345:1"), Some((8, 12345))),
            (Some("+8:12345"), Some((8, 12345))),
            (Some(" 8:12345"), Some((8, 12345))),
            (Some("8:12345\n"), Some((8, 12345))),
            (Some("0x8:12345"), Some((8, 12345))),
            (Some("8:18446744073709551616"), Some((8, 0))),
        ] {
            assert!(!journal_matches(stream, fd2), "{stream:?} {fd2:?}");
        }
    }

    #[test]
    fn the_prefix_appears_only_on_the_journal() {
        assert_eq!(prefix_for(Priority::Error, true), "<3>");
        assert_eq!(prefix_for(Priority::Warning, true), "<4>");
        assert_eq!(prefix_for(Priority::Error, false), "");
        assert_eq!(prefix_for(Priority::Warning, false), "");

        let mut out = Vec::new();
        let mut log = Log::new(&mut out, true);
        log.line(
            Priority::Warning,
            "stale",
            &[("reason", "rpc-unreachable"), ("action", "kept")],
        );
        log.line(Priority::Error, "error", &[("reason", "config-invalid")]);
        log.plain("result=stale reason=rpc-unreachable");
        log.finish();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "<4>stale reason=rpc-unreachable action=kept\n<3>error reason=config-invalid\nresult=stale reason=rpc-unreachable\n"
        );

        let mut out = Vec::new();
        let mut log = Log::new(&mut out, false);
        log.line(
            Priority::Warning,
            "stale",
            &[("reason", "rpc-unreachable"), ("action", "kept")],
        );
        log.line(Priority::Error, "error", &[("reason", "config-invalid")]);
        log.finish();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "stale reason=rpc-unreachable action=kept\nerror reason=config-invalid\n"
        );
    }

    #[test]
    fn odd_values_are_quoted_and_stay_on_one_line() {
        let mut out = Vec::new();
        let mut log = Log::new(&mut out, false);
        log.line(
            Priority::Error,
            "error",
            &[
                ("empty", ""),
                ("plain", "a/b:c=d,e"),
                ("space", "two words"),
                ("newline", "one\ntwo"),
                ("quote", "say \"hi\""),
                ("backslash", "a\\b"),
                ("unicode", "caf\u{e9}"),
                ("control", "\u{1b}[31m"),
            ],
        );
        log.finish();
        let text = String::from_utf8_lossy(&out).into_owned();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert_eq!(
            text,
            "error empty= plain=a/b:c=d,e space=\"two words\" newline=\"one\\ntwo\" quote=\"say \\\"hi\\\"\" backslash=\"a\\\\b\" unicode=\"caf\u{e9}\" control=\"\\u{1b}[31m\"\n"
        );
    }

    #[test]
    fn seven_lines_fit_and_the_eighth_spends_the_last_slot_on_the_cap() {
        let seven = logged(false, 7);
        let expected: String = (0..7).map(|n| format!("event n={n}\n")).collect();
        assert_eq!(seven, expected);

        let eight = logged(false, 8);
        let mut expected: String = (0..6).map(|n| format!("event n={n}\n")).collect();
        expected.push_str("log-capped dropped=2\n");
        assert_eq!(eight, expected);

        let many = logged(true, 30);
        let mut expected: String = (0..6).map(|n| format!("<4>event n={n}\n")).collect();
        expected.push_str("<4>log-capped dropped=24\n");
        assert_eq!(many, expected);
        assert_eq!(many.lines().count(), ORDINARY_LINES);

        assert_eq!(logged(false, 0), "");
        assert_eq!(logged(false, 1), "event n=0\n");
    }

    #[test]
    fn the_held_line_is_written_on_drop_when_no_closing_line_comes() {
        let mut out = Vec::new();
        let mut log = Log::new(&mut out, false);
        for n in 0..7 {
            log.line(Priority::Warning, "event", &[("n", &n.to_string())]);
        }
        // six are written at once, and the seventh waits for the end
        assert_eq!(
            String::from_utf8_lossy(log.out.as_slice()).lines().count(),
            6
        );
        drop(log);
        let text = String::from_utf8_lossy(&out).into_owned();
        assert_eq!(text.lines().count(), 7, "{text}");
        assert!(text.ends_with("event n=5\nevent n=6\n"), "{text}");
    }

    #[test]
    fn the_closing_line_is_never_dropped_and_always_last() {
        // seven ordinary lines fit beside it
        let mut expected: String = (0..7).map(|n| format!("event n={n}\n")).collect();
        expected.push_str("stale reason=r\n");
        assert_eq!(closed(7), expected);
        assert_eq!(closed(7).lines().count(), MAX_LINES);

        // past seven, the seventh slot counts every ordinary line not written
        let mut expected: String = (0..6).map(|n| format!("event n={n}\n")).collect();
        expected.push_str("log-capped dropped=2\nstale reason=r\n");
        assert_eq!(closed(8), expected);
        let mut expected: String = (0..6).map(|n| format!("event n={n}\n")).collect();
        expected.push_str("log-capped dropped=14\nstale reason=r\n");
        assert_eq!(closed(20), expected);

        assert_eq!(closed(0), "stale reason=r\n");
        assert_eq!(closed(1), "event n=0\nstale reason=r\n");
    }

    #[test]
    fn the_closing_line_waits_for_the_end_and_carries_its_prefix() {
        let mut out = Vec::new();
        let mut log = Log::new(&mut out, true);
        log.last(
            Priority::Error,
            "error",
            &[("reason", "nft-apply-failed"), ("detail", "one\ntwo")],
        );
        // lines logged after it still come before it
        log.line(
            Priority::Warning,
            "status-value-invalid",
            &[("key", "EPOCH")],
        );
        assert_eq!(
            String::from_utf8_lossy(log.out.as_slice()),
            "<4>status-value-invalid key=EPOCH\n"
        );
        log.finish();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "<4>status-value-invalid key=EPOCH\n<3>error reason=nft-apply-failed detail=\"one\\ntwo\"\n"
        );

        let mut out = Vec::new();
        let mut log = Log::new(&mut out, true);
        log.line(Priority::Warning, "floor-missing", &[]);
        log.last_plain("result=rendered reason=");
        log.finish();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "<4>floor-missing\nresult=rendered reason=\n"
        );
    }

    #[test]
    fn a_second_closing_line_is_logged_as_an_ordinary_one() {
        let mut out = Vec::new();
        let mut log = Log::new(&mut out, false);
        log.last(Priority::Warning, "stale", &[("reason", "first")]);
        log.last(Priority::Error, "error", &[("reason", "second")]);
        log.finish();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "error reason=second\nstale reason=first\n"
        );

        // with every ordinary slot taken, it is dropped and counted
        let mut out = Vec::new();
        let mut log = Log::new(&mut out, false);
        for n in 0..7 {
            log.line(Priority::Warning, "event", &[("n", &n.to_string())]);
        }
        log.last(Priority::Warning, "stale", &[("reason", "first")]);
        log.last_plain("result=stale reason=second");
        log.finish();
        let mut expected: String = (0..6).map(|n| format!("event n={n}\n")).collect();
        expected.push_str("log-capped dropped=2\nstale reason=first\n");
        assert_eq!(String::from_utf8_lossy(&out), expected);
    }

    #[test]
    fn finishing_twice_writes_once_and_later_lines_are_dropped() {
        let mut out = Vec::new();
        let mut log = Log::new(&mut out, false);
        for n in 0..7 {
            log.line(Priority::Warning, "event", &[("n", &n.to_string())]);
        }
        log.last(Priority::Warning, "stale", &[("reason", "r")]);
        log.close();
        log.line(Priority::Warning, "late", &[]);
        log.last(Priority::Error, "error", &[("reason", "late")]);
        log.close();
        drop(log);
        let text = String::from_utf8_lossy(&out).into_owned();
        assert_eq!(text.lines().count(), MAX_LINES, "{text}");
        assert!(text.ends_with("event n=6\nstale reason=r\n"), "{text}");
    }
}
