//! Basic git actions inside a `--branch` review (config `git_actions`).
//!
//! A branch review diffs base to working tree, so it already shows the branch's next commits. These
//! actions work on the uncommitted part: `s` stages or unstages a file or directory, the tree marks
//! each file's state, `:uncommitted` narrows the tree to files with something to commit, and `d`
//! discards a file's or directory's uncommitted changes. Staging never changes the diff, which ends
//! at the working tree, so reviewed marks are unaffected.

use super::*;
use crate::vcs::traits::ChangeKind;

/// Where a file's uncommitted changes sit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitFileState {
    /// The index differs from HEAD.
    pub staged: bool,
    /// The working tree differs from the index (untracked files included).
    pub unstaged: bool,
}

/// Changes `d` will discard once confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardRequest {
    /// The file or directory the dialog names.
    pub target: String,
    /// Files with uncommitted changes, as the tree shows them.
    pub files: usize,
    /// Every path to restore or delete, including a rename's old side.
    pub paths: Vec<PathBuf>,
}

impl GitFileState {
    /// True when all the file's changes in this review are already committed.
    pub fn committed(self) -> bool {
        !self.staged && !self.unstaged
    }
}

impl App {
    /// Git actions need the setting, a branch review, and git itself.
    pub fn git_actions_active(&self) -> bool {
        self.git_actions && self.branch_base().is_some() && self.vcs_info.vcs_type == VcsType::Git
    }

    /// The state of `path`, or `None` when git actions are off.
    pub fn git_file_state(&self, path: &Path) -> Option<GitFileState> {
        self.git_actions_active()
            .then(|| self.git_file_states.get(path).copied().unwrap_or_default())
    }

    /// Re-read which files are staged and unstaged (startup, staging changes, `:e`).
    pub fn refresh_git_file_states(&mut self) {
        self.git_file_states.clear();
        if !self.git_actions_active() {
            return;
        }
        for kind in [ChangeKind::Staged, ChangeKind::Unstaged] {
            let paths = match self.vcs.list_changed_paths(kind) {
                Ok(paths) => paths,
                Err(e) => {
                    self.set_warning(format!("Could not read the git status: {e}"));
                    return;
                }
            };
            for path in paths {
                let state = self.git_file_states.entry(path).or_default();
                match kind {
                    ChangeKind::Staged => state.staged = true,
                    ChangeKind::Unstaged => state.unstaged = true,
                }
            }
        }
    }

    /// Explain why `s` does nothing outside a branch review with the setting.
    pub(in crate::app) fn require_git_actions(&mut self) -> bool {
        if self.git_actions_active() {
            return true;
        }
        let reason = if !self.git_actions {
            "Git actions are off \u{00b7} set git_actions = true"
        } else if self.vcs_info.vcs_type != VcsType::Git {
            "Git actions need a git repository"
        } else {
            "Git actions need a --branch review"
        };
        self.set_message(reason);
        false
    }

    /// `s` on a file: stage unstaged changes, else unstage. A file with nothing uncommitted is left
    /// alone.
    pub fn toggle_staged_for_file_idx(&mut self, file_idx: usize) {
        if !self.require_git_actions() {
            return;
        }
        let Some(path) = self
            .diff_files
            .get(file_idx)
            .map(|file| file.display_path().clone())
        else {
            return;
        };
        let state = self.git_file_states.get(&path).copied().unwrap_or_default();
        if state.committed() {
            return;
        }
        let stage = state.unstaged;
        let result = if stage {
            self.vcs.stage_file(&path)
        } else {
            self.vcs.unstage_file(&path)
        };
        let name = path.display();
        match result {
            Ok(()) => {
                let verb = if stage { "Staged" } else { "Unstaged" };
                self.set_message(format!("{verb} {name}"));
            }
            Err(e) => {
                let verb = if stage { "stage" } else { "unstage" };
                self.set_error(format!("Failed to {verb} {name}: {e}"));
            }
        }
        self.after_staging_change(std::slice::from_ref(&path));
    }

