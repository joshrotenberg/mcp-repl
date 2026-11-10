//! A stdio server's stderr: a bounded history, and whether it prints live.
//!
//! The server process writes its diagnostics to stderr, separately from the
//! JSON-RPC stream on stdout. Those lines are still worth seeing, but printed
//! unlabelled above the prompt they read like tool output, and once they have
//! scrolled away there is no getting them back. This keeps the most recent
//! lines, already sanitized, so `stderr` can show them on demand, and lets
//! `stderr quiet` stop the live printing without losing anything.
//!
//! Process-global like `wire()`: there is one terminal and one server at a
//! time, and the reader task and the command loop both reach for it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::untrusted::sanitize_log;

/// How many lines are kept. Enough for a startup failure plus some context,
/// small enough that a chatty server cannot grow memory without bound.
pub const CAPACITY: usize = 500;

pub struct ServerStderr {
    lines: Mutex<VecDeque<String>>,
    capacity: usize,
    live: AtomicBool,
}

impl ServerStderr {
    fn new(capacity: usize) -> Self {
        Self {
            lines: Mutex::new(VecDeque::with_capacity(capacity.min(64))),
            capacity,
            live: AtomicBool::new(true),
        }
    }

    /// Store one raw line from the server and return its sanitized form if it
    /// should also print now.
    pub fn record(&self, raw: &str) -> Option<String> {
        let line = sanitize_log(raw).into_owned();
        {
            let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
            if lines.len() == self.capacity {
                lines.pop_front();
            }
            lines.push_back(line.clone());
        }
        self.is_live().then_some(line)
    }

    /// The most recent `limit` lines, oldest first, or all of them.
    pub fn recent(&self, limit: Option<usize>) -> Vec<String> {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let skip = limit.map_or(0, |limit| lines.len().saturating_sub(limit));
        lines.iter().skip(skip).cloned().collect()
    }

    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }

    pub fn set_live(&self, live: bool) {
        self.live.store(live, Ordering::SeqCst);
    }
}

static LOG: OnceLock<ServerStderr> = OnceLock::new();

/// The session's server stderr log.
pub fn server_stderr() -> &'static ServerStderr {
    LOG.get_or_init(|| ServerStderr::new(CAPACITY))
}

/// What `stderr` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum StderrCommand {
    Show(Option<usize>),
    SetLive(bool),
}

/// Parse the words after `stderr`.
pub fn parse_command(rest: &[&str]) -> Result<StderrCommand, String> {
    match rest {
        [] => Ok(StderrCommand::Show(None)),
        ["live"] => Ok(StderrCommand::SetLive(true)),
        ["quiet"] => Ok(StderrCommand::SetLive(false)),
        [count] => match count.parse::<usize>() {
            Ok(count) if count > 0 => Ok(StderrCommand::Show(Some(count))),
            _ => Err(format!(
                "usage: stderr [<lines>|live|quiet] (got `{}`)",
                crate::untrusted::sanitize(count)
            )),
        },
        _ => Err("usage: stderr [<lines>|live|quiet]".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_newest_lines_in_order() {
        let log = ServerStderr::new(3);
        for line in ["one", "two", "three", "four"] {
            log.record(line);
        }
        assert_eq!(log.recent(None), ["two", "three", "four"]);
        assert_eq!(log.recent(Some(2)), ["three", "four"]);
        assert_eq!(log.recent(Some(10)), ["two", "three", "four"]);
    }

    #[test]
    fn live_prints_and_quiet_only_records() {
        let log = ServerStderr::new(10);
        assert_eq!(log.record("shown").as_deref(), Some("shown"));
        log.set_live(false);
        assert_eq!(log.record("kept"), None);
        log.set_live(true);
        assert_eq!(log.recent(None), ["shown", "kept"]);
    }

    #[test]
    fn stored_lines_are_sanitized() {
        let log = ServerStderr::new(10);
        let printed = log.record("\u{1b}[32mINFO\u{1b}[0m ok\u{1b}]52;c;eA==\u{07}");
        assert_eq!(printed.as_deref(), Some("INFO ok\u{FFFD}"));
        assert_eq!(log.recent(None), ["INFO ok\u{FFFD}"]);
    }

    #[test]
    fn parses_arguments() {
        assert_eq!(parse_command(&[]), Ok(StderrCommand::Show(None)));
        assert_eq!(parse_command(&["20"]), Ok(StderrCommand::Show(Some(20))));
        assert_eq!(parse_command(&["live"]), Ok(StderrCommand::SetLive(true)));
        assert_eq!(parse_command(&["quiet"]), Ok(StderrCommand::SetLive(false)));
        assert!(parse_command(&["0"]).is_err());
        assert!(parse_command(&["loud"]).is_err());
        assert!(parse_command(&["10", "live"]).is_err());
    }
}
