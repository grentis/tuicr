//! PR open path.
//!
//! Given a `ForgeBackend` and a `PullRequestTarget`, produce the materials
//! the App needs to enter PR review mode: parsed diff files, a session, and
//! a `PrSessionKey` that scopes persistence and remote context fetches.
//!
//! Key invariants enforced here:
//! - The current local checkout is never treated as the source of truth.
//!   File identity comes from forge metadata; SHAs come from PR metadata.
//! - `.tuicrignore` is applied only when the caller supplies a local
//!   checkout path. Outside a checkout, the unfiltered diff is shown.
//! - No checkout mutation. We never spawn `git checkout/fetch/reset/stash`
//!   or branch-creation commands here.

use std::path::{Path, PathBuf};

use crate::error::{Result, TuicrError};
use crate::forge::traits::{
    ForgeBackend, PrSessionKey, PullRequestCommit, PullRequestDetails, PullRequestInfo,
    PullRequestReviewMetadata, PullRequestTarget,
};
use crate::model::{DiffFile, FilePatch, ReviewSession, SessionDiffSource};
use crate::syntax::SyntaxHighlighter;
use crate::tuicrignore;
use crate::vcs::diff_parser::parse_file_patches;

/// Everything the App needs to enter PR review mode.
#[derive(Debug)]
pub struct OpenedPullRequest {
    pub details: PullRequestDetails,
    pub diff_files: Vec<DiffFile>,
    pub session: ReviewSession,
    pub key: PrSessionKey,
    /// PR commits in newest-first display order. Empty when the forge
    /// returned no commits (or the backend failed and we degraded
    /// gracefully — the cumulative diff stays usable).
    pub commits: Vec<PullRequestCommit>,
    /// Best-effort metadata for detecting commits since the viewer's last
    /// submitted review. Empty when unsupported or unavailable.
    pub review_metadata: PullRequestReviewMetadata,
    /// Extended PR metadata for the description panel.
    pub pr_info: PullRequestInfo,
    /// Why the diff could not be loaded, when the PR opened without it
    /// (`diff_files` is then empty). The rest of the PR is usable.
    pub diff_error: Option<String>,
}

/// The PR's diff as fetched: its patches, or why the forge would not serve
/// them. A failed diff does not fail the open; the PR opens without it.
pub type PrPatches = std::result::Result<Vec<FilePatch>, String>;

/// Send-safe data fetched before the main thread materializes diff hunks.
pub type PrFetchData = (
    PullRequestDetails,
    PrPatches,
    Vec<PullRequestCommit>,
    PullRequestReviewMetadata,
    PullRequestInfo,
);

/// Open a PR target through a forge backend and prepare review state.
///
/// `local_checkout` is optional: when provided, `.tuicrignore` rules at the
/// root are applied. When absent (PR opened via URL outside a checkout, or
/// for a different repo), no filtering happens.
pub fn open_pull_request(
    backend: &dyn ForgeBackend,
    target: PullRequestTarget,
    local_checkout: Option<&Path>,
    highlighter: &SyntaxHighlighter,
    local_diff_fallback: bool,
) -> Result<OpenedPullRequest> {
    let (details, patches, commits, review_metadata, pr_info) =
        fetch_pr_data(backend, target, local_diff_fallback)?;
    prepare_fetched_pr(
        details,
        patches,
        commits,
        review_metadata,
        pr_info,
        local_checkout,
        highlighter,
    )
}

