//! Background task registry and notification reconciliation.

use std::collections::HashMap;
use std::sync::Mutex;

use nu_ansi_term::Style;
use tower_mcp::protocol::{TaskObject, TaskStatus, TaskStatusParams};
use tower_mcp::tasks::TaskStatusNotificationParams;

use crate::output::AsyncOutput;
use crate::style::{paint, sanitize, tag, task_status_style};

const MAX_PENDING_NOTIFICATIONS: usize = 128;

/// A task started by this REPL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// The server's id, which is authoritative and goes on the wire.
    pub task_id: String,
    /// A small number assigned by this session, so the task can be named
    /// at the prompt without copying 32 hex characters.
    pub number: usize,
    pub tool: String,
    pub status: TaskStatus,
    pub status_message: Option<String>,
}

impl Job {
    /// How the task is shown: the short number, with enough of the real id
    /// to recognize it in a server log.
    pub fn label(&self) -> String {
        format!("{} ({})", self.number, abbreviate(&self.task_id))
    }
}

/// Server task ids are opaque and often long. Show enough to match against
/// a log line without filling the terminal.
pub fn abbreviate(task_id: &str) -> String {
    const KEEP: usize = 8;
    match task_id.char_indices().nth(KEEP) {
        Some((cut, _)) if task_id.len() > KEEP + 3 => format!("{}...", &task_id[..cut]),
        _ => task_id.to_string(),
    }
}

#[derive(Clone)]
struct PendingStatus {
    status: TaskStatus,
    status_message: Option<String>,
}

#[derive(Default)]
struct State {
    jobs: Vec<Job>,
    pending: HashMap<String, PendingStatus>,
    /// Monotonic within the session, so a number is never reused even
    /// after a task settles.
    next_number: usize,
}

struct Transition {
    task_id: String,
    status: TaskStatus,
    status_message: Option<String>,
}

/// Shared tasks started by the REPL.
pub struct Jobs {
    state: Mutex<State>,
    output: AsyncOutput,
    announce: bool,
}

impl Jobs {
    pub fn new(output: AsyncOutput, announce: bool) -> Self {
        Self {
            state: Mutex::new(State::default()),
            output,
            announce,
        }
    }

    /// Record a task returned by a background tool call. If its notification
    /// raced ahead of the response, reconcile and announce that newer state.
    pub fn register(
        &self,
        task_id: String,
        tool: String,
        status: TaskStatus,
        status_message: Option<String>,
    ) {
        let transition = {
            let mut state = self.state.lock().unwrap();
            let pending = state.pending.remove(&task_id);
            state.next_number += 1;
            let number = state.next_number;
            state.jobs.push(Job {
                task_id: task_id.clone(),
                number,
                tool,
                status,
                status_message,
            });
            pending.and_then(|pending| {
                apply_status(
                    &mut state.jobs,
                    &task_id,
                    pending.status,
                    pending.status_message,
                )
            })
        };
        self.announce(transition);
    }

    /// Observe a legacy task notification.
    pub fn observe_legacy(&self, params: TaskStatusParams) {
        self.observe(params.task_id, params.status, params.status_message);
    }

    /// Observe a final-protocol task notification.
    pub fn observe_final(&self, params: TaskStatusNotificationParams) {
        self.observe(
            params.task.task_id().to_string(),
            params.task.status(),
            params.task.metadata().status_message.clone(),
        );
    }

    /// Observe a status fetched by the bounded per-task polling fallback.
    pub fn observe_task(&self, task: &TaskObject) {
        self.observe(
            task.task_id.clone(),
            task.status,
            task.status_message.clone(),
        );
    }

    /// Silently refresh a known task after an explicit `jobs`, `task`, or
    /// `wait` request. Manual commands render their own authoritative output.
    pub fn sync(&self, task_id: &str, status: TaskStatus, status_message: Option<String>) {
        let mut state = self.state.lock().unwrap();
        if let Some(job) = state.jobs.iter_mut().find(|job| job.task_id == task_id) {
            job.status = status;
            job.status_message = status_message;
        }
    }

    pub fn list(&self) -> Vec<Job> {
        self.state.lock().unwrap().jobs.clone()
    }

