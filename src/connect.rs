//! The reviewer's side of tuicr-connect, for programs that host a review the
//! way a running tuicr does.
//!
//! The protocol lives in `active_sessions.json`, in the reviews directory:
//! each running reviewer has one entry, keyed by its process id, naming the
//! session file it has open. An agent finds the entry through
//! `tuicr review list`, attaches to it (`tuicr review attach`), waits for a
//! `:submit` (`tuicr review wait`), reads and answers the comments in the
//! session file (`tuicr review comments` / `add`), and says it is finished
//! (`tuicr review done`). The reviewer registers, keeps its entry fresh,
//! wakes the waiter on submit, watches the agent's progress, and removes its
//! entry when it closes.
//!
//! Every function here works on the calling process's entry, as tuicr's own
//! TUI does: one process, one entry. A program hosting several reviews runs
//! one process per review. The `_in_dir` variants take the reviews directory
//! and the process id, for tests.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::model::ReviewSession;
use crate::persistence::storage;

pub use crate::persistence::storage::{AgentDone, AgentProgress, slug_for_session};

/// Where `session` is saved in the reviews directory: the path to register
/// with [`mark_session_active`], and the file the agent reads and writes.
pub fn session_path(session: &ReviewSession) -> Result<PathBuf> {
    storage::session_path(session)
}

/// [`session_path`] in `reviews_dir`.
pub fn session_path_in_dir(session: &ReviewSession, reviews_dir: &Path) -> Result<PathBuf> {
    storage::session_path_in_dir(session, reviews_dir)
}

/// Register this process as the reviewer of the session saved at `path`:
/// its one entry, with the session's slug, now as last seen. Called again
/// (tuicr does on every save) it keeps the attached agent, the waiter and
/// what they recorded. Stale entries of other processes are dropped.
pub fn mark_session_active(session: &ReviewSession, path: &Path) -> Result<()> {
    storage::mark_session_active(session, path)
}

/// [`mark_session_active`] for process `pid` in `reviews_dir`.
pub fn mark_session_active_in_dir(
    session: &ReviewSession,
    path: &Path,
    reviews_dir: &Path,
    pid: u32,
) -> Result<()> {
    storage::mark_session_active_for_pid_in_dir(session, path, reviews_dir, pid)
}

/// The heartbeat: refresh this process's entry, so a review left open
/// longer than the stale limit without saving still lists as active.
pub fn touch_active_session_for_pid() -> Result<()> {
    storage::touch_active_session_for_pid()
}

/// [`touch_active_session_for_pid`] for process `pid` in `reviews_dir`.
pub fn touch_active_session_for_pid_in_dir(reviews_dir: &Path, pid: u32) -> Result<()> {
    storage::touch_active_session_for_pid_in_dir(reviews_dir, pid)
}

/// Remove this process's entry: the review closed.
pub fn clear_active_session_for_pid() -> Result<()> {
    storage::clear_active_session_for_pid()
}

/// [`clear_active_session_for_pid`] for process `pid` in `reviews_dir`.
pub fn clear_active_session_for_pid_in_dir(reviews_dir: &Path, pid: u32) -> Result<()> {
    storage::clear_active_session_of_pid_in_dir(reviews_dir, pid)
}

/// The agent `tuicr review attach` registered on this process's entry, if
/// any: the id `[agent] send_command` is given on `:submit`.
pub fn attached_agent_for_pid() -> Result<Option<String>> {
    storage::attached_agent_for_pid()
}

/// [`attached_agent_for_pid`] for process `pid` in `reviews_dir`.
pub fn attached_agent_for_pid_in_dir(reviews_dir: &Path, pid: u32) -> Result<Option<String>> {
    storage::attached_agent_for_pid_in_dir(reviews_dir, pid)
}

/// `:submit`: wake the `tuicr review wait` waiting on this process's
/// session, stamping the submit on the entry. False when none waits (then
/// tuicr runs `[agent] send_command` for the attached agent instead).
pub fn notify_waiter_for_pid() -> Result<bool> {
    storage::notify_waiter_for_pid()
}

/// [`notify_waiter_for_pid`] for process `pid` in `reviews_dir`.
pub fn notify_waiter_for_pid_in_dir(reviews_dir: &Path, pid: u32) -> Result<bool> {
    storage::notify_waiter_for_pid_in_dir(reviews_dir, pid)
}

/// The agent's progress on this process's session: the last submit it was
/// told of, its last `tuicr review done`, and whether it waits now. Polled
/// for tuicr's "Agent done" status.
pub fn agent_progress_for_pid() -> Result<Option<AgentProgress>> {
    storage::agent_progress_for_pid()
}

/// [`agent_progress_for_pid`] for process `pid` in `reviews_dir`.
pub fn agent_progress_for_pid_in_dir(
    reviews_dir: &Path,
    pid: u32,
) -> Result<Option<AgentProgress>> {
    storage::agent_progress_for_pid_in_dir(reviews_dir, pid)
}

#[cfg(test)]
mod tests {
    //! The by-process flow through this module, with the agent's side as
    //! `tuicr review` runs it.

    use super::*;
    use crate::model::{FileStatus, SessionDiffSource};
    use crate::persistence::storage::{
        WaitState, active_session_owners_in_dir, attach_agent_in_dir, mark_agent_done_in_dir,
        mark_agent_read_in_dir, save_session_in_dir, wait_on_session_in_dir,
    };

