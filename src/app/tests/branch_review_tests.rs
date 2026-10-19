//! `--branch`: one session per branch, its range following new commits.
//! Driven through `new_branch_review` and `reload_diff_files` (startup and
//! `:e`) against a VCS whose commits the test changes between calls.

use crate::app::*;
use crate::model::FileStatus;
use crate::vcs::traits::{
    ChangeKind, ResolvedRevisionRange, RevisionDiffTarget, VcsChangeStatus, VcsType,
};
use std::sync::{Arc, Mutex};

struct TestReviewsDir {
    _dir: tempfile::TempDir,
}

impl TestReviewsDir {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create test reviews dir");
        crate::persistence::storage::set_test_reviews_dir(Some(dir.path().to_path_buf()));
        Self { _dir: dir }
    }
}

impl Drop for TestReviewsDir {
    fn drop(&mut self) {
        crate::persistence::storage::set_test_reviews_dir(None);
    }
}

#[derive(Default)]
struct BranchState {
    /// Commits the branch has on top of its base, oldest first.
    commit_ids: Vec<String>,
    /// `master` is missing locally; only `origin/master` exists.
    local_base_missing: bool,
    /// Every revision expression the app asked to resolve.
    resolved: Vec<String>,
    /// Commit lists the commits + working-tree diff was asked for.
    combined_diffs: Vec<Vec<String>>,
    /// `committed.py`'s hash in the whole-branch diff, where an uncommitted
    /// edit shows.
    committed_hash: u64,
    /// Files the combined diff returns; empty means `committed.py` and `wip.py`.
    diff_paths: Vec<String>,
    /// Nothing uncommitted: the commit pane has no staged or unstaged row.
    clean: bool,
    /// Paths whose index differs from HEAD.
    staged: Vec<PathBuf>,
    /// Paths whose working tree differs from the index.
    unstaged: Vec<PathBuf>,
    /// `stage <path>` / `unstage <path>` for each call, in order.
    staging_calls: Vec<String>,
}

struct BranchVcs {
    info: VcsInfo,
    state: Arc<Mutex<BranchState>>,
}

impl VcsBackend for BranchVcs {
    fn info(&self) -> &VcsInfo {
        &self.info
    }

    fn get_working_tree_diff(&self, _highlighter: &SyntaxHighlighter) -> Result<Vec<DiffFile>> {
        Ok(vec![diff_file("wip.py")])
    }

    fn get_change_status(&self) -> Result<VcsChangeStatus> {
        Ok(VcsChangeStatus {
            staged: false,
            unstaged: !self.state.lock().unwrap().clean,
        })
    }

