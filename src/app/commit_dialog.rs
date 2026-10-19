//! `:commit` / `<leader>g` commits what is staged. Needs a `--branch` review with git actions on.
//!
//! The pop-up has a title line and a description. `git commit` runs on a background thread, since
//! signing or a hook can be slow. Esc or a failure keeps the draft.

use super::*;

/// The commit pop-up box with the cursor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommitField {
    #[default]
    Title,
    Description,
}

/// Text typed into the commit pop-up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitDraft {
    pub title: String,
    /// Cursor byte offset in `title`.
    pub title_cursor: usize,
    pub description: String,
    /// Cursor byte offset in `description`.
    pub description_cursor: usize,
    pub field: CommitField,
}

impl CommitDraft {
    /// The message for git: the title, plus a blank line and the description if any.
    pub fn message(&self) -> String {
        let title = self.title.trim();
        let description = self.description.trim_end();
        let description = description.trim_start_matches('\n');
        if description.trim().is_empty() {
            title.to_string()
        } else {
            format!("{title}\n\n{description}")
        }
    }

    /// The focused field's text and cursor.
    pub fn active_mut(&mut self) -> (&mut String, &mut usize) {
        match self.field {
            CommitField::Title => (&mut self.title, &mut self.title_cursor),
            CommitField::Description => (&mut self.description, &mut self.description_cursor),
        }
    }
}

/// Result of a background commit.
#[derive(Debug)]
pub enum CommitEvent {
    /// The new id, or git's error output.
    Done(std::result::Result<String, String>),
}

impl App {
    /// Paths with staged changes, in tree order.
    pub fn staged_paths(&self) -> Vec<PathBuf> {
        self.diff_files
            .iter()
            .map(|file| file.display_path())
            .filter(|path| self.git_file_states.get(*path).is_some_and(|s| s.staged))
            .cloned()
            .collect()
    }

    /// Open the commit pop-up, unless there is nothing to commit.
    pub fn open_commit_dialog(&mut self) {
        if !self.require_git_actions() {
            return;
        }
        if self.commit_rx.is_some() {
            self.set_message("Still committing\u{2026}");
            return;
        }
        if !self.git_file_states.values().any(|state| state.staged) {
            self.set_message("Nothing staged \u{00b7} s stages the file at the cursor");
            return;
        }
        let draft = self.commit_draft.get_or_insert_with(CommitDraft::default);
        draft.field = CommitField::Title;
        self.input_mode = InputMode::CommitMessage;
    }

    /// Close the pop-up, keeping the draft for next time.
    pub fn close_commit_dialog(&mut self) {
        self.input_mode = InputMode::Normal;
    }

    /// Refuse an empty title; otherwise close the pop-up and run `git commit` in the background.
    pub fn submit_commit(&mut self) {
        let Some(draft) = self.commit_draft.as_ref() else {
            return;
        };
        if draft.title.trim().is_empty() {
            self.set_warning("The commit needs a title");
            return;
        }
        let message = draft.message();
        let root = self.vcs_info.root_path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.commit_rx = Some(rx);
        std::thread::spawn(move || {
            let result =
                crate::vcs::git::commit::commit_staged(&root, &message).map_err(|e| e.to_string());
            let _ = tx.send(CommitEvent::Done(result));
        });
        self.input_mode = InputMode::Normal;
        self.set_message("Committing\u{2026}");
    }

    /// Apply a finished commit, if any. Returns whether to repaint.
    pub fn poll_commit_events(&mut self) -> bool {
        let Some(rx) = self.commit_rx.as_ref() else {
            return false;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.commit_rx = None;
                return false;
            }
        };
        self.commit_rx = None;
        let CommitEvent::Done(result) = event;
        self.finish_commit(result);
        true
    }

    /// After a commit, re-read the commits and file states. The whole-branch diff is unchanged, but
    /// a narrowed selection's staged/unstaged rows may be gone, widening it, so its diff is re-read
    /// too.
    pub(in crate::app) fn finish_commit(&mut self, result: std::result::Result<String, String>) {
        match result {
            Ok(id) => {
                let title = self
                    .commit_draft
                    .take()
                    .map(|draft| draft.title.trim().to_string())
                    .unwrap_or_default();
                let narrowed = Self::is_strict_commit_selection(
                    self.commit_selection_range,
                    self.review_commits.len(),
                );
                let refreshed = if narrowed {
                    self.reload_diff_files().map(|_| ())
                } else {
                    self.refresh_branch_target(true).map(|_| ())
                };
                if let Err(e) = refreshed {
                    self.set_error(format!("Committed, but the refresh failed: {e}"));
                    return;
                }
                let short: String = id.chars().take(7).collect();
                self.set_message(format!("Committed {short} {title}"));
            }
            Err(error) => {
                self.set_error(format!(
                    "Commit failed \u{00b7} {error} \u{00b7} :commit to retry"
                ));
            }
        }
    }
}