/// Network-only half of the PR open path: fetch PR metadata, structured file
/// patches, and the commit list. Safe to run on a background thread
/// because it does no syntax parsing and holds nothing that isn't `Send`.
///
/// The commit list is best-effort: if the forge fails on that endpoint
/// only, we still return the diff so PR review proceeds without the
/// inline selector. The PR's metadata is required; its diff is not: when
/// only the diff fails (a PR over GitHub's 300-file limit, say), the error
/// travels in place of the patches so the PR still opens, without a diff.
/// With `local_diff_fallback`, a diff refused as too large comes from the
/// local checkout instead.
pub fn fetch_pr_data(
    backend: &dyn ForgeBackend,
    target: PullRequestTarget,
    local_diff_fallback: bool,
) -> Result<PrFetchData> {
    let mut pr_info = backend.get_pull_request_info(target)?;
    // Pin `base_sha` to the commit the displayed patch is actually taken
    // from before anything downstream reads it. Everything that resolves the
    // old side of the diff — editor snapshots, context expansion, the
    // commit-range fallback parent — derives from this field, so refining it
    // once here keeps those consistent by construction.
    if let Some(base_sha) = backend.resolve_diff_base_sha(&pr_info.details) {
        pr_info.details.base_sha = base_sha;
    }
    let details = pr_info.details.clone();
    let patches = match backend.get_pull_request_diff(&details) {
        Ok(patches) => Ok(patches),
        Err(error) if local_diff_fallback && backend.is_diff_too_large(&error) => {
            local_pr_diff(backend, &details).map_err(|why| format!("{why} · {error}"))
        }
        Err(error) => Err(error.to_string()),
    };
    let commits = backend
        .list_pull_request_commits(&details)
        .unwrap_or_default();
    let review_metadata = backend
        .list_pull_request_review_metadata(&details)
        .unwrap_or_default();
    Ok((details, patches, commits, review_metadata, pr_info))
}

/// The PR's diff from the local checkout, as the forge builds it: the
/// merge-base of base and head against head. Errs with what is missing.
fn local_pr_diff(
    backend: &dyn ForgeBackend,
    pr: &PullRequestDetails,
) -> std::result::Result<Vec<FilePatch>, String> {
    let Some(root) = backend.local_checkout_path() else {
        return Err(format!(
            "run tuicr from a checkout of {} to diff it locally",
            pr.repository.slug()
        ));
    };
    let merge_base =
        crate::forge::local_merge_base(&root, &pr.base_sha, &pr.head_sha).ok_or_else(|| {
            format!(
                "the local checkout lacks the PR's commits: fetch {} and the PR's head",
                pr.base_ref_name
            )
        })?;
    let range = format!("{merge_base}..{}", pr.head_sha);
    crate::vcs::git::raw::run_git_diff(&root, &[range.as_str()]).map_err(|e| e.to_string())
}

/// Main-thread half of the PR open for fetched data: the usual preparation
/// when the diff came, or the PR without a diff when it did not.
pub fn prepare_fetched_pr(
    details: PullRequestDetails,
    patches: PrPatches,
    commits: Vec<PullRequestCommit>,
    review_metadata: PullRequestReviewMetadata,
    pr_info: PullRequestInfo,
    local_checkout: Option<&Path>,
    highlighter: &SyntaxHighlighter,
) -> Result<OpenedPullRequest> {
    let patches = match patches {
        Ok(patches) => patches,
        Err(diff_error) => {
            let key = PrSessionKey::from_details(&details);
            let session = build_session(&details, &key, &[]);
            let mut commits = commits;
            commits.reverse();
            return Ok(OpenedPullRequest {
                details,
                diff_files: Vec::new(),
                session,
                key,
                commits,
                review_metadata,
                pr_info,
                diff_error: Some(diff_error),
            });
        }
    };
    prepare_open_pr(
        details,
        patches,
        commits,
        review_metadata,
        pr_info,
        local_checkout,
        highlighter,
    )
}

