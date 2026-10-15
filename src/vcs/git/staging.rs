use git2::Repository;
use std::path::Path;

use crate::error::Result;
use crate::vcs::git::diff::fresh_index;
use crate::vcs::traits::VcsChangeStatus;

pub fn stage_file(repo: &Repository, path: &Path) -> Result<()> {
    // A stale index written back would undo what was staged meanwhile.
    let mut index = fresh_index(repo)?;
    if repo
        .workdir()
        .is_some_and(|workdir| workdir.join(path).exists())
    {
        index.add_path(path)?;
    } else {
        index.remove_path(path)?;
    }
    index.write()?;
    Ok(())
}

/// Staged and unstaged changes of one path (untracked counts as unstaged).
pub fn path_change_status(repo: &Repository, path: &Path) -> Result<VcsChangeStatus> {
    let status = match repo.status_file(path) {
        Ok(status) => status,
        // Neither in HEAD, the index nor the working tree: nothing to commit.
        Err(e) if e.code() == git2::ErrorCode::NotFound => git2::Status::CURRENT,
        Err(e) => return Err(e.into()),
    };
    let staged = status.intersects(
        git2::Status::INDEX_NEW
            | git2::Status::INDEX_MODIFIED
            | git2::Status::INDEX_DELETED
            | git2::Status::INDEX_RENAMED
            | git2::Status::INDEX_TYPECHANGE,
    );
    let unstaged = status.intersects(
        git2::Status::WT_NEW
            | git2::Status::WT_MODIFIED
            | git2::Status::WT_DELETED
            | git2::Status::WT_RENAMED
            | git2::Status::WT_TYPECHANGE,
    );
    Ok(VcsChangeStatus { staged, unstaged })
}

/// `git reset -- <path>`: reset the index entry to HEAD's, or drop it when HEAD lacks the file (or
/// there is no HEAD).
pub fn unstage_file(repo: &Repository, path: &Path) -> Result<()> {
    // The reset writes the index back: re-read it first.
    let mut index = fresh_index(repo)?;
    match repo.head().ok().and_then(|head| head.peel_to_commit().ok()) {
        Some(head) => repo.reset_default(Some(head.as_object()), [path])?,
        None => {
            index.remove_path(path)?;
            index.write()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_initial_commit(repo: &Repository, file_name: &str, content: &str) {
        fs::write(repo.workdir().unwrap().join(file_name), content)
            .expect("failed to write initial file");

        let mut index = repo.index().expect("failed to open index");
        index
            .add_path(Path::new(file_name))
            .expect("failed to add file to index");
        index.write().expect("failed to write index");

        let tree_id = index.write_tree().expect("failed to write tree");
        let tree = repo.find_tree(tree_id).expect("failed to find tree");
        let sig = git2::Signature::now("Test User", "test@example.com")
            .expect("failed to create signature");

        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .expect("failed to create commit");
    }

    #[test]
    fn stage_file_keeps_what_was_staged_outside_an_open_handle() {
        // given: an open handle that has read the index, then a file staged
        // by another process
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");
        create_initial_commit(&repo, "a.txt", "one\n");
        fs::write(temp_dir.path().join("a.txt"), "two\n").unwrap();
        fs::write(temp_dir.path().join("b.txt"), "new\n").unwrap();
        repo.index().unwrap();
        let status = std::process::Command::new("git")
            .current_dir(temp_dir.path())
            .args(["add", "a.txt"])
            .status()
            .expect("git add");
        assert!(status.success());

        // when: another file is staged through the handle
        stage_file(&repo, Path::new("b.txt")).unwrap();

        // then: both are staged
        let reopened = Repository::open(temp_dir.path()).expect("failed to reopen repo");
        let head_tree = reopened.head().unwrap().peel_to_tree().unwrap();
        let index = reopened.index().unwrap();
        let staged: Vec<_> = reopened
            .diff_tree_to_index(Some(&head_tree), Some(&index), None)
            .unwrap()
            .deltas()
            .filter_map(|delta| delta.new_file().path().map(Path::to_path_buf))
            .collect();
        assert_eq!(staged, [Path::new("a.txt"), Path::new("b.txt")]);
    }

    #[test]
    fn stage_file_adds_to_index() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");

        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "hello\n").unwrap();

        stage_file(&repo, Path::new("test.txt")).unwrap();

        let index = repo.index().unwrap();
        assert!(index.get_path(Path::new("test.txt"), 0).is_some());
    }

    #[test]
    fn stage_file_removes_deleted_file_from_index() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");
        create_initial_commit(&repo, "deleted.txt", "goodbye\n");

        fs::remove_file(temp_dir.path().join("deleted.txt")).expect("failed to delete file");

        stage_file(&repo, Path::new("deleted.txt")).unwrap();

        let reopened_repo = Repository::open(temp_dir.path()).expect("failed to reopen repo");
        let index = reopened_repo.index().expect("failed to reopen index");
        assert!(index.get_path(Path::new("deleted.txt"), 0).is_none());

        let head_tree = reopened_repo
            .head()
            .expect("failed to find HEAD")
            .peel_to_tree()
            .expect("failed to find HEAD tree");
        let diff = reopened_repo
            .diff_tree_to_index(Some(&head_tree), Some(&index), None)
            .expect("failed to diff HEAD against index");
        let delta = diff.deltas().next().expect("expected staged deletion");
        assert_eq!(delta.status(), git2::Delta::Deleted);
    }
}
