//! `:commit` / `<leader>g` commits what is staged; `e` on a commit in the commit pane renames it.
//! Both need a `--branch` review with git actions on.
//!
//! Both share one pop-up: a title line and a description. Git runs on a background thread, since
//! signing or a hook can be slow. Esc or a failure keeps the draft; a rename has its own draft,
//! tied to its commit, so it never disturbs a commit message in progress.

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

/// Result of background git work.
#[derive(Debug)]
pub enum CommitEvent {
    /// A commit: the new id, or git's error output.
    Done(std::result::Result<String, String>),
    /// A rename: every commit id that changed, old to new, or git's error.
    Renamed(std::result::Result<Vec<(String, String)>, String>),
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
        self.dialog_renaming = false;
        self.input_mode = InputMode::CommitMessage;
    }

    /// `e` in the commit pane: edit the commit's message, or the draft kept for that commit.
    pub fn open_rename_dialog(&mut self) {
        if !self.require_git_actions() {
            return;
        }
        if self.commit_rx.is_some() {
            self.set_message("Still working on the last commit\u{2026}");
            return;
        }
        let Some(commit) = self.review_commits.get(self.commit_list_cursor) else {
            return;
        };
        if Self::is_special_commit(commit) {
            self.set_message("Only commits can be renamed");
            return;
        }
        let id = commit.id.clone();
        if self
            .rename_draft
            .as_ref()
            .is_none_or(|(kept, _)| kept != &id)
        {
            let message =
                match crate::vcs::git::commit::commit_message(&self.vcs_info.root_path, &id) {
                    Ok(message) => message,
                    Err(e) => {
                        self.set_error(format!("Could not read the commit message: {e}"));
                        return;
                    }
                };
            let (title, description) = message.split_once('\n').unwrap_or((&message, ""));
            let description = description.trim_start_matches('\n').to_string();
            self.rename_draft = Some((
                id,
                CommitDraft {
                    title_cursor: title.len(),
                    title: title.to_string(),
                    description_cursor: description.len(),
                    description,
                    field: CommitField::Title,
                },
            ));
        }
        self.dialog_renaming = true;
        self.input_mode = InputMode::CommitMessage;
    }

    /// The draft being edited: the rename's or the commit's.
    pub fn dialog_draft(&self) -> Option<&CommitDraft> {
        if self.dialog_renaming {
            self.rename_draft.as_ref().map(|(_, draft)| draft)
        } else {
            self.commit_draft.as_ref()
        }
    }

    pub fn dialog_draft_mut(&mut self) -> Option<&mut CommitDraft> {
        if self.dialog_renaming {
            self.rename_draft.as_mut().map(|(_, draft)| draft)
        } else {
            self.commit_draft.as_mut()
        }
    }

    /// The commit being renamed, if any.
    pub fn dialog_rename_target(&self) -> Option<&str> {
        self.dialog_renaming
            .then(|| self.rename_draft.as_ref().map(|(id, _)| id.as_str()))
            .flatten()
    }

    /// Close the pop-up, keeping the draft for next time.
    pub fn close_commit_dialog(&mut self) {
        self.input_mode = InputMode::Normal;
    }

    /// Enter / Ctrl-S in the pop-up: commit, or rename.
    pub fn submit_commit_dialog(&mut self) {
        if self.dialog_renaming {
            self.submit_rename();
        } else {
            self.submit_commit();
        }
    }

    /// Refuse an empty title; otherwise close the pop-up and rename in the background.
    fn submit_rename(&mut self) {
        let Some((id, draft)) = self.rename_draft.as_ref() else {
            return;
        };
        if draft.title.trim().is_empty() {
            self.set_warning("The commit needs a title");
            return;
        }
        let (id, message) = (id.clone(), draft.message());
        let root = self.vcs_info.root_path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.commit_rx = Some(rx);
        std::thread::spawn(move || {
            let result = crate::vcs::git::commit::reword_commit(&root, &id, &message)
                .map_err(|e| e.to_string());
            let _ = tx.send(CommitEvent::Renamed(result));
        });
        self.input_mode = InputMode::Normal;
        self.set_message("Renaming\u{2026}");
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
        match event {
            CommitEvent::Done(result) => self.finish_commit(result),
            CommitEvent::Renamed(result) => self.finish_rename(result),
        }
        true
    }

    /// A rename gives every rewritten commit a new id. Remap commit rows, commit-scoped comments
    /// and narrowed reviewed marks first, so the refresh finds them. Files are unchanged.
    pub(in crate::app) fn finish_rename(
        &mut self,
        result: std::result::Result<Vec<(String, String)>, String>,
    ) {
        let rewritten = match result {
            Ok(rewritten) => rewritten,
            Err(error) => {
                self.set_error(format!(
                    "Rename failed \u{00b7} {error} \u{00b7} e on the commit to retry"
                ));
                return;
            }
        };
        let renamed = self.rename_draft.take();
        let ids: HashMap<String, String> = rewritten.iter().cloned().collect();
        self.remap_commit_ids(&ids);
        self.commit_diff_cache.clear();
        if let Err(e) = self.refresh_branch_target(true) {
            self.set_error(format!("Renamed, but the refresh failed: {e}"));
            return;
        }
        let short = |id: &str| id.chars().take(7).collect::<String>();
        let (old, new) = renamed
            .and_then(|(old, _)| ids.get(&old).map(|new| (old.clone(), new.clone())))
            .unwrap_or_default();
        let later = rewritten.len().saturating_sub(1);
        let tail = match later {
            0 => String::new(),
            1 => " \u{00b7} 1 later commit rewritten".to_string(),
            n => format!(" \u{00b7} {n} later commits rewritten"),
        };
        self.set_message(format!(
            "Renamed {} \u{2192} {}{tail}",
            short(&old),
            short(&new)
        ));
    }

    /// Replace rewritten commit ids wherever the review keeps one.
    fn remap_commit_ids(&mut self, ids: &HashMap<String, String>) {
        let remap = |id: &mut String| {
            if let Some(new) = ids.get(id.as_str()) {
                *id = new.clone();
            }
        };
        // A narrowed view is keyed "<id>..<id>".
        let remap_view = |view: &str| -> String {
            view.split("..")
                .map(|id| ids.get(id).map_or(id, String::as_str))
                .collect::<Vec<_>>()
                .join("..")
        };

        for commit in &mut self.review_commits {
            if let Some(new) = ids.get(&commit.id) {
                commit.short_id = new.chars().take(7).collect();
                commit.id = new.clone();
            }
        }
        for comment in &mut self.session.review_comments {
            if let Some(id) = comment.commit_id.as_mut() {
                remap(id);
            }
        }
        for review in self.session.files.values_mut() {
            let comments = review
                .file_comments
                .iter_mut()
                .chain(review.line_comments.values_mut().flatten());
            for comment in comments {
                if let Some(id) = comment.commit_id.as_mut() {
                    remap(id);
                }
            }
            review.reviewed_view_hashes = std::mem::take(&mut review.reviewed_view_hashes)
                .into_iter()
                .map(|(view, hash)| (remap_view(&view), hash))
                .collect();
            if let Some(view) = review.content_hash_view.as_mut() {
                *view = remap_view(view);
            }
        }
        if let Some(range) = self.session.commit_range.as_mut() {
            range.iter_mut().for_each(remap);
        }
        self.dirty = true;
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
