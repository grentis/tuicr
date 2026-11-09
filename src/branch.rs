//! A branch review, as `tuicr -b` opens it: everything since the branch left
//! its base, up to the working tree, uncommitted changes included.
//!
//! The session is keyed by the branch alone (`<repo>@<branch>/branch`), so
//! committing or amending keeps it, and every program that opens the branch
//! this way, tuicr included, sees the same comments. The range is resolved
//! again on every read: `base...HEAD`, so the diff starts at the merge-base.
//!
//! [`BranchReview`] is the whole of it for programs that use tuicr as a
//! library; the free functions are its parts, which tuicr's App uses too.

use std::path::Path;

use chrono::Utc;

use crate::app::{App, DiffSource, STAGED_SELECTION_ID, UNSTAGED_SELECTION_ID};
use crate::error::{Result, TuicrError};
use crate::model::{DiffFile, ReviewSession, SessionDiffSource};
use crate::persistence::load_latest_session_for_context;
use crate::syntax::SyntaxHighlighter;
use crate::vcs::traits::VcsType;
use crate::vcs::{
    CommitInfo, DiffWhitespaceMode, GitBackend, GitBackendPreference, VcsBackend, VcsChangeStatus,
    VcsInfo,
};

/// The base of a bare `--branch` in the current directory's checkout: git
/// config `tuicr.branchBase`, then `config_base` (tuicr's `branch_base`),
/// then `origin/HEAD`'s branch, then `main`, then `master`.
pub fn default_branch_base(config_base: Option<&str>) -> String {
    default_branch_base_in(Path::new("."), config_base)
}

/// [`default_branch_base`] for the checkout at `checkout`.
pub fn default_branch_base_in(checkout: &Path, config_base: Option<&str>) -> String {
    branch_base_from(config_base, |args| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(checkout)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
        output.status.success().then_some(out)
    })
}

/// `default_branch_base` with `git` running a git command: trimmed stdout,
/// `None` on failure.
fn branch_base_from(config_base: Option<&str>, git: impl Fn(&[&str]) -> Option<String>) -> String {
    let named = |base: Option<String>| base.filter(|base| !base.is_empty());
    named(git(&["config", "--get", "tuicr.branchBase"]))
        .or_else(|| named(config_base.map(str::to_string)))
        .or_else(|| {
            let head = git(&[
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ])?;
            named(head.strip_prefix("origin/").map(str::to_string))
        })
        .or_else(|| {
            git(&["rev-parse", "--verify", "--quiet", "refs/heads/main"]).map(|_| "main".into())
        })
        .unwrap_or_else(|| "master".to_string())
}

/// A branch review needs git and a checked-out branch.
pub fn require_branch_checkout(vcs_info: &VcsInfo) -> Result<()> {
    if vcs_info.vcs_type != VcsType::Git {
        return Err(TuicrError::UnsupportedOperation(
            "--branch is only supported in git repositories".to_string(),
        ));
    }
    if vcs_info.branch_name.is_none() {
        return Err(TuicrError::UnsupportedOperation(
            "--branch needs a checked-out branch, but HEAD is detached".to_string(),
        ));
    }
    Ok(())
}

/// Commits HEAD has on top of `base`, oldest first. `base...HEAD`, so the
/// diff starts at the merge-base even when the base moved on. A base
/// missing locally falls back to `origin/<base>`.
pub fn branch_commit_ids(vcs: &dyn VcsBackend, base: &str) -> Result<Vec<String>> {
    let resolve = |base: &str| vcs.resolve_revision_range(&format!("{base}...HEAD"));
    let range = match resolve(base) {
        Ok(range) => range,
        Err(TuicrError::NoChanges) => return Ok(Vec::new()),
        Err(error) if !base.starts_with("origin/") => match resolve(&format!("origin/{base}")) {
            Ok(range) => range,
            Err(TuicrError::NoChanges) => return Ok(Vec::new()),
            Err(_) => return Err(error),
        },
        Err(error) => return Err(error),
    };
    Ok(range.commit_ids.into_owned())
}

