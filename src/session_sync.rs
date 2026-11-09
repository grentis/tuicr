//! A session open in memory, kept in step with its file while other
//! processes write to it: the agent through `tuicr review add` and `done`,
//! or another tuicr.
//!
//! It is a three-way merge. The base is the session as last read from or
//! written to the file (the snapshot); what changed since in memory and what
//! changed since on disk are both kept, the local side winning a conflict:
//! a comment edited or deleted here stays so; one added, edited or deleted
//! only there comes in; a file's reviewed mark changed only there comes in.
//!
//! [`merge_in`] is "what changed in" (a poll of the file), [`save_out`]
//! "what changed out" (a save merging what is there meanwhile). tuicr's App
//! uses both on its own fields; [`SyncedSession`] holds them for programs
//! that use tuicr as a library.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::Utc;

use crate::error::Result;
use crate::model::{Comment, ReviewSession};
use crate::persistence::storage;

/// The session file's modification time and size, to tell whether it
/// changed since last read without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFileState {
    modified: Option<SystemTime>,
    len: u64,
}

impl SessionFileState {
    pub fn from_path(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

/// What [`merge_in`] took from the file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionMerge {
    /// Comments added by another writer.
    pub added: usize,
    /// Every merged change, including external deletions and edits, which
    /// need a redraw even though they add nothing.
    pub changed: usize,
}

/// What changed in: when the file at `path` changed since `file_state` (or
/// with `force`), merge what another process wrote into `session`, against
/// `snapshot`; then `snapshot` is the file's session and `file_state` its
/// state. A missing file clears `file_state` and merges nothing.
pub fn merge_in(
    session: &mut ReviewSession,
    snapshot: &mut ReviewSession,
    path: &Path,
    file_state: &mut Option<SessionFileState>,
    force: bool,
) -> Result<SessionMerge> {
    if !path.exists() {
        *file_state = None;
        return Ok(SessionMerge::default());
    }
    let state = SessionFileState::from_path(path)?;
    if !force && *file_state == Some(state) {
        return Ok(SessionMerge::default());
    }

    let latest = storage::load_session(path)?;
    let before = comment_count(session);
    let changed = merge_external_session_changes(session, snapshot, &latest);
    *snapshot = latest;
    *file_state = Some(state);
    Ok(SessionMerge {
        added: comment_count(session).saturating_sub(before),
        changed,
    })
}

/// What changed out: save `session` to its file, merging in what another
/// process wrote there since `snapshot`. Returns the file's path and the
/// session as saved, the new snapshot.
pub fn save_out(
    session: &ReviewSession,
    snapshot: &ReviewSession,
) -> Result<(PathBuf, ReviewSession)> {
    let (path, saved, ()) = storage::save_session_by_identity(session, |persisted| {
        let mut merged = session.clone();
        if let Some(latest) = persisted.as_ref() {
            merge_external_session_changes(&mut merged, snapshot, latest);
        }
        merged.updated_at = Utc::now();
        Ok((merged, ()))
    })?;
    Ok((path, saved))
}

/// Merge into `current` what changed from `base` to `latest`, keeping what
/// changed locally. Returns how many changes it took.
pub fn merge_external_session_changes(
    current: &mut ReviewSession,
    base: &ReviewSession,
    latest: &ReviewSession,
) -> usize {
    let mut changed = 0;

    for (path, latest_review) in &latest.files {
        if !current.files.contains_key(path) {
            current.files.insert(path.clone(), latest_review.clone());
            changed += latest_review.comment_count();
            continue;
        }

        let base_reviewed = base.files.get(path).map(|review| review.reviewed);
        if let Some(current_review) = current.files.get_mut(path)
            && Some(current_review.reviewed) == base_reviewed
            && current_review.reviewed != latest_review.reviewed
        {
            current_review.reviewed = latest_review.reviewed;
            changed += 1;
        }
    }

    let base_comments = collect_stored_comments(base);
    let current_comments = collect_stored_comments(current);
    let latest_comments = collect_stored_comments(latest);

    for (id, latest_comment) in &latest_comments {
        match base_comments.get(id) {
            None => {
                if !current_comments.contains_key(id) {
                    upsert_stored_comment(current, latest_comment.clone());
                    changed += 1;
                }
            }
            Some(base_comment) if latest_comment != base_comment => {
                match current_comments.get(id) {
                    Some(current_comment) if current_comment == base_comment => {
                        upsert_stored_comment(current, latest_comment.clone());
                        changed += 1;
                    }
                    None => {
                        // Local deletion wins over an external edit.
                    }
                    Some(_) => {
                        // Local edit wins over an external edit of the same comment.
                    }
                }
            }
            Some(_) => {}
        }
    }

    for (id, base_comment) in &base_comments {
        if !latest_comments.contains_key(id)
            && current_comments
                .get(id)
                .is_some_and(|current_comment| current_comment == base_comment)
            && remove_stored_comment(current, id)
        {
            changed += 1;
        }
    }

    changed
}

/// A session kept in step with its file: [`SyncedSession::merge_in`] for
/// another process's writes, [`SyncedSession::save`] for this one's.
pub struct SyncedSession {
    pub session: ReviewSession,
    path: PathBuf,
    snapshot: ReviewSession,
    file_state: Option<SessionFileState>,
}

impl SyncedSession {
    /// Follow `session`'s file in the reviews directory. The snapshot is the
    /// file's session when it exists, else `session` itself.
    pub fn new(session: ReviewSession) -> Result<Self> {
        let path = storage::session_path(&session)?;
        Ok(Self::at(session, path))
    }

