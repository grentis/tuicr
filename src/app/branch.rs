//! `--branch`: one review session per branch.
//!
//! Other local sources are keyed by their commits, so committing or amending
//! opens a new session. A branch review is keyed by the branch alone
//! (`<repo>@<branch>/branch`), and its range (everything since HEAD left its
//! base, up to the working tree) is re-resolved on every reload.
//!
//! It reuses the existing sources: `StagedAndUnstaged` while the branch has
//! no commits, else `StagedUnstagedAndCommits`, whose diff starts at the
//! merge-base. The range, the diff and the session are the library's
//! (`crate::branch`); this is the App around them: the commit pane, the
//! selection, the reload.

use super::diff_load::CommitSelectionAnchor;
use super::*;
use crate::vcs::traits::VcsChangeStatus;

impl App {
    /// Open a `--branch` review of the current branch against `base`.
    pub(in crate::app) fn new_branch_review(
        vcs: Box<dyn VcsBackend>,
        theme: Theme,
        comment_type_configs: Option<Vec<CommentTypeConfig>>,
        output_to_stdout: bool,
        options: &AppStartupOptions<'_>,
        base: &str,
    ) -> Result<Self> {
        let vcs_info = vcs.info().clone();
        crate::branch::require_branch_checkout(&vcs_info)?;
        let highlighter = theme.syntax_highlighter();

        let commit_ids = crate::branch::branch_commit_ids(vcs.as_ref(), base)?;
        let diff_source = crate::branch::branch_diff_source(commit_ids.clone());
        let diff_files = crate::branch::branch_diff_files(
            vcs.as_ref(),
            &vcs_info.root_path,
            &diff_source,
            highlighter,
            options.path_filter,
        )?;
        let change_status = Self::get_change_status_with_ignore(
            vcs.as_ref(),
            &vcs_info.root_path,
            highlighter,
            options.path_filter,
        )?;
        let rows = Self::branch_commit_rows(vcs.as_ref(), &commit_ids, Some(change_status))?;
        let session = crate::branch::load_or_create_branch_session(&vcs_info, base, &commit_ids);

        let mut app = Self::build(
            vcs,
            vcs_info,
            theme,
            comment_type_configs,
            output_to_stdout,
            diff_files,
            session,
            diff_source,
            InputMode::Normal,
            Vec::new(),
            options.path_filter,
            options.repo_url_override.clone(),
        )?
        .with_vcs_open_options(options.vcs_open_options());

        app.install_branch_commit_rows(rows, options.commit_selection)?;
        if app.diff_files.is_empty() {
            let branch = app.vcs_info.branch_name.clone().unwrap_or_default();
            app.set_message(format!("No changes on {branch} since {base} yet"));
        }
        Ok(app)
    }

    /// Switch the running app to a `--branch` review (resuming from the
    /// Sessions tab).
    pub(in crate::app) fn load_branch_selection(&mut self, base: &str) -> Result<()> {
        crate::branch::require_branch_checkout(&self.vcs_info)?;
        let highlighter = self.theme.syntax_highlighter();
        let commit_ids = crate::branch::branch_commit_ids(self.vcs.as_ref(), base)?;
        let diff_source = crate::branch::branch_diff_source(commit_ids.clone());
        let diff_files = crate::branch::branch_diff_files(
            self.vcs.as_ref(),
            &self.vcs_info.root_path,
            &diff_source,
            highlighter,
            self.path_filter.as_deref(),
        )?;
        let change_status = Self::get_change_status_with_ignore(
            self.vcs.as_ref(),
            &self.vcs_info.root_path,
            highlighter,
            self.path_filter.as_deref(),
        )?;
        let rows = Self::branch_commit_rows(self.vcs.as_ref(), &commit_ids, Some(change_status))?;

        self.session =
            crate::branch::load_or_create_branch_session(&self.vcs_info, base, &commit_ids);
        for file in &diff_files {
            self.session.add_diff_file(file);
        }
        self.reset_persisted_session_tracking()?;

        self.diff_files = diff_files;
        self.diff_source = diff_source;
        self.input_mode = InputMode::Normal;
        self.diff_state = DiffState::default();
        self.file_list_state = FileListState::default();
        self.pr_commits.clear();
        self.pr_last_reviewed_commit_index = None;
        self.saved_inline_selection = None;
        self.install_branch_commit_rows(rows, self.commit_selection_start)
    }