    fn resolve_revision_range(&self, revisions: &str) -> Result<ResolvedRevisionRange<'static>> {
        let mut state = self.state.lock().unwrap();
        state.resolved.push(revisions.to_string());
        if state.local_base_missing && revisions.starts_with("master...") {
            return Err(TuicrError::VcsCommand("unknown revision master".into()));
        }
        if state.commit_ids.is_empty() {
            return Err(TuicrError::NoChanges);
        }
        Ok(ResolvedRevisionRange::from_owned_commit_ids(
            state.commit_ids.clone(),
            RevisionDiffTarget::CommitList,
        ))
    }

    fn get_commits_info(&self, ids: &[String]) -> Result<Vec<CommitInfo>> {
        Ok(ids.iter().map(|id| commit(id)).collect())
    }

    fn get_working_tree_with_commits_diff(
        &self,
        commit_ids: &[String],
        _highlighter: &SyntaxHighlighter,
    ) -> Result<Vec<DiffFile>> {
        self.state
            .lock()
            .unwrap()
            .combined_diffs
            .push(commit_ids.to_vec());
        let state = self.state.lock().unwrap();
        if state.diff_paths.is_empty() {
            let mut committed = diff_file("committed.py");
            committed.content_hash = state.committed_hash;
            return Ok(vec![committed, diff_file("wip.py")]);
        }
        Ok(state.diff_paths.iter().map(|p| diff_file(p)).collect())
    }

    fn list_changed_paths(&self, kind: ChangeKind) -> Result<Vec<PathBuf>> {
        let state = self.state.lock().unwrap();
        Ok(match kind {
            ChangeKind::Staged => state.staged.clone(),
            ChangeKind::Unstaged => state.unstaged.clone(),
        })
    }

    fn path_change_status(&self, path: &Path) -> Result<VcsChangeStatus> {
        let state = self.state.lock().unwrap();
        Ok(VcsChangeStatus {
            staged: state.staged.iter().any(|p| p == path),
            unstaged: state.unstaged.iter().any(|p| p == path),
        })
    }

    /// Staging moves the whole file into the index, as `git add` does.
    fn stage_file(&self, path: &Path) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state
            .staging_calls
            .push(format!("stage {}", path.display()));
        state.unstaged.retain(|p| p != path);
        if !state.staged.iter().any(|p| p == path) {
            state.staged.push(path.to_path_buf());
        }
        Ok(())
    }

    fn unstage_file(&self, path: &Path) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state
            .staging_calls
            .push(format!("unstage {}", path.display()));
        state.staged.retain(|p| p != path);
        if !state.unstaged.iter().any(|p| p == path) {
            state.unstaged.push(path.to_path_buf());
        }
        Ok(())
    }

    /// A narrowed selection diffs only its commits.
    fn get_commit_range_diff(
        &self,
        _revision_range: &ResolvedRevisionRange<'_>,
        _highlighter: &SyntaxHighlighter,
    ) -> Result<Vec<DiffFile>> {
        Ok(vec![diff_file("committed.py")])
    }

    fn fetch_context_lines(
        &self,
        _file_path: &Path,
        _file_status: FileStatus,
        _ref_commit: Option<&str>,
        _start_line: u32,
        _end_line: u32,
    ) -> Result<Vec<DiffLine>> {
        Ok(Vec::new())
    }

    fn file_line_count(
        &self,
        _file_path: &Path,
        _file_status: FileStatus,
        _ref_commit: Option<&str>,
    ) -> Result<u32> {
        Ok(0)
    }
}

fn diff_file(path: &str) -> DiffFile {
    DiffFile {
        old_path: Some(PathBuf::from(path)),
        new_path: Some(PathBuf::from(path)),
        status: FileStatus::Modified,
        hunks: Vec::new(),
        is_binary: false,
        is_too_large: false,
        is_commit_message: false,
        content_hash: 0,
    }
}

fn commit(id: &str) -> CommitInfo {
    CommitInfo {
        id: id.to_string(),
        short_id: id.chars().take(7).collect(),
        branch_name: None,
        summary: format!("commit {id}"),
        body: None,
        author: "Test".to_string(),
        time: Utc::now(),
    }
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn open(state: &Arc<Mutex<BranchState>>, branch: Option<&str>) -> Result<App> {
    let vcs_info = VcsInfo {
        root_path: PathBuf::from("/tmp/tuicr-branch-review-test"),
        head_commit: "head".to_string(),
        branch_name: branch.map(str::to_string),
        vcs_type: VcsType::Git,
    };
    let options = AppStartupOptions {
        revisions: None,
        working_tree: false,
        path_filter: None,
        file_path: None,
        all_files: false,
        show_pr_checks: false,
        show_pr_comments: false,
        pr_comments_visibility: None,
        local_diff_fallback: false,
        git_backend_preference: crate::vcs::git::GitBackendPreference::Libgit2,
        diff_whitespace_mode: Default::default(),
        commit_selection: CommitSelectionStart::All,
        pr_target: None,
        repo_url_override: None,
        branch_base: Some("master"),
    };
    App::new_branch_review(
        Box::new(BranchVcs {
            info: vcs_info,
            state: Arc::clone(state),
        }),
        Theme::dark(),
        None,
        false,
        &options,
        "master",
    )
}

fn state_with(commit_ids: &[&str]) -> Arc<Mutex<BranchState>> {
    Arc::new(Mutex::new(BranchState {
        commit_ids: ids(commit_ids),
        ..Default::default()
    }))
}

fn commit_rows(app: &App) -> Vec<String> {
    app.review_commits
        .iter()
        .filter(|c| !App::is_special_commit(c))
        .map(|c| c.id.clone())
        .collect()
}

#[test]
fn should_open_branch_without_commits_as_its_uncommitted_changes() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&[]);

    let app = open(&state, Some("feature")).expect("open branch review");

    // HEAD is the merge-base, so the branch is exactly the working tree.
    assert_eq!(app.diff_source, DiffSource::StagedAndUnstaged);
    assert_eq!(app.session.diff_source, SessionDiffSource::Branch);
    assert_eq!(app.session.branch_base.as_deref(), Some("master"));
    assert_eq!(app.branch_base(), Some("master"));
    // Merge-base range, not `master..HEAD`.
    assert_eq!(state.lock().unwrap().resolved, ids(&["master...HEAD"]));
}