    /// [`SyncedSession::new`] with the file at `path`.
    pub fn at(session: ReviewSession, path: PathBuf) -> Self {
        let file_state = SessionFileState::from_path(&path).ok();
        let snapshot = file_state
            .and_then(|_| storage::load_session(&path).ok())
            .unwrap_or_else(|| session.clone());
        Self {
            session,
            path,
            snapshot,
            file_state,
        }
    }

    /// The session file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Merge what another process wrote since the last look; with `force`,
    /// even when the file looks unchanged.
    pub fn merge_in(&mut self, force: bool) -> Result<SessionMerge> {
        merge_in(
            &mut self.session,
            &mut self.snapshot,
            &self.path,
            &mut self.file_state,
            force,
        )
    }

    /// Save, merging what another process wrote meanwhile; the session is
    /// then the one saved. Returns the file's path.
    pub fn save(&mut self) -> Result<PathBuf> {
        let (path, saved) = save_out(&self.session, &self.snapshot)?;
        self.session = saved.clone();
        self.snapshot = saved;
        self.file_state = SessionFileState::from_path(&path).ok();
        self.path = path.clone();
        Ok(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredComment {
    location: StoredCommentLocation,
    comment: Comment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StoredCommentLocation {
    Review,
    File { path: PathBuf },
    Line { path: PathBuf, line: u32 },
}

fn comment_count(session: &ReviewSession) -> usize {
    session.review_comments.len()
        + session
            .files
            .values()
            .map(|review| review.comment_count())
            .sum::<usize>()
}

fn collect_stored_comments(session: &ReviewSession) -> HashMap<String, StoredComment> {
    let mut comments = HashMap::new();
    for comment in &session.review_comments {
        comments.insert(
            comment.id.clone(),
            StoredComment {
                location: StoredCommentLocation::Review,
                comment: comment.clone(),
            },
        );
    }

    for (path, review) in &session.files {
        for comment in &review.file_comments {
            comments.insert(
                comment.id.clone(),
                StoredComment {
                    location: StoredCommentLocation::File { path: path.clone() },
                    comment: comment.clone(),
                },
            );
        }

        for (line, line_comments) in &review.line_comments {
            for comment in line_comments {
                comments.insert(
                    comment.id.clone(),
                    StoredComment {
                        location: StoredCommentLocation::Line {
                            path: path.clone(),
                            line: *line,
                        },
                        comment: comment.clone(),
                    },
                );
            }
        }
    }

    comments
}

fn upsert_stored_comment(session: &mut ReviewSession, stored: StoredComment) {
    remove_stored_comment(session, &stored.comment.id);
    match stored.location {
        StoredCommentLocation::Review => {
            session.review_comments.push(stored.comment);
            session.order_threads();
        }
        StoredCommentLocation::File { path } => {
            if let Some(review) = session.files.get_mut(&path) {
                review.file_comments.push(stored.comment);
                review.order_threads();
            }
        }
        StoredCommentLocation::Line { path, line } => {
            if let Some(review) = session.files.get_mut(&path) {
                review
                    .line_comments
                    .entry(line)
                    .or_default()
                    .push(stored.comment);
                review.order_threads();
            }
        }
    }
}

fn remove_stored_comment(session: &mut ReviewSession, id: &str) -> bool {
    if let Some(index) = session
        .review_comments
        .iter()
        .position(|comment| comment.id == id)
    {
        session.review_comments.remove(index);
        return true;
    }

    for review in session.files.values_mut() {
        if let Some(index) = review
            .file_comments
            .iter()
            .position(|comment| comment.id == id)
        {
            review.file_comments.remove(index);
            return true;
        }

        let mut removed_from_line = None;
        for (line, comments) in &mut review.line_comments {
            if let Some(index) = comments.iter().position(|comment| comment.id == id) {
                comments.remove(index);
                removed_from_line = Some((*line, comments.is_empty()));
                break;
            }
        }
        if let Some((line, emptied)) = removed_from_line {
            if emptied {
                review.line_comments.remove(&line);
            }
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommentType, FileStatus, SessionDiffSource};
    use crate::review_store::{AddCommentRequest, CommentTarget, add_comment_to_session};

    /// A reviews directory of the test's own, for as long as it lives.
    struct Reviews {
        _dir: tempfile::TempDir,
    }

    impl Reviews {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            storage::set_test_reviews_dir(Some(dir.path().to_path_buf()));
            Reviews { _dir: dir }
        }
    }

    impl Drop for Reviews {
        fn drop(&mut self) {
            storage::set_test_reviews_dir(None);
        }
    }

    fn note(content: &str) -> Comment {
        Comment::new(content.to_string(), CommentType::from_id("note"), None)
    }

    /// The agent's side: read the file, change it, write it back, as
    /// `tuicr review add` does.
    fn agent(path: &Path, change: impl FnOnce(&mut ReviewSession)) {
        let mut on_disk = storage::load_session(path).unwrap();
        change(&mut on_disk);
        // A later write, so the file's state differs even on coarse clocks.
        std::thread::sleep(std::time::Duration::from_millis(20));
        storage::save_session(&on_disk).unwrap();
    }

    fn contents(session: &ReviewSession) -> Vec<String> {
        let mut all: Vec<String> = session
            .review_comments
            .iter()
            .map(|c| c.content.clone())
            .collect();
        all.sort();
        all
    }

    #[test]
    fn what_another_process_writes_comes_in_and_what_is_written_here_goes_out() {
        let _reviews = Reviews::new();
        let repo = tempfile::tempdir().unwrap();
        let mut session = ReviewSession::new(
            repo.path().to_path_buf(),
            "abc".to_string(),
            Some("feature".to_string()),
            SessionDiffSource::Branch,
        );
        session.add_file(PathBuf::from("src/main.rs"), FileStatus::Modified, 0);
        storage::save_session(&session).unwrap();
        let mut synced = SyncedSession::new(session).unwrap();
        let path = synced.path().to_path_buf();

        // Nothing changed on disk: nothing to merge.
        assert_eq!(synced.merge_in(false).unwrap(), SessionMerge::default());

        // A comment written here, one added by the agent meanwhile: both kept.
        synced.session.review_comments.push(note("mine"));
        agent(&path, |s| {
            add_comment_to_session(
                s,
                AddCommentRequest {
                    target: CommentTarget::Review,
                    content: "theirs".to_string(),
                    comment_type: CommentType::from_id("note"),
                    author: "agent".to_string(),
                    commit_id: None,
                },
            )
            .unwrap();
        });
        let merge = synced.merge_in(false).unwrap();
        assert_eq!(
            merge,
            SessionMerge {
                added: 1,
                changed: 1
            }
        );
        assert_eq!(contents(&synced.session), ["mine", "theirs"]);

        // Saved, the file holds both.
        assert_eq!(synced.save().unwrap(), path);
        assert_eq!(
            contents(&storage::load_session(&path).unwrap()),
            ["mine", "theirs"]
        );

        // The agent deletes its own and edits mine; mine edited here too:
        // its deletion comes in, the local edit wins.
        let mine = synced
            .session
            .review_comments
            .iter()
            .position(|c| c.content == "mine");
        synced.session.review_comments[mine.unwrap()].content = "mine, edited".to_string();
        agent(&path, |s| {
            s.review_comments.retain(|c| c.content != "theirs");
            for c in &mut s.review_comments {
                c.content = "mine, by the agent".to_string();
            }
        });
        let merge = synced.merge_in(false).unwrap();
        assert_eq!(merge.changed, 1);
        assert_eq!(contents(&synced.session), ["mine, edited"]);

        // Saved again over the agent's write: the local edit goes out.
        synced.save().unwrap();
        assert_eq!(
            contents(&storage::load_session(&path).unwrap()),
            ["mine, edited"]
        );
    }

    #[test]
    fn a_session_without_a_file_merges_nothing_until_saved() {
        let _reviews = Reviews::new();
        let repo = tempfile::tempdir().unwrap();
        let session = ReviewSession::new(
            repo.path().to_path_buf(),
            "abc".to_string(),
            Some("feature".to_string()),
            SessionDiffSource::Branch,
        );
        let mut synced = SyncedSession::new(session).unwrap();
        assert!(!synced.path().exists());
        assert_eq!(synced.merge_in(true).unwrap(), SessionMerge::default());
        synced.session.review_comments.push(note("first"));
        let path = synced.save().unwrap();
        assert_eq!(contents(&storage::load_session(&path).unwrap()), ["first"]);
    }
}