/// CPU-only half of the PR open path: apply `.tuicrignore` to the raw
/// patches, then parse the hunks and build the session. Filtering before the
/// parse keeps an ignored large file from being highlighted at all. Runs on
/// the main thread because `SyntaxHighlighter` is not trivially
/// `Send`-cloneable.
pub fn prepare_open_pr(
    details: PullRequestDetails,
    patches: Vec<FilePatch>,
    commits: Vec<PullRequestCommit>,
    review_metadata: PullRequestReviewMetadata,
    pr_info: PullRequestInfo,
    local_checkout: Option<&Path>,
    highlighter: &SyntaxHighlighter,
) -> Result<OpenedPullRequest> {
    let had_patches = !patches.is_empty();
    let patches = match local_checkout {
        Some(root) => tuicrignore::filter_file_patches(root, patches),
        None => patches,
    };

    let diff_files = if had_patches && patches.is_empty() {
        Vec::new()
    } else {
        match parse_file_patches(patches, highlighter) {
            Ok(files) => files,
            Err(TuicrError::NoChanges) => {
                return Err(TuicrError::Forge(format!(
                    "Pull request #{} has no file changes",
                    details.number
                )));
            }
            Err(e) => return Err(e),
        }
    };

    let key = PrSessionKey::from_details(&details);
    let session = build_session(&details, &key, &diff_files);
    // Forge returns commits oldest-first; the inline selector renders
    // newest-first so reverse here once.
    let mut commits = commits;
    commits.reverse();

    Ok(OpenedPullRequest {
        details,
        diff_files,
        session,
        key,
        commits,
        review_metadata,
        pr_info,
        diff_error: None,
    })
}

fn build_session(
    details: &PullRequestDetails,
    key: &PrSessionKey,
    diff_files: &[DiffFile],
) -> ReviewSession {
    // The session's repo_path is purely a presentation/identity slot for PR
    // sessions. We use a virtual path so PR sessions don't collide with
    // local sessions stored under the same on-disk repo root.
    let repo_path = pr_session_repo_path(key);
    let branch_name = Some(details.head_ref_name.clone());
    let mut session = ReviewSession::new(
        repo_path,
        details.head_sha.clone(),
        branch_name,
        SessionDiffSource::PullRequest,
    );
    session.pr_session_key = Some(key.clone());
    for file in diff_files {
        session.add_diff_file(file);
    }
    session
}