#[test]
fn should_take_in_new_commit_on_reload_without_changing_session() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&[]);
    let mut app = open(&state, Some("feature")).expect("open branch review");
    let session_id = app.session.id.clone();
    app.session.review_comments.push(Comment::new(
        "keep me".to_string(),
        CommentType::default(),
        None,
    ));

    // The user commits what they were reviewing.
    state.lock().unwrap().commit_ids = ids(&["c1"]);
    app.reload_diff_files().expect(":e");

    assert_eq!(
        app.diff_source,
        DiffSource::StagedUnstagedAndCommits(ids(&["c1"]))
    );
    assert_eq!(commit_rows(&app), ids(&["c1"]));
    assert_eq!(app.session.id, session_id);
    assert_eq!(app.session.review_comments.len(), 1);
    // The committed content is still in the review.
    assert!(
        app.diff_files
            .iter()
            .any(|f| f.display_path() == &PathBuf::from("committed.py"))
    );
}

#[test]
fn should_keep_every_commit_selected_when_one_is_added() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = open(&state, Some("feature")).expect("open branch review");
    assert!(!App::is_strict_commit_selection(
        app.commit_selection_range,
        app.review_commits.len()
    ));

    state.lock().unwrap().commit_ids = ids(&["c1", "c2"]);
    app.reload_diff_files().expect(":e");

    assert_eq!(commit_rows(&app), ids(&["c2", "c1"]));
    assert!(!App::is_strict_commit_selection(
        app.commit_selection_range,
        app.review_commits.len()
    ));
    assert_eq!(
        state.lock().unwrap().combined_diffs.last(),
        Some(&ids(&["c1", "c2"]))
    );
}

#[test]
fn should_keep_a_narrowed_selection_on_its_commit_when_one_is_added() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1", "c2"]);
    let mut app = open(&state, Some("feature")).expect("open branch review");
    let c1 = app
        .review_commits
        .iter()
        .position(|c| c.id == "c1")
        .unwrap();
    app.commit_selection_range = Some((c1, c1));

    state.lock().unwrap().commit_ids = ids(&["c1", "c2", "c3"]);
    app.reload_diff_files().expect(":e");

    let (start, end) = app.commit_selection_range.expect("selection kept");
    assert_eq!(start, end);
    assert_eq!(app.review_commits[start].id, "c1");
}

#[test]
fn should_unmark_a_file_edited_outside_a_narrowed_selection_on_reload() {
    // given: only the commits selected, not the "Unstaged changes" row
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1", "c2"]);
    let mut app = open(&state, Some("feature")).expect("open branch review");
    let row = |app: &App, id: &str| app.review_commits.iter().position(|c| c.id == id).unwrap();
    app.commit_selection_range = Some((row(&app, "c2"), row(&app, "c1")));
    app.reload_inline_selection().expect("narrow");
    let path = PathBuf::from("committed.py");
    app.session.get_file_mut(&path).unwrap().reviewed = true;

    // when: reloaded with nothing edited
    app.reload_diff_files().expect(":e");

    // then: the mark stays
    assert!(app.session.is_file_reviewed(&path));

    // when: edited in the working tree, outside the selection, and reloaded
    state.lock().unwrap().committed_hash = 7;
    app.reload_diff_files().expect(":e");

    // then: the whole branch shows the edit, so the mark goes
    assert!(!app.session.is_file_reviewed(&path));
}

#[test]
fn should_fall_back_to_origin_when_base_is_missing_locally() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    state.lock().unwrap().local_base_missing = true;

    let app = open(&state, Some("feature")).expect("open branch review");

    assert_eq!(
        state.lock().unwrap().resolved,
        ids(&["master...HEAD", "origin/master...HEAD"])
    );
    assert_eq!(commit_rows(&app), ids(&["c1"]));
}