/// The source diffing the merge-base against the working tree: staged and
/// unstaged changes alone while the branch has no commits.
pub fn branch_diff_source(commit_ids: Vec<String>) -> DiffSource {
    if commit_ids.is_empty() {
        DiffSource::StagedAndUnstaged
    } else {
        DiffSource::StagedUnstagedAndCommits(commit_ids)
    }
}

/// The branch's diff, `.tuicrignore` and `path_filter` applied; a branch
/// with nothing on it yet reads as empty rather than failing.
pub fn branch_diff_files(
    vcs: &dyn VcsBackend,
    root_path: &Path,
    diff_source: &DiffSource,
    highlighter: &SyntaxHighlighter,
    path_filter: Option<&str>,
) -> Result<Vec<DiffFile>> {
    match App::fetch_diff_files_for_source(vcs, root_path, diff_source, highlighter, path_filter) {
        Err(TuicrError::NoChanges) => Ok(Vec::new()),
        other => other,
    }
}

/// Load or create the branch's session, keyed by the branch alone;
/// `commit_ids` only refreshes what it records. Not saved.
pub fn load_or_create_branch_session(
    vcs_info: &VcsInfo,
    base: &str,
    commit_ids: &[String],
) -> ReviewSession {
    let loaded = load_latest_session_for_context(
        &vcs_info.root_path,
        vcs_info.branch_name.as_deref(),
        &vcs_info.head_commit,
        SessionDiffSource::Branch,
        None,
    )
    .ok()
    .flatten()
    .map(|(_path, session)| session);

    let mut session = loaded.unwrap_or_else(|| {
        ReviewSession::new(
            vcs_info.root_path.clone(),
            vcs_info.head_commit.clone(),
            vcs_info.branch_name.clone(),
            SessionDiffSource::Branch,
        )
    });
    session.branch_base = Some(base.to_string());
    session.base_commit = vcs_info.head_commit.clone();
    session.commit_range = (!commit_ids.is_empty()).then(|| commit_ids.to_vec());
    untag_rewritten_commit_comments(&mut session, commit_ids);
    session
}

/// Untag comments whose commit a rebase or amend removed, so they show on
/// the full diff instead of never. Returns how many changed.
pub fn untag_rewritten_commit_comments(
    session: &mut ReviewSession,
    commit_ids: &[String],
) -> usize {
    let file_comments = session.files.values_mut().flat_map(|review| {
        review
            .file_comments
            .iter_mut()
            .chain(review.line_comments.values_mut().flatten())
    });
    let mut untagged = 0;
    for comment in session.review_comments.iter_mut().chain(file_comments) {
        if comment
            .commit_id
            .as_ref()
            .is_some_and(|id| !commit_ids.contains(id))
        {
            comment.commit_id = None;
            untagged += 1;
        }
    }
    untagged
}

/// The commit pane's row for the staged changes: a commit by its id only,
/// [`STAGED_SELECTION_ID`], which no real commit has.
pub fn staged_entry() -> CommitInfo {
    CommitInfo {
        id: STAGED_SELECTION_ID.to_string(),
        short_id: "STAGED".to_string(),
        branch_name: None,
        summary: "Staged changes".to_string(),
        body: None,
        author: String::new(),
        time: Utc::now(),
    }
}

/// The commit pane's row for the unstaged changes, as [`staged_entry`].
pub fn unstaged_entry() -> CommitInfo {
    CommitInfo {
        id: UNSTAGED_SELECTION_ID.to_string(),
        short_id: "UNSTAGED".to_string(),
        branch_name: None,
        summary: "Unstaged changes".to_string(),
        body: None,
        author: String::new(),
        time: Utc::now(),
    }
}

pub fn is_staged(commit: &CommitInfo) -> bool {
    commit.id == STAGED_SELECTION_ID
}

pub fn is_unstaged(commit: &CommitInfo) -> bool {
    commit.id == UNSTAGED_SELECTION_ID
}

/// A row standing for staged or unstaged changes, not a commit.
pub fn is_special(commit: &CommitInfo) -> bool {
    is_staged(commit) || is_unstaged(commit)
}

