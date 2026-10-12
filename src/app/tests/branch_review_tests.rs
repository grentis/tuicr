//! `--branch`: one session per branch, its range following new commits.
//! Driven through `new_branch_review` and `reload_diff_files` (startup and
//! `:e`) against a VCS whose commits the test changes between calls.

use crate::app::*;
use crate::model::FileStatus;
use crate::vcs::traits::{ResolvedRevisionRange, RevisionDiffTarget, VcsChangeStatus, VcsType};
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
            unstaged: true,
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
        let mut committed = diff_file("committed.py");
        committed.content_hash = self.state.lock().unwrap().committed_hash;
        Ok(vec![committed, diff_file("wip.py")])
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