#[test]
fn should_refuse_detached_head() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);

    let error = open(&state, None).err().expect("detached HEAD must fail");

    assert!(error.to_string().contains("detached"), "{error}");
}

#[test]
fn should_reopen_the_same_session_after_new_commits() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut first = open(&state, Some("feature")).expect("first run");
    first.session.review_comments.push(Comment::new(
        "from the first run".to_string(),
        CommentType::default(),
        None,
    ));
    first
        .save_current_session_merging_external()
        .expect("persist");
    let session_id = first.session.id.clone();
    drop(first);

    // Quit, commit twice, reopen: same branch, new range.
    state.lock().unwrap().commit_ids = ids(&["c1", "c2", "c3"]);
    let second = open(&state, Some("feature")).expect("second run");

    assert_eq!(second.session.id, session_id);
    assert_eq!(second.session.review_comments.len(), 1);
    assert_eq!(commit_rows(&second), ids(&["c3", "c2", "c1"]));

    // A different branch never sees it.
    let other = open(&state, Some("other")).expect("other branch");
    assert_ne!(other.session.id, session_id);
}

#[test]
fn should_untag_comments_whose_commit_was_rewritten() {
    let mut session = ReviewSession::new(
        PathBuf::from("/tmp"),
        "head".to_string(),
        Some("feature".to_string()),
        SessionDiffSource::Branch,
    );
    let tagged = |content: &str, commit: &str| {
        let mut comment = Comment::new(content.to_string(), CommentType::default(), None);
        comment.commit_id = Some(commit.to_string());
        comment
    };
    let survivor = tagged("on a commit still there", "c1");
    let orphan = tagged("on an amended commit", "gone");
    session.review_comments = vec![survivor, orphan];

    let untagged = App::untag_rewritten_commit_comments(&mut session, &ids(&["c1"]));

    assert_eq!(untagged, 1);
    assert_eq!(session.review_comments[0].commit_id.as_deref(), Some("c1"));
    assert_eq!(session.review_comments[1].commit_id, None);
}

#[test]
fn should_tell_narrowed_views_apart_only_in_branch_reviews() {
    // given: a branch review narrowed to its oldest commit
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1", "c2"]);
    let mut app = open(&state, Some("feature")).expect("open branch review");
    let oldest = app.review_commits.len() - 1;
    app.commit_selection_range = Some((oldest, oldest));

    // then: the narrowed view has its own key, by commit id
    assert_eq!(app.review_view_key(), "c1..c1");
    app.commit_selection_range = Some((0, oldest));
    assert_eq!(app.review_view_key(), FULL_REVIEW_VIEW);

    // when: the same narrowing in any other kind of review
    app.commit_selection_range = Some((oldest, oldest));
    app.session.diff_source = SessionDiffSource::CommitRange;

    // then: one view only, as upstream
    assert_eq!(app.review_view_key(), FULL_REVIEW_VIEW);
}

/// A branch review with git actions on: `committed.py` has no uncommitted
/// changes left, `wip.py` is unstaged, and `staged.py` is staged.
fn git_actions_app(state: &Arc<Mutex<BranchState>>) -> App {
    {
        let mut state = state.lock().unwrap();
        state.diff_paths = vec!["committed.py".into(), "staged.py".into(), "wip.py".into()];
        state.staged = vec![PathBuf::from("staged.py")];
        state.unstaged = vec![PathBuf::from("wip.py")];
    }
    let mut app = open(state, Some("feature")).expect("open branch review");
    app.git_actions = true;
    app.refresh_git_file_states();
    app
}

fn file_idx(app: &App, path: &str) -> usize {
    app.diff_files
        .iter()
        .position(|file| file.display_path() == Path::new(path))
        .expect("file in diff")
}

fn staging_calls(state: &Arc<Mutex<BranchState>>) -> Vec<String> {
    state.lock().unwrap().staging_calls.clone()
}

#[test]
fn should_report_each_files_git_state_with_git_actions() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let app = git_actions_app(&state);

    let state_of = |path: &str| app.git_file_state(Path::new(path)).unwrap();
    assert!(state_of("committed.py").committed());
    assert!(state_of("staged.py").staged && !state_of("staged.py").unstaged);
    assert!(state_of("wip.py").unstaged && !state_of("wip.py").staged);
}