    /// A session of a repository of its own, saved in `reviews`.
    fn saved(reviews: &Path) -> (tempfile::TempDir, ReviewSession, PathBuf) {
        let repo = tempfile::tempdir().unwrap();
        let mut s = ReviewSession::new(
            repo.path().to_path_buf(),
            "abc1234".to_string(),
            Some("feature".to_string()),
            SessionDiffSource::Branch,
        );
        s.add_file(PathBuf::from("src/main.rs"), FileStatus::Modified, 0);
        let path = save_session_in_dir(&s, reviews).unwrap();
        (repo, s, path)
    }

    /// Another live process, standing for a second reviewer.
    struct Other(std::process::Child);

    impl Other {
        fn start() -> Self {
            Other(
                std::process::Command::new("sleep")
                    .arg("60")
                    .spawn()
                    .unwrap(),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Other {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn two_reviewers_keep_an_entry_each_and_an_agent_talks_to_one() {
        let reviews = tempfile::tempdir().unwrap();
        let reviews = reviews.path();
        let (_repo_a, a, path_a) = saved(reviews);
        let (_repo_b, b, path_b) = saved(reviews);
        let me = std::process::id();
        let other = Other::start();

        // Two reviewers register side by side: one entry each, by pid.
        mark_session_active_in_dir(&a, &path_a, reviews, me).unwrap();
        mark_session_active_in_dir(&b, &path_b, reviews, other.pid()).unwrap();
        let owners = active_session_owners_in_dir(reviews).unwrap();
        assert_eq!(owners.len(), 2);
        let norm = |p: &Path| std::fs::canonicalize(p).unwrap();
        assert_eq!(owners[&norm(&path_a)].pid, me);
        assert_eq!(owners[&norm(&path_b)].pid, other.pid());

        // The file keeps its shape: the fields of an entry, by name.
        let file: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(reviews.join("active_sessions.json")).unwrap(),
        )
        .unwrap();
        let entry = file["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["pid"] == me)
            .unwrap();
        let mut keys: Vec<&str> = entry
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort();
        assert_eq!(keys, ["last_seen_at", "path", "pid", "slug"]);
        assert_eq!(entry["slug"], slug_for_session(&a).unwrap().to_string());

        // An agent attaches to the first: only its reviewer sees it.
        assert!(attach_agent_in_dir(reviews, &path_a, "w1:p1").unwrap());
        assert_eq!(
            attached_agent_for_pid_in_dir(reviews, me)
                .unwrap()
                .as_deref(),
            Some("w1:p1")
        );
        assert_eq!(
            attached_agent_for_pid_in_dir(reviews, other.pid()).unwrap(),
            None
        );
        // Registering again (a save) keeps the agent.
        mark_session_active_in_dir(&a, &path_a, reviews, me).unwrap();
        assert_eq!(
            attached_agent_for_pid_in_dir(reviews, me)
                .unwrap()
                .as_deref(),
            Some("w1:p1")
        );

        // It waits; :submit on its reviewer wakes it, the other's does not.
        let waiter = Other::start();
        assert!(!notify_waiter_for_pid_in_dir(reviews, me).unwrap());
        assert_eq!(
            wait_on_session_in_dir(reviews, &path_a, waiter.pid()).unwrap(),
            WaitState::Waiting { submitted_at: None }
        );
        assert!(notify_waiter_for_pid_in_dir(reviews, me).unwrap());
        assert!(!notify_waiter_for_pid_in_dir(reviews, other.pid()).unwrap());
        let progress = agent_progress_for_pid_in_dir(reviews, me).unwrap().unwrap();
        assert!(progress.waiting && progress.submitted_at.is_some() && progress.done.is_none());

        // It reads, then says it is done: its reviewer sees it, the other not.
        mark_agent_read_in_dir(reviews, &path_a).unwrap();
        assert!(mark_agent_done_in_dir(reviews, &path_a, Some("all answered")).unwrap());
        let done = agent_progress_for_pid_in_dir(reviews, me)
            .unwrap()
            .unwrap()
            .done
            .unwrap();
        assert_eq!(done.message.as_deref(), Some("all answered"));
        assert!(done.covers.is_some());
        let theirs = agent_progress_for_pid_in_dir(reviews, other.pid())
            .unwrap()
            .unwrap();
        assert!(theirs.done.is_none() && theirs.submitted_at.is_none());
        // Its entry now holds every field the protocol writes.
        let file: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(reviews.join("active_sessions.json")).unwrap(),
        )
        .unwrap();
        let entry = file["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["pid"] == me)
            .unwrap();
        let mut keys: Vec<&str> = entry
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "agent",
                "agent_done",
                "agent_read_at",
                "last_seen_at",
                "path",
                "pid",
                "slug",
                "submitted_at",
                "waiter_pid"
            ]
        );

        // The heartbeat and the close touch only their own entry.
        touch_active_session_for_pid_in_dir(reviews, other.pid()).unwrap();
        clear_active_session_for_pid_in_dir(reviews, me).unwrap();
        let owners = active_session_owners_in_dir(reviews).unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[&norm(&path_b)].pid, other.pid());

        // A reviewer gone without clearing: its entry is stale, dropped.
        drop(other);
        assert!(active_session_owners_in_dir(reviews).unwrap().is_empty());
        mark_session_active_in_dir(&a, &path_a, reviews, me).unwrap();
        let owners = active_session_owners_in_dir(reviews).unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[&norm(&path_a)].pid, me);
    }

    #[test]
    fn the_session_path_is_where_the_session_is_saved() {
        let reviews = tempfile::tempdir().unwrap();
        let (_repo, s, path) = saved(reviews.path());
        assert_eq!(session_path_in_dir(&s, reviews.path()).unwrap(), path);
    }
}