    /// Resolve what the user typed to a server task id.
    ///
    /// Accepts the short session number, the full id, or an unambiguous
    /// prefix of one. Returns `None` when nothing matches, so the caller
    /// can report it rather than sending a made-up id to the server.
    pub fn resolve(&self, typed: &str) -> Option<String> {
        let state = self.state.lock().unwrap();
        if let Ok(number) = typed.parse::<usize>()
            && let Some(job) = state.jobs.iter().find(|job| job.number == number)
        {
            return Some(job.task_id.clone());
        }
        if state.jobs.iter().any(|job| job.task_id == typed) {
            return Some(typed.to_string());
        }
        let mut matches = state
            .jobs
            .iter()
            .filter(|job| job.task_id.starts_with(typed));
        match (matches.next(), matches.next()) {
            (Some(job), None) => Some(job.task_id.clone()),
            // Ambiguous or absent: the caller says so.
            _ => None,
        }
    }

    /// The short label for a task id, for rendering a command's own output.
    pub fn label_for(&self, task_id: &str) -> String {
        self.state
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|job| job.task_id == task_id)
            .map(Job::label)
            .unwrap_or_else(|| abbreviate(task_id))
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().unwrap().jobs.is_empty()
    }

    pub fn is_terminal(&self, task_id: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|job| job.task_id == task_id)
            .is_some_and(|job| job.status.is_terminal())
    }

    pub fn automatic_updates_enabled(&self) -> bool {
        self.announce
    }

    fn observe(&self, task_id: String, status: TaskStatus, status_message: Option<String>) {
        let transition = {
            let mut state = self.state.lock().unwrap();
            if let Some(transition) =
                apply_status(&mut state.jobs, &task_id, status, status_message.clone())
            {
                Some(transition)
            } else if state.jobs.iter().any(|job| job.task_id == task_id) {
                None
            } else {
                if state.pending.len() >= MAX_PENDING_NOTIFICATIONS
                    && !state.pending.contains_key(&task_id)
                    && let Some(evicted) = state.pending.keys().next().cloned()
                {
                    state.pending.remove(&evicted);
                }
                state.pending.insert(
                    task_id,
                    PendingStatus {
                        status,
                        status_message,
                    },
                );
                None
            }
        };
        self.announce(transition);
    }

    fn announce(&self, transition: Option<Transition>) {
        if !self.announce {
            return;
        }
        let Some(transition) = transition else {
            return;
        };
        let status = transition.status.to_string();
        // Task ids and status messages are server-authored and land on the
        // terminal asynchronously, so they are sanitized like any other
        // server string.
        let task_id = sanitize(&transition.task_id);
        let label = self.label_for(&transition.task_id);
        let mut line = format!(
            "{} {}",
            tag(Style::new(), &format!("task {}", sanitize(&label))),
            paint(task_status_style(transition.status), &status)
        );
        if let Some(message) = transition
            .status_message
            .as_deref()
            .filter(|message| !message.is_empty())
        {
            line.push_str(&format!(" — {}", sanitize(message)));
        }
        if transition.status.is_terminal() || transition.status == TaskStatus::InputRequired {
            let short = self
                .state
                .lock()
                .unwrap()
                .jobs
                .iter()
                .find(|job| job.task_id == transition.task_id)
                .map(|job| job.number.to_string())
                .unwrap_or_else(|| task_id.to_string());
            line.push_str(&format!(
                "  {}",
                paint(
                    Style::new().dimmed(),
                    &format!("run `task {short}` for details")
                )
            ));
        }
        self.output.line(line);
    }
}