#[test]
fn should_report_no_git_state_without_git_actions() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);
    app.git_actions = false;

    assert_eq!(app.git_file_state(Path::new("wip.py")), None);
    app.toggle_staged_for_file_idx(file_idx(&app, "wip.py"));

    assert!(staging_calls(&state).is_empty());
    assert!(
        app.message
            .as_ref()
            .unwrap()
            .content
            .contains("git_actions")
    );
}

#[test]
fn should_stage_an_unstaged_file_and_unstage_a_staged_one() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);

    app.toggle_staged_for_file_idx(file_idx(&app, "wip.py"));
    app.toggle_staged_for_file_idx(file_idx(&app, "staged.py"));

    assert_eq!(staging_calls(&state), ["stage wip.py", "unstage staged.py"]);
    assert!(app.git_file_state(Path::new("wip.py")).unwrap().staged);
    assert!(app.git_file_state(Path::new("staged.py")).unwrap().unstaged);
    assert!(
        app.message
            .as_ref()
            .unwrap()
            .content
            .contains("Unstaged staged.py")
    );
}

#[test]
fn should_leave_a_file_without_uncommitted_changes_alone() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);

    app.toggle_staged_for_file_idx(file_idx(&app, "committed.py"));

    assert!(staging_calls(&state).is_empty());
}

#[test]
fn should_stage_a_directory_then_unstage_it() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    {
        let mut state = state.lock().unwrap();
        state.diff_paths = vec!["src/a.py".into(), "src/b.py".into(), "src/done.py".into()];
        state.staged = vec![PathBuf::from("src/b.py")];
        state.unstaged = vec![PathBuf::from("src/a.py")];
    }
    let mut app = open(&state, Some("feature")).expect("open branch review");
    app.git_actions = true;
    app.refresh_git_file_states();

    // Something under src/ is unstaged, so the directory gets staged.
    app.toggle_staged_for_directory("src");
    assert_eq!(staging_calls(&state), ["stage src/a.py"]);

    // Now all of it is staged, so the directory gets unstaged.
    app.toggle_staged_for_directory("src");
    assert_eq!(
        staging_calls(&state),
        ["stage src/a.py", "unstage src/a.py", "unstage src/b.py"]
    );
}

#[test]
fn should_show_only_files_with_uncommitted_changes() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);

    app.toggle_only_uncommitted();

    let shown: Vec<_> = app
        .filtered_file_indices()
        .into_iter()
        .map(|idx| app.diff_files[idx].display_path().display().to_string())
        .collect();
    assert_eq!(shown, ["staged.py", "wip.py"]);

    app.toggle_only_uncommitted();
    assert_eq!(app.filtered_file_indices().len(), 3);
}

fn staging_rows(app: &App) -> Vec<String> {
    app.review_commits
        .iter()
        .filter(|c| App::is_special_commit(c))
        .map(|c| c.summary.clone())
        .collect()
}

#[test]
fn should_update_only_the_staging_rows_after_staging() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);
    app.toggle_staged_for_file_idx(file_idx(&app, "wip.py"));
    let resolved_before = state.lock().unwrap().resolved.len();

    // wip.py was the only unstaged file: its row goes, the staged one stays.
    assert_eq!(staging_rows(&app), ["Staged changes"]);

    app.toggle_staged_for_file_idx(file_idx(&app, "wip.py"));
    assert_eq!(staging_rows(&app), ["Staged changes", "Unstaged changes"]);
    assert_eq!(commit_rows(&app), ["c1"]);
    // Commits were not re-read.
    assert_eq!(state.lock().unwrap().resolved.len(), resolved_before);
}

fn press(app: &mut App, code: crossterm::event::KeyCode) {
    crate::handler::handle_commit_message_key(
        app,
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
    );
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, crossterm::event::KeyCode::Char(c));
    }
}

#[test]
fn should_refuse_to_open_the_commit_dialog_with_nothing_staged() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);
    app.toggle_staged_for_file_idx(file_idx(&app, "staged.py"));

    app.open_commit_dialog();

    assert_eq!(app.input_mode, InputMode::Normal);
    assert!(
        app.message
            .as_ref()
            .unwrap()
            .content
            .contains("Nothing staged")
    );
}

