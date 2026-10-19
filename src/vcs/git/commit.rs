//! Committing the index for git actions.
//!
//! Uses the `git` CLI, not libgit2, so the user's signing (`commit.gpgsign`) and hooks apply.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{Result, TuicrError};

/// Commit what is staged in `repo_root` and return the new id. Errors carry git's output, where a
/// hook or signing failure explains itself.
pub fn commit_staged(repo_root: &Path, message: &str) -> Result<String> {
    let mut child = Command::new("git")
        .current_dir(repo_root)
        .args(["commit", "--quiet", "--file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| TuicrError::VcsCommand(format!("Failed to run git: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(message.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(TuicrError::VcsCommand(if stderr.is_empty() {
            stdout
        } else {
            stderr
        }));
    }

    let head = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "HEAD"])
        .output()?;
    if !head.status.success() {
        return Err(TuicrError::VcsCommand(
            String::from_utf8_lossy(&head.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&head.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A one-commit repository whose own config disables signing and sets a committer, isolating it
    /// from the user's global setup.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "user.email", "test@example.com"]);
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-q", "-m", "base"]);
        dir
    }

    #[test]
    fn should_commit_the_index_with_the_message() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);

        let id = commit_staged(dir.path(), "Title\n\nBody line").unwrap();

        assert_eq!(id, git(dir.path(), &["rev-parse", "HEAD"]));
        assert_eq!(
            git(dir.path(), &["log", "-1", "--format=%B"]),
            "Title\n\nBody line"
        );
    }

    #[test]
    fn should_report_why_git_refused_to_commit() {
        let dir = repo();

        let error = commit_staged(dir.path(), "Nothing").unwrap_err();

        assert!(error.to_string().contains("nothing"), "{error}");
    }
}
