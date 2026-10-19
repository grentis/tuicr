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