/// The commit pane's rows, newest first: the staged and unstaged rows
/// `status` earns, then `fetched`'s commits. The special rows already in
/// `current` keep their order, one appearing goes after them, and with no
/// `status` they are left as they are (see `App::commit_pane_rows`).
pub fn commit_pane_rows(
    current: &[CommitInfo],
    fetched: Vec<CommitInfo>,
    status: Option<VcsChangeStatus>,
) -> Vec<CommitInfo> {
    let existing = current.iter().take_while(|commit| is_special(commit));
    let mut rows: Vec<CommitInfo> = match status {
        None => existing.cloned().collect(),
        Some(status) => {
            let mut kept: Vec<CommitInfo> = existing
                .filter(|commit| special_row_still_earned(commit, status))
                .cloned()
                .collect();
            if status.staged && !kept.iter().any(is_staged) {
                kept.push(staged_entry());
            }
            if status.unstaged && !kept.iter().any(is_unstaged) {
                kept.push(unstaged_entry());
            }
            kept
        }
    };
    rows.extend(fetched);
    rows
}

/// Whether the tree still holds what a special row stands for. A row this
/// does not recognise is kept: reading "not staged" as "therefore unstaged"
/// would drop a third kind the day one is added.
fn special_row_still_earned(commit: &CommitInfo, status: VcsChangeStatus) -> bool {
    match commit {
        row if is_staged(row) => status.staged,
        row if is_unstaged(row) => status.unstaged,
        _ => true,
    }
}

/// Which diff the rows `start..=end` of the commit pane (newest first) mean:
/// commits alone a commit range; the staged or unstaged row with commits,
/// those commits and the working tree; the special rows alone, the staged,
/// the unstaged, or both.
pub fn source_for_commit_subrange(rows: &[CommitInfo], start: usize, end: usize) -> DiffSource {
    let has_staged = (start..=end).any(|i| rows.get(i).is_some_and(is_staged));
    let has_unstaged = (start..=end).any(|i| rows.get(i).is_some_and(is_unstaged));
    let selected_ids: Vec<String> = (start..=end)
        .rev() // oldest to newest, matching every other DiffSource commit list
        .filter_map(|i| rows.get(i))
        .filter(|c| !is_special(c))
        .map(|c| c.id.clone())
        .collect();

    // Matched on the three facts that decide the answer, so every case is
    // visible at once and the compiler rejects a missing one.
    match (has_staged, has_unstaged, selected_ids.is_empty()) {
        // No staged or unstaged entry in the selection, so it is only commits.
        (false, false, _) => DiffSource::CommitRange(selected_ids),
        // A special entry plus real commits.
        (_, _, false) => DiffSource::StagedUnstagedAndCommits(selected_ids),
        // Special entries only, no commits alongside them.
        (true, true, true) => DiffSource::StagedAndUnstaged,
        (true, false, true) => DiffSource::Staged,
        (false, true, true) => DiffSource::Unstaged,
    }
}

/// A branch review open in a checkout: the diff from the merge-base to the
/// working tree, and the session `tuicr -b` keeps for the branch.
pub struct BranchReview {
    vcs: Box<dyn VcsBackend>,
    /// The base the branch is compared with.
    pub base: String,
    /// The branch's commits on top of the base, oldest first.
    pub commit_ids: Vec<String>,
    pub diff_source: DiffSource,
    pub diff_files: Vec<DiffFile>,
    /// The branch's session, loaded or new, with every diff file added; the
    /// caller saves it.
    pub session: ReviewSession,
}

