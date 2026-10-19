//! Committing and rewording for git actions.
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

/// Run git in `repo_root` with optional `stdin`; return trimmed stdout, or git's error text.
fn git(
    repo_root: &Path,
    args: &[&str],
    stdin: Option<&str>,
    env: &[(&str, &str)],
) -> Result<String> {
    let mut command = Command::new("git");
    command
        .current_dir(repo_root)
        .args(args)
        .envs(env.iter().copied())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| TuicrError::VcsCommand(format!("Failed to run git: {e}")))?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes())?;
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
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A commit's message exactly as stored, everything after its headers.
fn raw_message(repo_root: &Path, id: &str) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["cat-file", "commit", id])
        .output()?;
    if !output.status.success() {
        return Err(TuicrError::VcsCommand(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .split_once("\n\n")
        .map_or_else(String::new, |(_, message)| message.to_string()))
}

/// The message of commit `id`, without its trailing newline.
pub fn commit_message(repo_root: &Path, id: &str) -> Result<String> {
    Ok(raw_message(repo_root, id)?.trim_end().to_string())
}

/// Set the message of commit `id` on the current branch; return every changed id, old to new (it
/// and each later commit).
///
/// The latest commit is amended (`git commit --amend --only`), so hooks and signing apply. An older
/// one is reworded without a rebase: each commit from it to HEAD is rewritten with the same tree,
/// author and date and the new parent (and message, for the target), then the branch moves to the
/// last. The index and working tree are untouched and no conflict is possible; `commit.gpgsign`
/// signs them, but hooks do not run.
pub fn reword_commit(repo_root: &Path, id: &str, message: &str) -> Result<Vec<(String, String)>> {
    let message = format!("{}\n", message.trim_end());
    let head = git(repo_root, &["rev-parse", "HEAD"], None, &[])?;
    let target = git(
        repo_root,
        &["rev-parse", "--verify", &format!("{id}^{{commit}}")],
        None,
        &[],
    )?;

    if target == head {
        git(
            repo_root,
            &["commit", "--amend", "--only", "--quiet", "--file", "-"],
            Some(&message),
            &[],
        )?;
        let new_head = git(repo_root, &["rev-parse", "HEAD"], None, &[])?;
        return Ok(vec![(head, new_head)]);
    }

    let chain = git(
        repo_root,
        &[
            "rev-list",
            "--reverse",
            "--parents",
            &format!("{target}^..HEAD"),
        ],
        None,
        &[],
    )
    .map_err(|_| {
        TuicrError::VcsCommand("The first commit of a repository cannot be renamed".into())
    })?;
    let chain: Vec<Vec<&str>> = chain
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    if chain.first().and_then(|commit| commit.first()) != Some(&target.as_str()) {
        return Err(TuicrError::VcsCommand(
            "That commit is not on the current branch".into(),
        ));
    }
    if chain.iter().any(|commit| commit.len() > 2) {
        return Err(TuicrError::VcsCommand(
            "Cannot rename across a merge commit".into(),
        ));
    }

    let sign = git(
        repo_root,
        &["config", "--type=bool", "commit.gpgsign"],
        None,
        &[],
    )
    .is_ok_and(|value| value == "true");

    let mut rewritten: Vec<(String, String)> = Vec::new();
    for commit in &chain {
        let (old, parent) = (commit[0], commit[1]);
        let parent = rewritten
            .iter()
            .find(|(from, _)| from == parent)
            .map_or(parent, |(_, to)| to.as_str())
            .to_string();
        let tree = git(
            repo_root,
            &["rev-parse", &format!("{old}^{{tree}}")],
            None,
            &[],
        )?;
        let author = git(
            repo_root,
            &["log", "-1", "--format=%an%x00%ae%x00%ad", "--date=raw", old],
            None,
            &[],
        )?;
        let mut author = author.split('\0');
        let (name, email, date) = (
            author.next().unwrap_or_default(),
            author.next().unwrap_or_default(),
            author.next().unwrap_or_default(),
        );
        let commit_message = if old == target {
            message.clone()
        } else {
            raw_message(repo_root, old)?
        };
        let mut args = vec!["commit-tree", tree.as_str(), "-p", parent.as_str()];
        if sign {
            args.push("-S");
        }
        args.extend(["-F", "-"]);
        let new = git(
            repo_root,
            &args,
            Some(&commit_message),
            &[
                ("GIT_AUTHOR_NAME", name),
                ("GIT_AUTHOR_EMAIL", email),
                ("GIT_AUTHOR_DATE", date),
            ],
        )?;
        rewritten.push((old.to_string(), new));
    }

    let new_head = rewritten
        .last()
        .map(|(_, new)| new.clone())
        .unwrap_or_default();
    git(
        repo_root,
        &[
            "update-ref",
            "-m",
            "tuicr: rename commit",
            "HEAD",
            &new_head,
            &head,
        ],
        None,
        &[],
    )?;
    Ok(rewritten)
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

    fn commit_file(dir: &Path, name: &str, content: &str, message: &str) -> String {
        std::fs::write(dir.join(name), content).unwrap();
        git(dir, &["add", name]);
        git(dir, &["commit", "-q", "-m", message]);
        git(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn should_rename_the_latest_commit_by_amending_its_message() {
        let dir = repo();
        let old = commit_file(dir.path(), "b.txt", "b\n", "Old title");

        let rewritten = reword_commit(dir.path(), &old, "New title\n\nWith a body").unwrap();

        let head = git(dir.path(), &["rev-parse", "HEAD"]);
        assert_eq!(rewritten, vec![(old, head)]);
        assert_eq!(
            git(dir.path(), &["log", "-1", "--format=%B"]),
            "New title\n\nWith a body"
        );
    }

    #[test]
    fn should_rename_an_older_commit_and_keep_the_later_ones_and_the_worktree() {
        let dir = repo();
        let root = dir.path();
        let first = commit_file(root, "b.txt", "b\n", "First");
        let second = commit_file(root, "c.txt", "c\n", "Second\n\nkept body");
        let trees_before = git(root, &["log", "--format=%T", "-2"]);
        // Uncommitted work, staged and unstaged, must come through untouched.
        std::fs::write(root.join("b.txt"), "staged\n").unwrap();
        git(root, &["add", "b.txt"]);
        std::fs::write(root.join("c.txt"), "unstaged\n").unwrap();

        let rewritten = reword_commit(root, &first, "First, renamed").unwrap();

        assert_eq!(rewritten.len(), 2);
        assert_eq!(rewritten[0].0, first);
        assert_eq!(rewritten[1].0, second);
        assert_eq!(rewritten[1].1, git(root, &["rev-parse", "HEAD"]));
        assert_eq!(
            git(root, &["log", "--format=%s", "-3"]),
            "Second\nFirst, renamed\nbase"
        );
        assert_eq!(
            git(root, &["log", "-1", "--format=%B"]),
            "Second\n\nkept body"
        );
        assert_eq!(git(root, &["log", "--format=%T", "-2"]), trees_before);
        assert_eq!(git(root, &["status", "--short"]), "M  b.txt\n M c.txt");
    }

    #[test]
    fn should_refuse_to_rename_the_first_commit() {
        let dir = repo();
        let first = git(dir.path(), &["rev-list", "--max-parents=0", "HEAD"]);
        commit_file(dir.path(), "b.txt", "b\n", "Later");

        let error = reword_commit(dir.path(), &first, "Renamed").unwrap_err();

        assert!(error.to_string().contains("first commit"), "{error}");
    }
}
