//! `:submit` in a local review: tell the agent working on this checkout.
//!
//! `[agent] send_command` runs, with the id from `tuicr review attach` in
//! `TUICR_AGENT` and the slug in `TUICR_SESSION`; only the command interprets
//! the id. It runs on a background thread, so a hung command never freezes
//! the review. PR reviews never send.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::*;

/// Registration refresh interval, well inside the 12-hour stale limit.
const ACTIVE_HEARTBEAT_EVERY: Duration = Duration::from_secs(10 * 60);

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