    /// `s` on a directory: stage its files with unstaged changes or, if none has any, unstage them
    /// all. Files with nothing uncommitted are left alone.
    pub fn toggle_staged_for_directory(&mut self, dir: &str) {
        if !self.require_git_actions() {
            return;
        }
        let dir_path = Path::new(dir);
        let targets: Vec<(PathBuf, GitFileState)> = self
            .diff_files
            .iter()
            .filter(|file| self.file_matches_patterns(file))
            .map(|file| file.display_path().clone())
            .filter(|path| path.starts_with(dir_path))
            .filter_map(|path| {
                let state = self.git_file_states.get(&path).copied()?;
                (!state.committed()).then_some((path, state))
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        let stage = targets.iter().any(|(_, state)| state.unstaged);

        let mut changed: Vec<PathBuf> = Vec::new();
        for (path, state) in &targets {
            if stage != state.unstaged {
                continue;
            }
            let result = if stage {
                self.vcs.stage_file(path)
            } else {
                self.vcs.unstage_file(path)
            };
            if let Err(e) = result {
                let verb = if stage { "stage" } else { "unstage" };
                self.set_error(format!("Failed to {verb} {}: {e}", path.display()));
                changed.push(path.clone());
                self.after_staging_change(&changed);
                return;
            }
            changed.push(path.clone());
        }
        let verb = if stage { "Staged" } else { "Unstaged" };
        self.set_message(format!("{verb} {} file(s) in {dir}", changed.len()));
        self.after_staging_change(&changed);
    }

    /// `d` on a file: ask to discard its uncommitted changes, if it has any.
    pub fn request_discard_for_file_idx(&mut self, file_idx: usize) {
        if !self.require_git_actions() {
            return;
        }
        let Some(file) = self.diff_files.get(file_idx) else {
            return;
        };
        let path = file.display_path().clone();
        if self
            .git_file_states
            .get(&path)
            .is_none_or(|state| state.committed())
        {
            self.set_message(format!("{} has no uncommitted changes", path.display()));
            return;
        }
        let paths = self.discard_paths(std::slice::from_ref(&file_idx));
        self.ask_to_discard(DiscardRequest {
            target: path.display().to_string(),
            files: 1,
            paths,
        });
    }

    /// `d` on a directory: ask to discard the uncommitted changes of every file under it.
    pub fn request_discard_for_directory(&mut self, dir: &str) {
        if !self.require_git_actions() {
            return;
        }
        let dir_path = Path::new(dir);
        let file_idxs: Vec<usize> = self
            .diff_files
            .iter()
            .enumerate()
            .filter(|(_, file)| self.file_matches_patterns(file))
            .filter(|(_, file)| file.display_path().starts_with(dir_path))
            .filter(|(_, file)| {
                self.git_file_states
                    .get(file.display_path())
                    .is_some_and(|state| !state.committed())
            })
            .map(|(idx, _)| idx)
            .collect();
        if file_idxs.is_empty() {
            self.set_message(format!("Nothing uncommitted in {dir}"));
            return;
        }
        let paths = self.discard_paths(&file_idxs);
        self.ask_to_discard(DiscardRequest {
            target: format!("{}/", dir.trim_end_matches('/')),
            files: file_idxs.len(),
            paths,
        });
    }

    /// Paths to discard: each file's own, plus an uncommitted rename's old side, which must come
    /// back.
    fn discard_paths(&self, file_idxs: &[usize]) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for file in file_idxs.iter().filter_map(|idx| self.diff_files.get(*idx)) {
            let path = file.display_path().clone();
            if let Some(old) = file.old_path.as_ref().filter(|old| **old != path)
                && self
                    .vcs
                    .path_change_status(old)
                    .is_ok_and(|status| status.staged || status.unstaged)
            {
                paths.push(old.clone());
            }
            paths.push(path);
        }
        paths
    }

    fn ask_to_discard(&mut self, request: DiscardRequest) {
        self.pending_discard = Some(request);
        self.enter_confirm_mode(ConfirmAction::DiscardChanges);
    }

    /// `y` on the confirmation: discard, then reload, since the branch diff ends at the working
    /// tree.
    pub fn confirm_discard(&mut self) {
        let Some(request) = self.pending_discard.take() else {
            return;
        };
        let mut failure = None;
        for path in &request.paths {
            if let Err(e) = self.vcs.discard_file(path) {
                failure = Some(format!("Failed to discard {}: {e}", path.display()));
                break;
            }
        }
        let reloaded = self.reload_diff_files();
        match (failure, reloaded) {
            (Some(error), _) => self.set_error(error),
            (None, Err(e)) => self.set_error(format!("Discarded, but the reload failed: {e}")),
            (None, Ok(_)) => {
                let what = match request.files {
                    1 if !request.target.ends_with('/') => request.target,
                    1 => format!("1 file in {}", request.target),
                    n => format!("{n} files in {}", request.target),
                };
                self.set_message(format!("Discarded the changes to {what}"));
            }
        }
    }

    /// Update only what staging `paths` changed: their states, probed per path, and the commit
    /// pane's staged/unstaged rows. Commits cannot have changed, so they are not re-read.
    fn after_staging_change(&mut self, paths: &[PathBuf]) {
        for path in paths {
            match self.vcs.path_change_status(path) {
                Ok(status) => {
                    let state = GitFileState {
                        staged: status.staged,
                        unstaged: status.unstaged,
                    };
                    if state.committed() {
                        self.git_file_states.remove(path);
                    } else {
                        self.git_file_states.insert(path.clone(), state);
                    }
                }
                // No cheap probe: read the whole status once.
                Err(_) => {
                    self.refresh_git_file_states();
                    break;
                }
            }
        }
        self.refresh_staging_rows();
    }

    /// Sync the commit pane's staged/unstaged rows with the file states. Only files in the review
    /// count, matching the full status check's ignore rules.
    fn refresh_staging_rows(&mut self) {
        let in_review: HashSet<&PathBuf> = self
            .diff_files
            .iter()
            .map(|file| file.display_path())
            .collect();
        let mut status = crate::vcs::traits::VcsChangeStatus {
            staged: false,
            unstaged: false,
        };
        for (path, state) in &self.git_file_states {
            if in_review.contains(path) {
                status.staged |= state.staged;
                status.unstaged |= state.unstaged;
            }
        }
        let history: Vec<CommitInfo> = self
            .review_commits
            .iter()
            .filter(|commit| !Self::is_special_commit(commit))
            .cloned()
            .collect();
        let rows = Self::commit_pane_rows(&self.review_commits, history, Some(status));
        self.apply_branch_commit_rows(rows);

        // Cached narrowed diffs may hold the old index. Only a selection narrowed onto the staged
        // or unstaged row shows the index, so only then is the visible diff reloaded.
        self.commit_diff_cache.clear();
        let specials = self
            .review_commits
            .iter()
            .take_while(|commit| Self::is_special_commit(commit))
            .count();
        let narrowed_onto_index = Self::is_strict_commit_selection(
            self.commit_selection_range,
            self.review_commits.len(),
        ) && self
            .commit_selection_range
            .is_some_and(|(start, _)| start < specials);
        if narrowed_onto_index && let Err(e) = self.reload_inline_selection() {
            self.set_error(format!("Failed to reload the diff: {e}"));
        }
    }

    /// True when the tree shows only files with uncommitted changes.
    pub fn only_uncommitted(&self) -> bool {
        self.file_filter.only_uncommitted
    }

    /// `:uncommitted`: toggle showing only files with something left to commit.
    pub fn toggle_only_uncommitted(&mut self) {
        if !self.require_git_actions() {
            return;
        }
        self.file_filter.only_uncommitted = !self.file_filter.only_uncommitted;
        self.apply_file_filter_change();
        if self.file_filter.only_uncommitted {
            let shown = self
                .diff_files
                .iter()
                .filter(|file| self.file_passes_filter(file))
                .count();
            self.set_message(format!(
                "Showing {shown} file(s) with uncommitted changes \u{00b7} :uncommitted shows all"
            ));
        } else {
            self.set_message(format!("Showing all {} files", self.file_count()));
        }
    }

    /// Whether `path` passes the `:uncommitted` filter.
    pub(in crate::app) fn passes_uncommitted_filter(&self, path: &Path) -> bool {
        !self.file_filter.only_uncommitted
            || self
                .git_file_state(path)
                .is_some_and(|state| !state.committed())
    }
}