#[test]
fn should_write_a_title_and_a_multi_line_description() {
    use crossterm::event::KeyCode;
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);

    app.open_commit_dialog();
    assert_eq!(app.input_mode, InputMode::CommitMessage);
    type_text(&mut app, "Fix it");
    press(&mut app, KeyCode::Tab);
    type_text(&mut app, "first");
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "second");
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::End);
    type_text(&mut app, "!");

    let draft = app.commit_draft.as_ref().unwrap();
    assert_eq!(draft.title, "Fix it");
    assert_eq!(draft.description, "first!\nsecond");
    assert_eq!(draft.message(), "Fix it\n\nfirst!\nsecond");
    // Enter in the description adds a line; it does not commit.
    assert_eq!(app.input_mode, InputMode::CommitMessage);
}

#[test]
fn should_refuse_to_commit_without_a_title() {
    use crossterm::event::KeyCode;
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);
    app.open_commit_dialog();

    press(&mut app, KeyCode::Enter);

    assert_eq!(app.input_mode, InputMode::CommitMessage);
    assert!(app.commit_rx.is_none());
    assert!(
        app.message
            .as_ref()
            .unwrap()
            .content
            .contains("needs a title")
    );
}

#[test]
fn should_keep_the_draft_when_the_dialog_closes() {
    use crossterm::event::KeyCode;
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);
    app.open_commit_dialog();
    type_text(&mut app, "Later");

    press(&mut app, KeyCode::Esc);
    assert_eq!(app.input_mode, InputMode::Normal);
    app.open_commit_dialog();

    assert_eq!(app.commit_draft.as_ref().unwrap().title, "Later");
}

#[test]
fn should_clear_the_draft_and_refresh_after_a_commit() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);
    app.open_commit_dialog();
    type_text(&mut app, "Ship it");
    app.close_commit_dialog();
    {
        // The repository after committing staged.py.
        let mut state = state.lock().unwrap();
        state.commit_ids.push("c2".into());
        state.staged.clear();
    }

    app.finish_commit(Ok("c2abcdef0123".into()));

    assert!(app.commit_draft.is_none());
    assert_eq!(commit_rows(&app), ["c2", "c1"]);
    assert!(
        app.git_file_state(Path::new("staged.py"))
            .unwrap()
            .committed()
    );
    assert!(
        app.message
            .as_ref()
            .unwrap()
            .content
            .contains("Committed c2abcde Ship it")
    );
}

#[test]
fn should_show_the_whole_branch_after_committing_from_the_uncommitted_rows() {
    // given: the review narrowed to its unstaged row
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);
    let unstaged = app
        .review_commits
        .iter()
        .position(App::is_unstaged_commit)
        .expect("unstaged row");
    app.commit_selection_range = Some((unstaged, unstaged));
    app.reload_diff_files().expect(":e");
    assert_eq!(app.diff_files.len(), 1);
    app.open_commit_dialog();
    type_text(&mut app, "Ship it");
    app.close_commit_dialog();

    // when: committing everything uncommitted
    {
        let mut state = state.lock().unwrap();
        state.commit_ids.push("c2".into());
        state.staged.clear();
        state.unstaged.clear();
        state.clean = true;
    }
    app.finish_commit(Ok("c2abcdef0123".into()));

    // then: the unstaged row is gone and the tree widens to every commit
    assert_eq!(commit_rows(&app), ["c2", "c1"]);
    assert!(!App::is_strict_commit_selection(
        app.commit_selection_range,
        app.review_commits.len()
    ));
    assert_eq!(app.diff_files.len(), 3);
}

#[test]
fn should_keep_the_draft_when_a_commit_fails() {
    let _reviews = TestReviewsDir::new();
    let state = state_with(&["c1"]);
    let mut app = git_actions_app(&state);
    app.open_commit_dialog();
    type_text(&mut app, "Ship it");
    app.close_commit_dialog();

    app.finish_commit(Err("hook says no".into()));

    assert_eq!(app.commit_draft.as_ref().unwrap().title, "Ship it");
    assert!(
        app.message
            .as_ref()
            .unwrap()
            .content
            .contains("hook says no")
    );
}