    /// The active branch review's base; `None` for any other session. Read
    /// from the session, so switching targets needs no reset.
    pub(in crate::app) fn branch_base(&self) -> Option<&str> {
        if self.session.diff_source != SessionDiffSource::Branch {
            return None;
        }
        self.session.branch_base.as_deref()
    }

    /// Re-resolve the branch range into the commit pane and diff source.
    /// Returns whether anything changed. `with_status` also re-reads the
    /// staged/unstaged rows (`:e`); the diff watcher's worker does that
    /// itself.
    pub(in crate::app) fn refresh_branch_target(&mut self, with_status: bool) -> Result<bool> {
        let Some(base) = self.branch_base().map(str::to_owned) else {
            return Ok(false);
        };
        if with_status {
            self.refresh_git_file_states();
        }
        let commit_ids = crate::branch::branch_commit_ids(self.vcs.as_ref(), &base)?;
        let status = if with_status {
            Some(Self::get_change_status_with_ignore(
                self.vcs.as_ref(),
                &self.vcs_info.root_path,
                self.theme.syntax_highlighter(),
                self.path_filter.as_deref(),
            )?)
        } else {
            None
        };

        let diff_source = crate::branch::branch_diff_source(commit_ids.clone());
        let history = Self::branch_history(self.vcs.as_ref(), &commit_ids)?;
        let rows = Self::commit_pane_rows(&self.review_commits, history, status);
        if diff_source == self.diff_source && rows == self.review_commits {
            return Ok(false);
        }

        self.apply_branch_commit_rows(rows);

        self.diff_source = diff_source;
        self.session.commit_range = (!commit_ids.is_empty()).then(|| commit_ids.clone());
        if crate::branch::untag_rewritten_commit_comments(&mut self.session, &commit_ids) > 0 {
            self.dirty = true;
        }
        Ok(true)
    }

    /// Install refreshed commit rows. An all-commits selection takes new commits; a narrowed one
    /// keeps its own, or widens to the whole branch once they are gone (rebase, amend).
    pub(in crate::app) fn apply_branch_commit_rows(&mut self, rows: Vec<CommitInfo>) {
        if rows == self.review_commits {
            return;
        }
        let range = match Self::reanchored_commit_selection(
            self.commit_selection_range,
            &self.review_commits,
            &rows,
        ) {
            CommitSelectionAnchor::Moved(range) => range,
            CommitSelectionAnchor::Lost => (!rows.is_empty()).then(|| (0, rows.len() - 1)),
        };
        let had_selector = self.review_commits.len() > 1;
        self.install_refreshed_commit_pane(rows, range);
        if !had_selector && self.review_commits.len() > 1 {
            self.show_commit_selector = true;
        }
    }

    /// The branch commits as the pane lists them, newest first.
    fn branch_history(vcs: &dyn VcsBackend, commit_ids: &[String]) -> Result<Vec<CommitInfo>> {
        if commit_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vcs
            .get_commits_info(commit_ids)?
            .into_iter()
            .rev()
            .collect())
    }

    /// A fresh branch review's commit pane: staged/unstaged rows, then the
    /// branch commits.
    fn branch_commit_rows(
        vcs: &dyn VcsBackend,
        commit_ids: &[String],
        status: Option<VcsChangeStatus>,
    ) -> Result<Vec<CommitInfo>> {
        let history = Self::branch_history(vcs, commit_ids)?;
        Ok(Self::commit_pane_rows(&[], history, status))
    }

    /// Install the pane and initial selection, then finalize the diff as the
    /// `-r … -w` startup does.
    fn install_branch_commit_rows(
        &mut self,
        rows: Vec<CommitInfo>,
        selection: CommitSelectionStart,
    ) -> Result<()> {
        self.range_diff_files = Some(self.diff_files.clone());
        let range = Self::initial_commit_range(selection, rows.len());
        self.commit_list = rows.clone();
        self.commit_selection_range = range;
        self.commit_list_cursor = range.map(|(start, _)| start).unwrap_or(0);
        self.commit_list_scroll_offset = 0;
        self.visible_commit_count = rows.len();
        self.has_more_commit = false;
        self.show_commit_selector = rows.len() > 1;
        self.commit_diff_cache.clear();
        self.review_commits = rows;

        if Self::is_strict_commit_selection(self.commit_selection_range, self.review_commits.len())
        {
            self.reload_inline_selection()?;
        } else {
            self.insert_commit_message_if_single();
            self.sort_files_by_directory(true);
            self.expand_all_dirs();
            self.rebuild_annotations();
        }
        Ok(())
    }
}
