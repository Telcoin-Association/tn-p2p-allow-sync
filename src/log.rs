//! Log lines on stderr: one line per event, the event name followed by `key=value` fields, at most [`MAX_LINES`] per run.
//!
//! Under systemd each line starts with a `<3>` (error) or `<4>` (warning) prefix, which journald reads as the line's priority.
//! The prefix appears only when `JOURNAL_STREAM` names the device and inode of fd 2, systemd's documented check, so interactive output stays plain.
//!
//! A value that is not plain printable ASCII is written in Rust's quoted, escaped form, so every event stays on one line whatever it carries.
//! Once a run has logged more than [`MAX_LINES`] lines, the last slot is spent on one `log-capped dropped=N` line instead.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;

/// The most lines one run writes to stderr, the `log-capped` line included.
pub const MAX_LINES: usize = 8;

/// Lines written as they come; the line after them is held until it is known whether the cap is reached.
const IMMEDIATE_LINES: usize = MAX_LINES.saturating_sub(1);

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

/// The log of one run, written to `out`.
///
/// The first [`MAX_LINES`]` - 1` lines are written as they come.
/// The next one is held: if the run logs nothing more, it is written when the log is finished; if it does, it and every later line are dropped, and the last slot says how many went.
/// Finishing happens on [`Log::finish`] or on drop, whichever comes first.
#[derive(Debug)]
pub struct Log<W: Write> {
    out: W,
    journal: bool,
    written: usize,
    held: Option<String>,
    dropped: u64,
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
            finished: false,
        }
    }

    /// Logs one line: the prefix, `event`, then ` key=value` for each field in order.
    ///
    /// An empty value is written as `key=`; a value that holds anything but printable ASCII other than `"` and `\` is written quoted and escaped.
    pub fn line(&mut self, priority: Priority, event: &str, fields: &[(&str, &str)]) {
        let mut text = String::from(prefix_for(priority, self.journal));
        text.push_str(event);
        for (key, value) in fields {
            text.push(' ');
            text.push_str(key);
            text.push('=');
            push_value(&mut text, value);
        }
        self.emit(text);
    }

    /// Logs `text` as it is, with no prefix: a summary line the caller formatted, which must not hold a newline.
    pub fn plain(&mut self, text: &str) {
        self.emit(text.to_owned());
    }

    /// Writes the held line, or the `log-capped` line when lines were dropped, and flushes; later lines are dropped.
    pub fn finish(mut self) {
        self.close();
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
        // the last slot: held until the run logs another line or ends
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
    fn eight_lines_fit_and_the_ninth_spends_the_last_slot_on_the_cap() {
        let eight = logged(false, 8);
        let expected: String = (0..8).map(|n| format!("event n={n}\n")).collect();
        assert_eq!(eight, expected);

        let nine = logged(false, 9);
        let mut expected: String = (0..7).map(|n| format!("event n={n}\n")).collect();
        expected.push_str("log-capped dropped=2\n");
        assert_eq!(nine, expected);

        let many = logged(true, 30);
        let mut expected: String = (0..7).map(|n| format!("<4>event n={n}\n")).collect();
        expected.push_str("<4>log-capped dropped=23\n");
        assert_eq!(many, expected);
        assert_eq!(many.lines().count(), MAX_LINES);

        assert_eq!(logged(false, 0), "");
        assert_eq!(logged(false, 1), "event n=0\n");
    }

    #[test]
    fn finishing_twice_writes_once_and_later_lines_are_dropped() {
        let mut out = Vec::new();
        let mut log = Log::new(&mut out, false);
        for n in 0..8 {
            log.line(Priority::Warning, "event", &[("n", &n.to_string())]);
        }
        log.close();
        log.line(Priority::Warning, "late", &[]);
        log.close();
        drop(log);
        let text = String::from_utf8_lossy(&out).into_owned();
        assert_eq!(text.lines().count(), 8, "{text}");
        assert!(text.ends_with("event n=7\n"), "{text}");
    }
}
