use git2::Repository;
use std::path::Path;

use crate::error::{Result, TuicrError};
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

/// Throw away every uncommitted change to `path`. A file in HEAD is restored in the index and
/// working tree (`git checkout HEAD -- <path>`); one HEAD lacks is unstaged and deleted.
///
/// Uses the git CLI so checkout filters and a partial clone's missing blobs work as in git.
pub fn discard_file(workdir: &Path, path: &Path) -> Result<()> {
    let git = |args: &[&str]| -> Result<String> {
        let output = std::process::Command::new("git")
            .current_dir(workdir)
            .args(args)
            .arg("--")
            .arg(path)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| TuicrError::VcsCommand(format!("Failed to run git: {e}")))?;
        if !output.status.success() {
            return Err(TuicrError::VcsCommand(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    // Trees only, so nothing is fetched just to ask.
    let in_head = git(&["ls-tree", "--name-only", "HEAD"])
        .map(|listed| !listed.trim().is_empty())
        .unwrap_or(false);
    if in_head {
        git(&["checkout", "-q", "HEAD"])?;
        return Ok(());
    }
    git(&["rm", "-q", "--cached", "--ignore-unmatch"])?;
    match std::fs::remove_file(workdir.join(path)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(TuicrError::VcsCommand(format!(
            "Failed to delete {}: {e}",
            path.display()
        ))),
        _ => Ok(()),
    }
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
    fn unstage_file_keeps_what_was_staged_outside_an_open_handle() {
        // given: a handle that has read the index, then two files staged by another process
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");
        create_initial_commit(&repo, "a.txt", "one\n");
        fs::write(temp_dir.path().join("a.txt"), "two\n").unwrap();
        fs::write(temp_dir.path().join("b.txt"), "new\n").unwrap();
        repo.index().unwrap();
        let status = std::process::Command::new("git")
            .current_dir(temp_dir.path())
            .args(["add", "a.txt", "b.txt"])
            .status()
            .expect("git add");
        assert!(status.success());

        // when: one of them is unstaged through the handle
        unstage_file(&repo, Path::new("b.txt")).unwrap();

        // then: the other stays staged
        let reopened = Repository::open(temp_dir.path()).expect("failed to reopen repo");
        let head_tree = reopened.head().unwrap().peel_to_tree().unwrap();
        let index = reopened.index().unwrap();
        let staged: Vec<_> = reopened
            .diff_tree_to_index(Some(&head_tree), Some(&index), None)
            .unwrap()
            .deltas()
            .filter_map(|delta| delta.new_file().path().map(Path::to_path_buf))
            .collect();
        assert_eq!(staged, [Path::new("a.txt")]);
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

    #[test]
    fn unstage_file_puts_the_head_version_back_in_the_index() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");
        create_initial_commit(&repo, "test.txt", "hello\n");
        fs::write(temp_dir.path().join("test.txt"), "changed\n").unwrap();
        stage_file(&repo, Path::new("test.txt")).unwrap();

        unstage_file(&repo, Path::new("test.txt")).unwrap();

        let statuses = repo.statuses(None).unwrap();
        let status = statuses.get(0).unwrap().status();
        assert!(status.contains(git2::Status::WT_MODIFIED));
        assert!(!status.contains(git2::Status::INDEX_MODIFIED));
    }

    #[test]
    fn unstage_file_drops_a_new_file_from_the_index() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");
        create_initial_commit(&repo, "test.txt", "hello\n");
        fs::write(temp_dir.path().join("new.txt"), "new\n").unwrap();
        stage_file(&repo, Path::new("new.txt")).unwrap();

        unstage_file(&repo, Path::new("new.txt")).unwrap();

        let index = repo.index().unwrap();
        assert!(index.get_path(Path::new("new.txt"), 0).is_none());
        assert!(temp_dir.path().join("new.txt").exists());
    }

    #[test]
    fn unstage_file_works_before_the_first_commit() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");
        fs::write(temp_dir.path().join("test.txt"), "hello\n").unwrap();
        stage_file(&repo, Path::new("test.txt")).unwrap();

        unstage_file(&repo, Path::new("test.txt")).unwrap();

        let index = repo.index().unwrap();
        assert!(index.get_path(Path::new("test.txt"), 0).is_none());
    }

    #[test]
    fn path_change_status_tells_staged_from_unstaged() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");
        create_initial_commit(&repo, "test.txt", "hello\n");
        let status = |path: &str| path_change_status(&repo, Path::new(path)).unwrap();

        assert_eq!(
            (status("test.txt").staged, status("test.txt").unstaged),
            (false, false)
        );

        fs::write(temp_dir.path().join("test.txt"), "changed\n").unwrap();
        assert_eq!(
            (status("test.txt").staged, status("test.txt").unstaged),
            (false, true)
        );

        stage_file(&repo, Path::new("test.txt")).unwrap();
        assert_eq!(
            (status("test.txt").staged, status("test.txt").unstaged),
            (true, false)
        );

        fs::write(temp_dir.path().join("test.txt"), "again\n").unwrap();
        assert_eq!(
            (status("test.txt").staged, status("test.txt").unstaged),
            (true, true)
        );

        fs::write(temp_dir.path().join("new.txt"), "new\n").unwrap();
        assert_eq!(
            (status("new.txt").staged, status("new.txt").unstaged),
            (false, true)
        );
        assert_eq!(
            (status("gone.txt").staged, status("gone.txt").unstaged),
            (false, false)
        );
    }

    /// Whether `path` has any change, staged or not, after a re-read.
    fn changed(root: &Path, path: &str) -> bool {
        let repo = Repository::open(root).unwrap();
        let status = path_change_status(&repo, Path::new(path)).unwrap();
        status.staged || status.unstaged
    }

    #[test]
    fn discard_file_restores_staged_and_unstaged_edits_from_head() {
        // given a committed file edited, part staged and part not
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        create_initial_commit(&repo, "a.txt", "committed\n");
        fs::write(dir.path().join("a.txt"), "staged\n").unwrap();
        stage_file(&repo, Path::new("a.txt")).unwrap();
        fs::write(dir.path().join("a.txt"), "unstaged\n").unwrap();
        // when
        discard_file(dir.path(), Path::new("a.txt")).unwrap();
        // then
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "committed\n"
        );
        assert!(!changed(dir.path(), "a.txt"));
    }

    #[test]
    fn discard_file_deletes_new_files_staged_or_untracked() {
        // given a staged new file and an untracked one
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        create_initial_commit(&repo, "a.txt", "committed\n");
        fs::write(dir.path().join("staged.txt"), "new\n").unwrap();
        stage_file(&repo, Path::new("staged.txt")).unwrap();
        fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();
        // when
        discard_file(dir.path(), Path::new("staged.txt")).unwrap();
        discard_file(dir.path(), Path::new("untracked.txt")).unwrap();
        // then both are gone, from disk and from the index
        assert!(!dir.path().join("staged.txt").exists());
        assert!(!dir.path().join("untracked.txt").exists());
        assert!(!changed(dir.path(), "staged.txt"));
        assert!(
            Repository::open(dir.path())
                .unwrap()
                .index()
                .unwrap()
                .get_path(Path::new("staged.txt"), 0)
                .is_none()
        );
    }

    #[test]
    fn discard_file_brings_back_a_deleted_file() {
        // given a committed file deleted, and the deletion staged
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        create_initial_commit(&repo, "gone.txt", "committed\n");
        fs::remove_file(dir.path().join("gone.txt")).unwrap();
        stage_file(&repo, Path::new("gone.txt")).unwrap();
        // when
        discard_file(dir.path(), Path::new("gone.txt")).unwrap();
        // then
        assert_eq!(
            fs::read_to_string(dir.path().join("gone.txt")).unwrap(),
            "committed\n"
        );
        assert!(!changed(dir.path(), "gone.txt"));
    }
}
