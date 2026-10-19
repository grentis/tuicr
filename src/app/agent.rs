//! `:submit` in a local review: tell the agent working on this checkout.
//!
//! A `tuicr review wait` on the session is told through its registration.
//! Otherwise `[agent] send_command` runs, with the id from
//! `tuicr review attach` in `TUICR_AGENT` and the slug in `TUICR_SESSION`;
//! only the command interprets the id. It runs on a background thread, so a
//! hung command never freezes the review. PR reviews never send.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::*;

/// Registration refresh interval, well inside the 12-hour stale limit.
const ACTIVE_HEARTBEAT_EVERY: Duration = Duration::from_secs(10 * 60);

/// How often a tuicr checks for its agent's `tuicr review done`.
const AGENT_WATCH_EVERY: Duration = Duration::from_secs(1);

/// How long without a waiting agent before a tuicr stops checking.
const AGENT_GONE_AFTER: Duration = Duration::from_secs(5 * 60);

/// A tuicr waiting for its agent's `tuicr review done`.
#[derive(Debug, Clone)]
pub struct AgentWatch {
    /// Only a done after this counts.
    seen_done_at: chrono::DateTime<chrono::Utc>,
    pub(in crate::app) next_check_at: Instant,
    waiter_gone_since: Option<Instant>,
}

/// A send's outcome: the agent's id, or why it failed.
pub type AgentSendResult = std::result::Result<String, String>;

impl App {
    /// Run `[agent] send_command` for the agent attached to this session.
    pub fn send_to_agent(&mut self) {
        if self.session.pr_session_key.is_some() {
            self.set_warning("Agents work with local reviews only");
            return;
        }
        let Some(command) = self.agent_send_command.clone() else {
            self.set_warning("Sending is off \u{00b7} set [agent] send_command in config.toml");
            return;
        };
        if self.agent_send_rx.is_some() {
            self.set_message("Still sending\u{2026}");
            return;
        }
        let agent = match crate::persistence::storage::attached_agent_for_pid() {
            Ok(Some(agent)) => agent,
            Ok(None) => {
                self.set_warning(
                    "No agent attached \u{00b7} run /tuicr-connect in the agent's pane",
                );
                return;
            }
            Err(e) => {
                self.set_error(format!("Could not read the attached agent: {e}"));
                return;
            }
        };
        let slug = match crate::persistence::storage::slug_for_session(&self.session) {
            Ok(slug) => slug.to_string(),
            Err(e) => {
                self.set_error(format!("Could not name this session: {e}"));
                return;
            }
        };
        let root = self.vcs_info.root_path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.agent_send_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(run_send_command(&command, &root, &slug, &agent));
        });
        self.set_message("Sending to the agent\u{2026}");
    }

    /// Tell the agent waiting on this session. Returns false only when none
    /// waits.
    pub(in crate::app) fn notify_waiting_agent(&mut self) -> bool {
        match crate::persistence::storage::notify_waiter_for_pid() {
            Ok(true) => {
                self.set_message("Submitted to the agent");
                self.watch_agent();
                true
            }
            Ok(false) => false,
            Err(e) => {
                self.set_error(format!("Could not reach the waiting agent: {e}"));
                true
            }
        }
    }

    /// Report a finished send. Returns whether a repaint is needed.
    pub fn poll_agent_send_events(&mut self) -> bool {
        let Some(rx) = self.agent_send_rx.as_ref() else {
            return false;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.agent_send_rx = None;
                return false;
            }
        };
        self.agent_send_rx = None;
        match result {
            Ok(agent) => self.set_message(format!("Sent to {agent}")),
            Err(error) => self.set_error(format!("Send failed \u{00b7} {error}")),
        }
        true
    }

    /// Start checking for the agent's `tuicr review done`, unless already:
    /// a `:submit` made meanwhile keeps the same watch, and its later done.
    fn watch_agent(&mut self) {
        if self.agent_watch.is_none() {
            self.agent_watch = Some(AgentWatch {
                seen_done_at: chrono::Utc::now(),
                next_check_at: Instant::now() + AGENT_WATCH_EVERY,
                waiter_gone_since: None,
            });
        }
    }

    /// While the agent works, check once a second whether it reported done.
    /// The watch ends once a done covers every `:submit`, or when no agent
    /// has waited for `AGENT_GONE_AFTER`. Returns whether to repaint.
    pub fn poll_agent_done(&mut self) -> bool {
        let now = Instant::now();
        let Some(watch) = self.agent_watch.as_mut() else {
            return false;
        };
        if now < watch.next_check_at {
            return false;
        }
        watch.next_check_at = now + AGENT_WATCH_EVERY;
        let Ok(Some(progress)) = crate::persistence::storage::agent_progress_for_pid() else {
            return false;
        };
        if progress.waiting {
            watch.waiter_gone_since = None;
        } else if now.duration_since(*watch.waiter_gone_since.get_or_insert(now))
            >= AGENT_GONE_AFTER
        {
            self.agent_watch = None;
            return false;
        }
        let Some(done) = progress.done.filter(|done| done.at > watch.seen_done_at) else {
            return false;
        };
        watch.seen_done_at = done.at;
        let covered = match (done.covers, progress.submitted_at) {
            (_, None) => true,
            (Some(read), Some(submitted)) => read >= submitted,
            (None, Some(_)) => false,
        };
        let mut text = match done.message {
            Some(message) => format!("Agent done \u{00b7} {message}"),
            None => "Agent done".to_string(),
        };
        if covered {
            self.agent_watch = None;
        } else {
            text.push_str(" \u{00b7} still on your last :submit");
        }
        self.set_message(text);
        true
    }

    /// Keep this process's active-session registration fresh while it runs.
    pub fn poll_active_session_heartbeat(&mut self) {
        let now = Instant::now();
        if now < self.next_active_heartbeat_at {
            return;
        }
        self.next_active_heartbeat_at = now + ACTIVE_HEARTBEAT_EVERY;
        let _ = crate::persistence::storage::touch_active_session_for_pid();
    }
}

/// Run the send command via the shell from the checkout root, stdin closed.
/// A failure is explained by its stderr, else its exit status.
pub(in crate::app) fn run_send_command(
    command: &str,
    root: &std::path::Path,
    slug: &str,
    agent: &str,
) -> AgentSendResult {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .env("TUICR_SESSION", slug)
        .env("TUICR_AGENT", agent)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run send_command: {e}"))?;
    if output.status.success() {
        return Ok(agent.to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("send_command exited with {}", output.status));
    Err(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_hand_the_session_and_agent_to_the_send_command() {
        let dir = tempfile::tempdir().unwrap();

        let result = run_send_command(
            r#"printf '%s %s %s' "$TUICR_SESSION" "$TUICR_AGENT" "$(pwd)" > sent.txt"#,
            dir.path(),
            "me/repo@feature/branch",
            "w1N:p1",
        );

        assert_eq!(result, Ok("w1N:p1".to_string()));
        let sent = std::fs::read_to_string(dir.path().join("sent.txt")).unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(
            sent,
            format!("me/repo@feature/branch w1N:p1 {}", root.display())
        );
    }

    #[test]
    fn should_report_why_the_send_command_failed() {
        let dir = tempfile::tempdir().unwrap();

        let stderr = run_send_command("echo 'no such pane' >&2; exit 3", dir.path(), "s", "a");
        let silent = run_send_command("exit 4", dir.path(), "s", "a");

        assert_eq!(stderr, Err("no such pane".to_string()));
        assert!(silent.unwrap_err().contains("exit status: 4"));
    }
}