impl BranchReview {
    /// Open the branch checked out at `checkout` against `base`, or the
    /// base a bare `tuicr -b` takes there ([`default_branch_base_in`], with
    /// tuicr's configured `branch_base`).
    pub fn open(
        checkout: &Path,
        base: Option<&str>,
        highlighter: &SyntaxHighlighter,
    ) -> Result<Self> {
        // tuicr's own settings: its git backend and whitespace mode.
        let config = crate::config::load_config().ok().and_then(|o| o.config);
        let preference =
            GitBackendPreference::from_config(config.as_ref().and_then(|c| c.backend.as_deref()));
        let whitespace = config
            .as_ref()
            .map_or(DiffWhitespaceMode::Normal, |c| c.diff_whitespace_mode());
        let vcs = GitBackend::discover_from(checkout, preference, whitespace)?;
        let base = match base {
            Some(base) => base.to_string(),
            None => default_branch_base_in(
                checkout,
                config.as_ref().and_then(|c| c.branch_base.as_deref()),
            ),
        };
        Self::open_with(Box::new(vcs), &base, highlighter)
    }

    /// [`BranchReview::open`] on a backend already open, against `base`.
    pub fn open_with(
        vcs: Box<dyn VcsBackend>,
        base: &str,
        highlighter: &SyntaxHighlighter,
    ) -> Result<Self> {
        require_branch_checkout(vcs.info())?;
        let commit_ids = branch_commit_ids(vcs.as_ref(), base)?;
        let session = load_or_create_branch_session(vcs.info(), base, &commit_ids);
        let mut review = BranchReview {
            vcs,
            base: base.to_string(),
            commit_ids: Vec::new(),
            diff_source: DiffSource::StagedAndUnstaged,
            diff_files: Vec::new(),
            session,
        };
        review.read(commit_ids, highlighter)?;
        Ok(review)
    }

    /// The checkout: its root, HEAD and branch.
    pub fn vcs_info(&self) -> &VcsInfo {
        self.vcs.info()
    }

    /// The commit pane's rows as `tuicr -b` lists them, newest first: the
    /// staged and unstaged changes when there are some, then the branch's
    /// commits.
    pub fn commit_rows(&self, highlighter: &SyntaxHighlighter) -> Result<Vec<CommitInfo>> {
        let history: Vec<CommitInfo> = if self.commit_ids.is_empty() {
            Vec::new()
        } else {
            let mut commits = self.vcs.get_commits_info(&self.commit_ids)?;
            commits.reverse();
            commits
        };
        // As tuicr -b asks: `.tuicrignore` heeded.
        let status = App::get_change_status_with_ignore(
            self.vcs.as_ref(),
            &self.vcs.info().root_path,
            highlighter,
            None,
        )?;
        Ok(commit_pane_rows(&[], history, Some(status)))
    }

    /// The diff of the rows `start..=end` of [`BranchReview::commit_rows`],
    /// read as `tuicr -b` reads a selection in its commit pane, and the
    /// source it read.
    pub fn read_selection(
        &self,
        rows: &[CommitInfo],
        start: usize,
        end: usize,
        highlighter: &SyntaxHighlighter,
    ) -> Result<(DiffSource, Vec<DiffFile>)> {
        let source = source_for_commit_subrange(rows, start, end);
        let files = branch_diff_files(
            self.vcs.as_ref(),
            &self.vcs.info().root_path,
            &source,
            highlighter,
            None,
        )?;
        Ok((source, files))
    }

    /// Read the branch again, after its files changed or commits came:
    /// the range, the diff, the session's files and commit range, comments
    /// on commits gone untagged. The session keeps its comments.
    pub fn reload(&mut self, highlighter: &SyntaxHighlighter) -> Result<()> {
        let commit_ids = branch_commit_ids(self.vcs.as_ref(), &self.base)?;
        self.session.commit_range = (!commit_ids.is_empty()).then(|| commit_ids.clone());
        untag_rewritten_commit_comments(&mut self.session, &commit_ids);
        self.read(commit_ids, highlighter)
    }

