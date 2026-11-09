//! Comments under a commit selection: which commits a selection holds,
//! which comments it shows, and the commit a new comment is made against.
//!
//! UI-free: tuicr's commit selector uses these, and so can other programs
//! that use tuicr as a library. `commits` is the list the selection's
//! indexes point into, in any order; `special` tells the rows that are no
//! commit (tuicr's staged and unstaged rows).

use std::collections::HashSet;

use crate::model::Comment;
use crate::vcs::CommitInfo;

/// The commit ids a selection holds. `None` when there is no selection,
/// in which case every comment shows. A selection of every commit holds
/// them all, so every comment shows too.
pub fn selected_commit_set(
    range: Option<(usize, usize)>,
    commits: &[CommitInfo],
    special: impl Fn(&CommitInfo) -> bool,
) -> Option<HashSet<String>> {
    let (start, end) = range?;
    if start > end || commits.is_empty() {
        return Some(HashSet::new());
    }
    let end = end.min(commits.len().saturating_sub(1));
    Some(
        (start..=end)
            .filter_map(|i| commits.get(i))
            .filter(|c| !special(c))
            .map(|c| c.id.clone())
            .collect(),
    )
}

/// Whether a comment shows under a selection's commit set: one made against
/// no single commit (`commit_id == None`) always does; one made against a
/// commit, only while that commit is selected. `set == None`, no selection,
/// shows every comment.
pub fn comment_visible_with(comment: &Comment, commit_set: Option<&HashSet<String>>) -> bool {
    match (&comment.commit_id, commit_set) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(sha), Some(set)) => set.contains(sha),
    }
}

/// The commit a new comment is made against: the selected one, when the
/// selection is exactly one commit. `None` otherwise (every commit, several,
/// or no selection), so the comment shows under every selection.
pub fn commit_id_for_new_comment(
    range: Option<(usize, usize)>,
    commits: &[CommitInfo],
    special: impl Fn(&CommitInfo) -> bool,
) -> Option<String> {
    let (start, end) = range?;
    if start != end {
        return None;
    }
    commits
        .get(start)
        .filter(|c| !special(c))
        .map(|c| c.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str) -> CommitInfo {
        CommitInfo {
            id: id.into(),
            short_id: id.into(),
            branch_name: None,
            summary: String::new(),
            body: None,
            author: String::new(),
            time: chrono::Utc::now(),
        }
    }

    #[test]
    fn should_show_comments_of_the_selected_commits_only() {
        let commits = [commit("a"), commit("b"), commit("c")];
        let set = selected_commit_set(Some((1, 2)), &commits, |_| false);
        let on = |id: Option<&str>| {
            let mut c = Comment::new(String::new(), crate::model::CommentType::None, None);
            c.commit_id = id.map(str::to_string);
            comment_visible_with(&c, set.as_ref())
        };
        assert!(on(None));
        assert!(on(Some("b")));
        assert!(!on(Some("a")));
        assert!(selected_commit_set(None, &commits, |_| false).is_none());
    }

    #[test]
    fn should_stamp_a_new_comment_only_on_a_single_commit() {
        let commits = [commit("a"), commit("b")];
        assert_eq!(
            commit_id_for_new_comment(Some((1, 1)), &commits, |_| false).as_deref(),
            Some("b")
        );
        assert_eq!(
            commit_id_for_new_comment(Some((0, 1)), &commits, |_| false),
            None
        );
        assert_eq!(
            commit_id_for_new_comment(Some((1, 1)), &commits, |c| c.id == "b"),
            None
        );
    }
}