fn apply_status(
    jobs: &mut [Job],
    task_id: &str,
    status: TaskStatus,
    status_message: Option<String>,
) -> Option<Transition> {
    let job = jobs.iter_mut().find(|job| job.task_id == task_id)?;
    if job.status == status {
        job.status_message = status_message;
        return None;
    }
    job.status = status;
    job.status_message = status_message.clone();
    Some(Transition {
        task_id: task_id.to_string(),
        status,
        status_message,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;

    fn fixture() -> (Jobs, reedline::ExternalPrinter<String>) {
        let output = AsyncOutput::new(Arc::new(AtomicBool::new(true)), true);
        let printer = output.external_printer().unwrap();
        (Jobs::new(output, true), printer)
    }

    #[test]
    fn tracked_transitions_print_once() {
        let (jobs, printer) = fixture();
        jobs.register(
            "task-1".into(),
            "slow_add".into(),
            TaskStatus::Working,
            None,
        );
        assert!(printer.get_line().is_none());

        jobs.observe_legacy(TaskStatusParams {
            task_id: "task-1".into(),
            status: TaskStatus::Completed,
            status_message: Some("done".into()),
            created_at: "2026-08-02T00:00:00Z".into(),
            last_updated_at: "2026-08-02T00:00:01Z".into(),
            ttl: None,
            poll_interval: None,
            meta: None,
        });
        let line = printer.get_line().unwrap();
        // The line names the task by its short session number, keeping the
        // server's id visible so it can be matched against a log.
        assert!(line.contains("[task 1 (task-1)]"), "{line}");
        assert!(line.contains("completed"), "{line}");
        // The follow-up hint is what the user types, so it is the number.
        assert!(line.contains("run `task 1`"), "{line}");

        jobs.observe("task-1".into(), TaskStatus::Completed, None);
        assert!(printer.get_line().is_none(), "replay must be deduplicated");
    }

    #[test]
    fn notification_that_wins_the_creation_race_is_reconciled() {
        let (jobs, printer) = fixture();
        jobs.observe("task-race".into(), TaskStatus::Failed, Some("boom".into()));
        assert!(printer.get_line().is_none(), "unknown tasks stay silent");

        jobs.register("task-race".into(), "run".into(), TaskStatus::Working, None);
        let line = printer.get_line().unwrap();
        assert!(line.contains("failed"));
        assert!(line.contains("boom"));
        assert_eq!(jobs.list()[0].status, TaskStatus::Failed);
    }

    #[test]
    fn input_failed_and_cancelled_transitions_are_visible() {
        let (jobs, printer) = fixture();
        for (id, status) in [
            ("input", TaskStatus::InputRequired),
            ("failed", TaskStatus::Failed),
            ("cancelled", TaskStatus::Cancelled),
        ] {
            jobs.register(id.into(), "run".into(), TaskStatus::Working, None);
            jobs.observe(id.into(), status, None);
            let line = printer.get_line().unwrap();
            assert!(line.contains(&status.to_string()), "{line}");
            // Each registration takes the next number; the server id stays
            // alongside it.
            assert!(line.contains(&format!("({id})")), "{line}");
        }
    }

    #[test]
    fn a_task_is_reachable_by_number_id_or_prefix() {
        let (jobs, _printer) = fixture();
        jobs.register(
            "f1c563f39a0b4e21".into(),
            "slow_add".into(),
            TaskStatus::Working,
            None,
        );
        // What the announcement told the user to type.
        assert_eq!(jobs.resolve("1").as_deref(), Some("f1c563f39a0b4e21"));
        // The full id a server log would show.
        assert_eq!(
            jobs.resolve("f1c563f39a0b4e21").as_deref(),
            Some("f1c563f39a0b4e21")
        );
        // The abbreviated form the REPL prints.
        assert_eq!(
            jobs.resolve("f1c563f3").as_deref(),
            Some("f1c563f39a0b4e21")
        );
        // Nothing invented for a task this session never started: the
        // command reports it instead of asking the server about a guess.
        assert_eq!(jobs.resolve("2"), None);
        assert_eq!(jobs.resolve("deadbeef"), None);
    }

    #[test]
    fn an_ambiguous_prefix_resolves_to_nothing() {
        let (jobs, _printer) = fixture();
        jobs.register("abc111".into(), "a".into(), TaskStatus::Working, None);
        jobs.register("abc222".into(), "b".into(), TaskStatus::Working, None);
        assert_eq!(jobs.resolve("abc"), None);
        assert_eq!(jobs.resolve("abc1").as_deref(), Some("abc111"));
        // Numbers stay unambiguous even when the ids overlap.
        assert_eq!(jobs.resolve("2").as_deref(), Some("abc222"));
    }

    #[test]
    fn long_ids_are_shortened_and_short_ones_are_not() {
        assert_eq!(abbreviate("f1c563f39a0b4e21f7"), "f1c563f3...");
        assert_eq!(abbreviate("task-1"), "task-1");
    }

    #[test]
    fn one_shot_policy_suppresses_automatic_lines() {
        let output = AsyncOutput::new(Arc::new(AtomicBool::new(true)), true);
        let printer = output.external_printer().unwrap();
        let jobs = Jobs::new(output, false);
        jobs.register(
            "task-1".into(),
            "slow_add".into(),
            TaskStatus::Working,
            None,
        );
        jobs.observe("task-1".into(), TaskStatus::Cancelled, None);
        assert!(printer.get_line().is_none());
    }
}