/// Synthetic path used as `ReviewSession::repo_path` for PR sessions.
/// Keeps PR session filenames distinct from local sessions and conveys
/// enough identity (`forge:host/owner/repo`) for humans inspecting the
/// reviews directory.
pub fn pr_session_repo_path(key: &PrSessionKey) -> PathBuf {
    PathBuf::from(format!(
        "forge:{}/{}/{}",
        key.repository.host, key.repository.owner, key.repository.name,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::traits::{
        ForgeFileLinesRequest, ForgeRepository, PagedPullRequests, PullRequestDetails,
        PullRequestListQuery,
    };
    use crate::model::DiffLine;
    use chrono::Utc;
    use std::cell::RefCell;

    fn repo() -> ForgeRepository {
        ForgeRepository::github("github.com", "agavra", "tuicr")
    }

    fn details() -> PullRequestDetails {
        PullRequestDetails {
            repository: repo(),
            number: 125,
            title: "Review workflow".to_string(),
            url: "https://github.com/agavra/tuicr/pull/125".to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            author: Some("alice".to_string()),
            head_ref_name: "reviews".to_string(),
            base_ref_name: "main".to_string(),
            head_sha: "abcdef0123456789".to_string(),
            base_sha: "1234567890abcdef".to_string(),
            body: "body".to_string(),
            updated_at: Some(Utc::now()),
            closed: false,
            merged_at: None,
            diff_start_sha: None,
        }
    }

    struct StaticBackend {
        details: PullRequestDetails,
        patch: String,
        calls: RefCell<Vec<&'static str>>,
        /// Stands in for a backend whose `base_sha` is the base branch tip
        /// rather than the merge base the patch is taken from. `None` models
        /// a backend that already reports the merge base (GitLab).
        diff_base_sha: Option<String>,
        /// When set, the diff fails with this error.
        diff_error: Option<&'static str>,
        local_checkout: Option<PathBuf>,
    }

    impl StaticBackend {
        fn new(details: PullRequestDetails, patch: &str) -> Self {
            Self {
                details,
                patch: patch.to_string(),
                calls: RefCell::new(Vec::new()),
                diff_base_sha: None,
                diff_error: None,
                local_checkout: None,
            }
        }

        fn with_diff_base_sha(mut self, sha: &str) -> Self {
            self.diff_base_sha = Some(sha.to_string());
            self
        }

        fn with_diff_error(mut self, error: &'static str) -> Self {
            self.diff_error = Some(error);
            self
        }

        fn with_local_checkout(mut self, root: &Path) -> Self {
            self.local_checkout = Some(root.to_path_buf());
            self
        }
    }

    impl ForgeBackend for StaticBackend {
        fn is_diff_too_large(&self, error: &TuicrError) -> bool {
            error.to_string().contains("too_large")
        }

        fn local_checkout_path(&self) -> Option<PathBuf> {
            self.local_checkout.clone()
        }

        fn resolve_diff_base_sha(&self, _pr: &PullRequestDetails) -> Option<String> {
            self.calls.borrow_mut().push("resolve_diff_base_sha");
            self.diff_base_sha.clone()
        }

        fn list_pull_requests(&self, _query: PullRequestListQuery) -> Result<PagedPullRequests> {
            unimplemented!()
        }
        fn get_pull_request(&self, _target: PullRequestTarget) -> Result<PullRequestDetails> {
            self.calls.borrow_mut().push("get_pull_request");
            Ok(self.details.clone())
        }
        fn get_pull_request_diff(&self, _pr: &PullRequestDetails) -> Result<Vec<FilePatch>> {
            self.calls.borrow_mut().push("get_pull_request_diff");
            if let Some(error) = self.diff_error {
                return Err(crate::error::TuicrError::Forge(error.to_string()));
            }
            Ok(crate::vcs::diff_parser::git_fixture_file_patches(
                &self.patch,
            ))
        }
        fn fetch_file_lines(&self, _req: ForgeFileLinesRequest) -> Result<Vec<DiffLine>> {
            unimplemented!()
        }
        fn list_review_threads(
            &self,
            _pr: &PullRequestDetails,
        ) -> Result<Vec<crate::forge::remote_comments::RemoteReviewThread>> {
            Ok(Vec::new())
        }
        fn list_pull_request_commits(
            &self,
            _pr: &PullRequestDetails,
        ) -> Result<Vec<crate::forge::traits::PullRequestCommit>> {
            Ok(Vec::new())
        }
        fn get_pull_request_commit_range_diff(
            &self,
            _pr: &PullRequestDetails,
            _start_sha: &str,
            _end_sha: &str,
        ) -> Result<Vec<FilePatch>> {
            Ok(crate::vcs::diff_parser::git_fixture_file_patches(
                &self.patch,
            ))
        }
        fn create_review(
            &self,
            _pr: &PullRequestDetails,
            _request: crate::forge::traits::CreateReviewRequest<'_>,
        ) -> Result<crate::forge::traits::GhCreateReviewResponse> {
            unimplemented!()
        }
    }

    const SIMPLE_PATCH: &str = r##"diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 pub fn answer() -> u32 {
-    41
+    42
 }
"##;

    #[test]
    fn should_parse_pr_diff_and_build_session_keyed_by_head_sha() {
        // given
        let backend = StaticBackend::new(details(), SIMPLE_PATCH);
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when
        let opened = open_pull_request(&backend, target, None, &highlighter, false).unwrap();
        // then
        assert_eq!(opened.diff_files.len(), 1);
        assert_eq!(opened.key.head_sha, "abcdef0123456789");
        assert_eq!(opened.key.number, 125);
        assert_eq!(opened.session.diff_source, SessionDiffSource::PullRequest);
        assert_eq!(
            opened.session.pr_session_key.as_ref().map(|k| k.number),
            Some(125),
        );
        assert_eq!(
            opened.session.repo_path,
            PathBuf::from("forge:github.com/agavra/tuicr"),
        );
        // and — the forge calls were made, in order. The diff base is
        // resolved between metadata and patch so `base_sha` is already
        // pinned to the patch's old side by the time anything reads it.
        assert_eq!(
            backend.calls.borrow().as_slice(),
            &[
                "get_pull_request",
                "resolve_diff_base_sha",
                "get_pull_request_diff"
            ],
        );
    }

    /// Patch fixture covering add/modify/delete/rename in a single PR diff.
    /// Tests pair its file blocks with explicit metadata before invoking the
    /// shared hunk parser.
    const MULTI_STATUS_PATCH: &str = r##"diff --git a/added.rs b/added.rs
new file mode 100644
index 0000000..abc1234
--- /dev/null
+++ b/added.rs
@@ -0,0 +1,2 @@
+pub fn new_thing() {}
+
diff --git a/modified.rs b/modified.rs
index 1111111..2222222 100644
--- a/modified.rs
+++ b/modified.rs
@@ -1,3 +1,3 @@
 pub fn answer() -> u32 {
-    41
+    42
 }
diff --git a/deleted.rs b/deleted.rs
deleted file mode 100644
index 3333333..0000000
--- a/deleted.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-pub fn gone() {}
-
diff --git a/old_name.rs b/new_name.rs
similarity index 100%
rename from old_name.rs
rename to new_name.rs
"##;

    #[test]
    fn should_parse_multi_status_pr_patch_into_correct_diff_files() {
        // given a backend serving a patch with add/modify/delete/rename
        let backend = StaticBackend::new(details(), MULTI_STATUS_PATCH);
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when
        let opened = open_pull_request(&backend, target, None, &highlighter, false).unwrap();
        // then — all four files are recognized with correct statuses
        assert_eq!(opened.diff_files.len(), 4);
        let statuses: Vec<(String, crate::model::FileStatus)> = opened
            .diff_files
            .iter()
            .map(|f| (f.display_path().to_string_lossy().into_owned(), f.status))
            .collect();
        // Order is not guaranteed by the parser, so look up by name.
        let by_name: std::collections::HashMap<_, _> = statuses.into_iter().collect();
        assert_eq!(
            by_name.get("added.rs"),
            Some(&crate::model::FileStatus::Added)
        );
        assert_eq!(
            by_name.get("modified.rs"),
            Some(&crate::model::FileStatus::Modified)
        );
        assert_eq!(
            by_name.get("deleted.rs"),
            Some(&crate::model::FileStatus::Deleted)
        );
        assert_eq!(
            by_name.get("new_name.rs"),
            Some(&crate::model::FileStatus::Renamed)
        );
    }

    #[test]
    fn should_surface_empty_pr_as_forge_error() {
        // given a PR with no file changes (empty patch)
        let backend = StaticBackend::new(details(), "");
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when
        let err = open_pull_request(&backend, target, None, &highlighter, false).unwrap_err();
        // then
        let msg = err.to_string();
        assert!(
            msg.contains("Pull request #125 has no file changes"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn should_drop_ignored_patches_before_parsing_them() {
        // given a checkout whose .tuicrignore excludes dist/, and a PR diff
        // whose ignored patch body is malformed (a hunk header the parser
        // rejects)
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        std::fs::write(dir.path().join(".tuicrignore"), "dist/\n")
            .expect("failed to write .tuicrignore");
        let patches = vec![
            FilePatch::new(
                None,
                Some(std::path::PathBuf::from("src/main.rs")),
                crate::model::FileStatus::Modified,
                "@@ -1,3 +1,3 @@\n pub fn answer() -> u32 {\n-    41\n+    42\n }\n",
            ),
            FilePatch::new(
                None,
                Some(std::path::PathBuf::from("dist/bundle.js")),
                crate::model::FileStatus::Modified,
                "@@not-a-hunk\n+minified one-liner\n",
            ),
        ];
        let highlighter = SyntaxHighlighter::default();
        // when
        let opened = prepare_open_pr(
            details(),
            patches,
            Vec::new(),
            crate::forge::traits::PullRequestReviewMetadata::default(),
            crate::forge::traits::PullRequestInfo::from_details(details()),
            Some(dir.path()),
            &highlighter,
        )
        .expect("ignored patch must never reach the parser");
        // then only the kept file was parsed into the review
        let kept: Vec<String> = opened
            .diff_files
            .iter()
            .map(|f| f.display_path().display().to_string())
            .collect();
        assert_eq!(kept, vec!["src/main.rs"]);
    }

    #[test]
    fn should_open_an_empty_review_when_every_pr_patch_is_ignored() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        std::fs::write(dir.path().join(".tuicrignore"), "dist/\n")
            .expect("failed to write .tuicrignore");
        let highlighter = SyntaxHighlighter::default();

        let opened = prepare_open_pr(
            details(),
            vec![FilePatch::new(
                None,
                Some(std::path::PathBuf::from("dist/bundle.js")),
                crate::model::FileStatus::Modified,
                "@@ -1 +1 @@\n-old\n+new\n",
            )],
            Vec::new(),
            crate::forge::traits::PullRequestReviewMetadata::default(),
            crate::forge::traits::PullRequestInfo::from_details(details()),
            Some(dir.path()),
            &highlighter,
        )
        .expect("an ignored-only PR should open as an empty review");

        assert!(opened.diff_files.is_empty());
    }

    #[test]
    fn should_pin_base_sha_to_the_resolved_diff_base_before_downstream_reads() {
        // given — a backend reporting the base branch tip as base_sha, with
        // the real three-dot merge base available separately.
        let backend =
            StaticBackend::new(details(), SIMPLE_PATCH).with_diff_base_sha("mergebase111");
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when
        let opened = open_pull_request(&backend, target, None, &highlighter, false).unwrap();
        // then — both the standalone details and the copy carried on pr_info
        // report the merge base, so PR-mode entry, the info panel, and the
        // reload paths can't disagree about the old side.
        assert_eq!(opened.details.base_sha, "mergebase111");
        assert_eq!(opened.pr_info.details.base_sha, "mergebase111");
        // and — resolution happened before the patch was fetched, so nothing
        // downstream ever observes the unrefined tip.
        let calls = backend.calls.borrow();
        let resolved_at = calls
            .iter()
            .position(|c| *c == "resolve_diff_base_sha")
            .expect("expected a resolve_diff_base_sha call");
        let diff_at = calls
            .iter()
            .position(|c| *c == "get_pull_request_diff")
            .expect("expected a get_pull_request_diff call");
        assert!(resolved_at < diff_at);
    }

    #[test]
    fn should_keep_reported_base_sha_when_backend_needs_no_refinement() {
        // given — a backend whose base_sha is already the merge base
        // (GitLab's diff_refs.base_sha), modeled by returning None.
        let backend = StaticBackend::new(details(), SIMPLE_PATCH);
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when
        let opened = open_pull_request(&backend, target, None, &highlighter, false).unwrap();
        // then — untouched.
        assert_eq!(opened.details.base_sha, "1234567890abcdef");
        assert_eq!(opened.pr_info.details.base_sha, "1234567890abcdef");
    }

    #[test]
    fn should_open_a_pr_without_its_diff_when_only_the_diff_fails() {
        // given a forge that serves the PR but not its diff
        let backend = StaticBackend::new(details(), SIMPLE_PATCH)
            .with_diff_error("PullRequest.diff too_large");
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when
        let opened = open_pull_request(&backend, target, None, &highlighter, false).unwrap();
        // then the PR opens, empty, saying why
        assert!(opened.diff_files.is_empty());
        assert!(
            opened
                .diff_error
                .as_deref()
                .is_some_and(|error| error.contains("too_large"))
        );
        assert_eq!(opened.key.number, 125);
        assert_eq!(
            opened.details.url,
            "https://github.com/agavra/tuicr/pull/125"
        );
        assert_eq!(
            opened.session.pr_session_key.as_ref().map(|k| k.number),
            Some(125)
        );
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// A checkout where `main` adds `a.txt` and the PR branch then adds
    /// `b.txt`; returns the base and head SHAs.
    fn pr_checkout(root: &Path) -> (String, String) {
        git(root, &["init", "--quiet"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "--quiet", "-m", "base"]);
        let base = git(root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "--quiet", "-m", "head"]);
        (base, git(root, &["rev-parse", "HEAD"]))
    }

    fn too_large_pr(root: &Path, base: String, head: String) -> StaticBackend {
        let mut pr = details();
        pr.base_sha = base;
        pr.head_sha = head;
        StaticBackend::new(pr, SIMPLE_PATCH)
            .with_diff_error("PullRequest.diff too_large")
            .with_local_checkout(root)
    }

    #[test]
    fn should_diff_a_too_large_pr_from_the_local_checkout_with_local_diff_fallback() {
        // given a forge refusing the diff, and a checkout with both commits
        let dir = tempfile::tempdir().unwrap();
        let (base, head) = pr_checkout(dir.path());
        let backend = too_large_pr(dir.path(), base, head);
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when
        let opened = open_pull_request(&backend, target, None, &highlighter, true).unwrap();
        // then the diff is the PR's, built locally
        assert!(opened.diff_error.is_none());
        let paths: Vec<_> = opened
            .diff_files
            .iter()
            .map(|file| file.display_path().to_path_buf())
            .collect();
        assert_eq!(paths, vec![PathBuf::from("b.txt")]);
    }

    #[test]
    fn should_keep_the_forge_error_without_local_diff_fallback() {
        // given the same PR and checkout
        let dir = tempfile::tempdir().unwrap();
        let (base, head) = pr_checkout(dir.path());
        let backend = too_large_pr(dir.path(), base, head);
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when the setting is off
        let opened = open_pull_request(&backend, target, None, &highlighter, false).unwrap();
        // then the PR opens without its diff, as before
        assert!(opened.diff_files.is_empty());
        assert!(
            opened
                .diff_error
                .as_deref()
                .is_some_and(|error| error.contains("too_large"))
        );
    }

    #[test]
    fn should_say_what_to_fetch_when_the_checkout_lacks_the_pr_commits() {
        // given a checkout without the PR's commits
        let dir = tempfile::tempdir().unwrap();
        pr_checkout(dir.path());
        let backend = too_large_pr(dir.path(), "1".repeat(40), "2".repeat(40));
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when
        let opened = open_pull_request(&backend, target, None, &highlighter, true).unwrap();
        // then the PR opens without its diff, saying what to fetch
        assert!(opened.diff_files.is_empty());
        let error = opened.diff_error.unwrap();
        assert!(error.contains("too_large"), "{error}");
        assert!(error.contains("fetch main and the PR's head"), "{error}");
    }

    #[test]
    fn should_say_to_run_from_a_checkout_when_there_is_none() {
        // given a forge refusing the diff, opened outside any checkout
        let backend = StaticBackend::new(details(), SIMPLE_PATCH)
            .with_diff_error("PullRequest.diff too_large");
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when
        let opened = open_pull_request(&backend, target, None, &highlighter, true).unwrap();
        // then
        let error = opened.diff_error.unwrap();
        assert!(
            error.contains("run tuicr from a checkout of agavra/tuicr"),
            "{error}"
        );
    }

    #[test]
    fn should_not_fall_back_to_the_local_checkout_on_other_diff_errors() {
        // given a diff failing for another reason
        let dir = tempfile::tempdir().unwrap();
        let (base, head) = pr_checkout(dir.path());
        let mut pr = details();
        pr.base_sha = base;
        pr.head_sha = head;
        let backend = StaticBackend::new(pr, SIMPLE_PATCH)
            .with_diff_error("HTTP 401: Bad credentials")
            .with_local_checkout(dir.path());
        let target = PullRequestTarget::with_repository(repo(), 125, "125");
        let highlighter = SyntaxHighlighter::default();
        // when
        let opened = open_pull_request(&backend, target, None, &highlighter, true).unwrap();
        // then the error stays
        assert!(opened.diff_files.is_empty());
        assert!(
            opened
                .diff_error
                .as_deref()
                .is_some_and(|error| error.contains("Bad credentials"))
        );
    }
}