    fn read(&mut self, commit_ids: Vec<String>, highlighter: &SyntaxHighlighter) -> Result<()> {
        let diff_source = branch_diff_source(commit_ids.clone());
        let diff_files = branch_diff_files(
            self.vcs.as_ref(),
            &self.vcs.info().root_path,
            &diff_source,
            highlighter,
            None,
        )?;
        for file in &diff_files {
            self.session.add_diff_file(file);
        }
        self.commit_ids = commit_ids;
        self.diff_source = diff_source;
        self.diff_files = diff_files;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake git answering only the commands in `answers`.
    fn git(answers: &[(&str, &str)]) -> impl Fn(&[&str]) -> Option<String> {
        let answers: Vec<(String, String)> = answers
            .iter()
            .map(|(cmd, out)| (cmd.to_string(), out.to_string()))
            .collect();
        move |args: &[&str]| {
            let cmd = args.join(" ");
            answers
                .iter()
                .find(|(c, _)| *c == cmd)
                .map(|(_, out)| out.clone())
        }
    }

    const REPO_BASE: &str = "config --get tuicr.branchBase";
    const ORIGIN_HEAD: &str = "symbolic-ref --quiet --short refs/remotes/origin/HEAD";
    const LOCAL_MAIN: &str = "rev-parse --verify --quiet refs/heads/main";

    #[test]
    fn should_prefer_the_repos_git_config_over_the_tuicr_config() {
        let git = git(&[(REPO_BASE, "develop"), (ORIGIN_HEAD, "origin/main")]);

        assert_eq!(branch_base_from(Some("trunk"), git), "develop");
    }

    #[test]
    fn should_take_the_tuicr_config_before_guessing() {
        let git = git(&[(ORIGIN_HEAD, "origin/main")]);

        assert_eq!(branch_base_from(Some("trunk"), git), "trunk");
    }

    #[test]
    fn should_follow_origin_head_without_any_config() {
        let git = git(&[(ORIGIN_HEAD, "origin/develop"), (LOCAL_MAIN, "abc")]);

        assert_eq!(branch_base_from(None, git), "develop");
    }

    #[test]
    fn should_fall_back_to_main_then_master() {
        assert_eq!(branch_base_from(None, git(&[(LOCAL_MAIN, "abc")])), "main");
        assert_eq!(branch_base_from(None, git(&[])), "master");
    }

    #[test]
    fn should_ignore_an_empty_base() {
        let git = git(&[(REPO_BASE, ""), (LOCAL_MAIN, "abc")]);

        assert_eq!(branch_base_from(Some(""), git), "main");
    }

    /// Run git in `dir`, unsigned, as a test user.
    fn run(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c"])
            .arg("commit.gpgsign=false")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn paths(review: &BranchReview) -> Vec<String> {
        let mut paths: Vec<String> = review
            .diff_files
            .iter()
            .map(|f| f.display_path().display().to_string())
            .collect();
        paths.sort();
        paths
    }

    fn open(dir: &Path) -> BranchReview {
        let vcs = GitBackend::discover_from(
            dir,
            GitBackendPreference::Libgit2,
            DiffWhitespaceMode::Normal,
        )
        .unwrap();
        BranchReview::open_with(Box::new(vcs), "main", &SyntaxHighlighter::default()).unwrap()
    }

    #[test]
    fn a_branch_review_reads_the_merge_base_to_the_working_tree_in_tuicrs_session() {
        let reviews = tempfile::tempdir().unwrap();
        crate::persistence::storage::set_test_reviews_dir(Some(reviews.path().to_path_buf()));
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path();
        run(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(dir, &["add", "."]);
        run(dir, &["commit", "-q", "-m", "base"]);
        run(dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        run(dir, &["add", "."]);
        run(dir, &["commit", "-q", "-m", "on the branch"]);
        // An uncommitted edit on top.
        std::fs::write(dir.join("a.txt"), "one\nmore\n").unwrap();

        // The branch's commit and the uncommitted edit, in the branch session.
        let mut review = open(dir);
        assert_eq!(review.commit_ids.len(), 1);
        assert!(matches!(
            review.diff_source,
            DiffSource::StagedUnstagedAndCommits(_)
        ));
        assert_eq!(paths(&review), ["a.txt", "b.txt"]);
        assert_eq!(review.session.diff_source, SessionDiffSource::Branch);
        assert_eq!(review.session.branch_base.as_deref(), Some("main"));
        assert_eq!(review.vcs_info().branch_name.as_deref(), Some("feature"));

        // A comment saved there is what the next opening, tuicr -b's
        // included, finds: the session is keyed by the branch.
        review
            .session
            .review_comments
            .push(crate::model::Comment::new(
                "looks fine".to_string(),
                crate::model::CommentType::from_id("note"),
                None,
            ));
        let saved = crate::persistence::save_session(&review.session).unwrap();
        run(dir, &["commit", "-q", "-am", "amend later"]);
        let again = open(dir);
        assert_eq!(again.session.review_comments.len(), 1);
        assert_eq!(crate::connect::session_path(&again.session).unwrap(), saved);

        // Files the agent writes show on the next read.
        std::fs::write(dir.join("c.txt"), "three\n").unwrap();
        run(dir, &["add", "c.txt"]);
        review.reload(&SyntaxHighlighter::default()).unwrap();
        assert_eq!(review.commit_ids.len(), 2);
        assert_eq!(paths(&review), ["a.txt", "b.txt", "c.txt"]);
        assert!(review.session.files.contains_key(Path::new("c.txt")));

        crate::persistence::storage::set_test_reviews_dir(None);
    }

    #[test]
    fn a_branch_with_nothing_on_it_reads_empty() {
        let reviews = tempfile::tempdir().unwrap();
        crate::persistence::storage::set_test_reviews_dir(Some(reviews.path().to_path_buf()));
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path();
        run(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(dir, &["add", "."]);
        run(dir, &["commit", "-q", "-m", "base"]);
        run(dir, &["checkout", "-q", "-b", "feature"]);

        let review = open(dir);
        assert!(review.commit_ids.is_empty());
        assert_eq!(review.diff_source, DiffSource::StagedAndUnstaged);
        assert!(review.diff_files.is_empty());
        crate::persistence::storage::set_test_reviews_dir(None);
    }

    fn file_paths(files: &[DiffFile]) -> Vec<String> {
        let mut paths: Vec<String> = files
            .iter()
            .map(|f| f.display_path().display().to_string())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn the_commit_pane_lists_staged_and_unstaged_changes_and_reads_a_selection() {
        let reviews = tempfile::tempdir().unwrap();
        crate::persistence::storage::set_test_reviews_dir(Some(reviews.path().to_path_buf()));
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path();
        run(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("s.txt"), "one\n").unwrap();
        run(dir, &["add", "."]);
        run(dir, &["commit", "-q", "-m", "base"]);
        run(dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        run(dir, &["add", "."]);
        run(dir, &["commit", "-q", "-m", "on the branch"]);
        // One edit staged, one not.
        std::fs::write(dir.join("s.txt"), "one\nstaged\n").unwrap();
        run(dir, &["add", "s.txt"]);
        std::fs::write(dir.join("a.txt"), "one\nmore\n").unwrap();

        let review = open(dir);
        let hl = SyntaxHighlighter::default();
        let rows = review.commit_rows(&hl).unwrap();
        let ids: Vec<&str> = rows.iter().map(|c| c.short_id.as_str()).collect();
        assert_eq!(&ids[..2], ["STAGED", "UNSTAGED"]);
        assert_eq!(rows.len(), 3);
        assert!(!is_special(&rows[2]));
        let read = |start, end| {
            let (source, files) = review.read_selection(&rows, start, end, &hl).unwrap();
            (source, file_paths(&files))
        };
        // Each special row alone, the commit alone, and everything.
        assert_eq!(read(0, 0), (DiffSource::Staged, vec!["s.txt".to_string()]));
        assert_eq!(
            read(1, 1),
            (DiffSource::Unstaged, vec!["a.txt".to_string()])
        );
        let (source, files) = read(2, 2);
        assert!(matches!(source, DiffSource::CommitRange(_)));
        assert_eq!(files, ["b.txt"]);
        let (source, files) = read(0, 2);
        assert!(matches!(source, DiffSource::StagedUnstagedAndCommits(_)));
        assert_eq!(files, ["a.txt", "b.txt", "s.txt"]);
        crate::persistence::storage::set_test_reviews_dir(None);
    }
}
