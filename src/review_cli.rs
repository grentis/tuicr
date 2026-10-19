//! Non-interactive review session commands.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::{LineSideArg, ReviewCommand};
use crate::config;
use crate::error::{Result, TuicrError};
use crate::model::comment::CommentLifecycleState;
use crate::model::{ClearScope, Comment, CommentType, LineRange, LineSide, ReviewSession};
use crate::persistence::storage;
use crate::review_store::{
    AddCommentRequest, CommentTarget, ReviewStore, SessionRef, SessionSummary,
};
use crate::slug::Slug;

pub fn run(command: ReviewCommand) -> Result<()> {
    let mut stdout = io::stdout();
    run_with_writer(command, &mut stdout)
}

fn run_with_writer(command: ReviewCommand, out: &mut impl Write) -> Result<()> {
    match command {
        ReviewCommand::List { repo, all } => list_sessions(&repo, all, out),
        ReviewCommand::Add {
            session,
            repo,
            comment_type,
            side,
            username,
            reply_to: Some(parent_id),
            content,
            ..
        } => add_reply(
            &session,
            &repo,
            &parent_id,
            AddCommentOptions {
                input: None,
                comment_type,
                file: None,
                line: None,
                end_line: None,
                side,
                username,
                content,
            },
            out,
        ),
        ReviewCommand::Add {
            session,
            input,
            repo,
            comment_type,
            file,
            line,
            end_line,
            side,
            username,
            reply_to: None,
            content,
        } => add_comment(
            &session,
            &repo,
            AddCommentOptions {
                input,
                comment_type,
                file,
                line,
                end_line,
                side,
                username,
                content,
            },
            out,
        ),
        ReviewCommand::Comments {
            session,
            repo,
            needs_reply,
        } => show_comments(&session, &repo, needs_reply.as_deref(), out),
        ReviewCommand::Delete {
            session,
            comment_id,
            repo,
        } => delete_comment(&session, &repo, &comment_id, out),
        ReviewCommand::Clearc { session, repo } => {
            clear_comments(&session, &repo, ClearScope::CommentsOnly, out)
        }
        ReviewCommand::Clear { session, repo } => {
            clear_comments(&session, &repo, ClearScope::CommentsAndReviewed, out)
        }
        ReviewCommand::Attach {
            session,
            repo,
            agent,
        } => attach_agent(&session, &repo, &agent, out),
        ReviewCommand::Wait { session, repo } => wait_for_submit(&session, &repo, out),
        ReviewCommand::Done {
            session,
            repo,
            message,
        } => report_done(&session, &repo, message.as_deref(), out),
    }
}

fn clear_comments(
    session: &str,
    repo: &Path,
    scope: ClearScope,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let (cleared, unreviewed) = store.clear_comments(&session_ref, scope)?;
    storage::delete_session_if_empty_and_inactive(session_ref.path())?;
    serde_json::to_writer_pretty(
        &mut *out,
        &serde_json::json!({ "cleared": cleared, "unreviewed": unreviewed }),
    )?;
    writeln!(out)?;
    Ok(())
}

fn delete_comment(
    session: &str,
    repo: &Path,
    comment_id: &str,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    if !store.delete_comment(&session_ref, comment_id)? {
        return Err(TuicrError::InvalidInput(format!(
            "no comment with id '{comment_id}' in session '{session}'"
        )));
    }
    storage::delete_session_if_empty_and_inactive(session_ref.path())?;
    serde_json::to_writer_pretty(&mut *out, &serde_json::json!({ "deleted": comment_id }))?;
    writeln!(out)?;
    Ok(())
}

fn list_sessions(repo: &Path, all: bool, out: &mut impl Write) -> Result<()> {
    let store = ReviewStore::new();
    let summaries = if all {
        store.list_all_sessions()?
    } else {
        store.list_sessions_for_repo(repo)?
    };
    let owners = store.active_owners()?;
    let output: Vec<_> = summaries
        .into_iter()
        .map(|summary| {
            let path = storage::normalize_path_for_comparison(summary.session_ref.path());
            let owner = owners.get(&path);
            SessionSummaryOutput {
                pid: owner.map(|owner| owner.pid),
                agent: owner.and_then(|owner| owner.agent.clone()),
                ..SessionSummaryOutput::from(summary)
            }
        })
        .collect();
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

/// `tuicr review attach`: set the agent `:submit` notifies. Refuses PR
/// sessions and sessions no running tuicr has open.
fn attach_agent(session: &str, repo: &Path, agent: &str, out: &mut impl Write) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    if store.get_review(&session_ref)?.pr_session_key.is_some() {
        return Err(TuicrError::InvalidInput(format!(
            "'{session}' is a PR session; agents attach to local reviews only"
        )));
    }
    if !store.attach_agent(&session_ref, agent)? {
        return Err(TuicrError::InvalidInput(format!(
            "no running tuicr has '{session}' open. Start it first, then attach."
        )));
    }
    let output = AttachOutput {
        session: session.to_string(),
        path: session_ref.path().display().to_string(),
        agent: agent.to_string(),
    };
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

/// How often `review wait` looks at the session's registration.
const WAIT_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// `tuicr review wait`: register as the session's waiter and return on
/// `:submit`. Errors if the tuicr closes or another agent takes over.
fn wait_for_submit(session: &str, repo: &Path, out: &mut impl Write) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    if store.get_review(&session_ref)?.pr_session_key.is_some() {
        return Err(TuicrError::InvalidInput(format!(
            "'{session}' is a PR session; agents wait on local reviews only"
        )));
    }
    let me = std::process::id();
    let mut since = None;
    let mut first = true;
    loop {
        match store.wait_on(&session_ref, me)? {
            storage::WaitState::TakenBy(other) => {
                return Err(TuicrError::InvalidInput(format!(
                    "another agent is waiting on '{session}' (process {other})"
                )));
            }
            storage::WaitState::NotOpen if first => {
                return Err(TuicrError::InvalidInput(format!(
                    "no running tuicr has '{session}' open. Start it first, then wait."
                )));
            }
            storage::WaitState::NotOpen => {
                return Err(TuicrError::InvalidInput(format!(
                    "the tuicr reviewing '{session}' was closed"
                )));
            }
            storage::WaitState::Waiting { submitted_at } => {
                // Only a `:submit` after the first look counts.
                if first {
                    since = submitted_at;
                } else if submitted_at > since {
                    let output = WaitOutput {
                        session: session.to_string(),
                        path: session_ref.path().display().to_string(),
                        submitted_at: submitted_at.map(|at| at.to_rfc3339()),
                    };
                    serde_json::to_writer_pretty(&mut *out, &output)?;
                    writeln!(out)?;
                    return Ok(());
                }
            }
        }
        first = false;
        std::thread::sleep(WAIT_POLL);
    }
}

/// `tuicr review done`: the agent finished every `:submit` it has read.
fn report_done(
    session: &str,
    repo: &Path,
    message: Option<&str>,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    if store.get_review(&session_ref)?.pr_session_key.is_some() {
        return Err(TuicrError::InvalidInput(format!(
            "'{session}' is a PR session; agents report on local reviews only"
        )));
    }
    let message = message.map(str::trim).filter(|m| !m.is_empty());
    if !store.mark_agent_done(&session_ref, message)? {
        return Err(TuicrError::InvalidInput(format!(
            "no running tuicr has '{session}' open"
        )));
    }
    serde_json::to_writer_pretty(
        &mut *out,
        &serde_json::json!({ "session": session, "message": message }),
    )?;
    writeln!(out)?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct WaitOutput {
    session: String,
    path: String,
    submitted_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct AttachOutput {
    session: String,
    path: String,
    agent: String,
}

/// `tuicr review add --reply-to`: answer a local comment in its thread.
fn add_reply(
    session: &str,
    repo: &Path,
    parent_id: &str,
    options: AddCommentOptions,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let review = store.get_review(&session_ref)?;
    if review.pr_session_key.is_some() {
        return Err(TuicrError::InvalidInput(format!(
            "'{session}' is a PR session; --reply-to answers local comments only"
        )));
    }
    let content = options
        .content
        .ok_or_else(|| TuicrError::InvalidInput("a reply needs its text".to_string()))?;
    let config = config::load_config()
        .ok()
        .and_then(|outcome| outcome.config);
    let comment_type = CommentType::from_id(&options.comment_type);
    if let Some(warning) = unknown_comment_type_warning(&comment_type, config.as_ref()) {
        eprintln!("{warning}");
    }
    let author = resolve_cli_author(options.username, config.as_ref(), &review.repo_path);
    let reply = Comment::new(content, comment_type, None).with_author(author);
    let updated = store.add_reply(&session_ref, parent_id, reply)?;
    let output = collect_comments(&updated.0)
        .into_iter()
        .find(|comment| comment.id == updated.1.id)
        .ok_or_else(|| TuicrError::InvalidInput("the stored reply went missing".to_string()))?;
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

struct AddCommentOptions {
    input: Option<String>,
    comment_type: String,
    file: Option<PathBuf>,
    line: Option<u32>,
    end_line: Option<u32>,
    side: LineSideArg,
    username: Option<String>,
    content: Option<String>,
}

fn add_comment(
    session: &str,
    repo: &Path,
    options: AddCommentOptions,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let request_parts = build_add_request_parts(options)?;
    let target = request_parts.target;
    let comment_type = CommentType::from_id(&request_parts.comment_type);
    // One config read serves both the type check and the author fallback.
    let config = config::load_config()
        .ok()
        .and_then(|outcome| outcome.config);
    if let Some(warning) = unknown_comment_type_warning(&comment_type, config.as_ref()) {
        eprintln!("{warning}");
    }
    let repo_path = store.get_review(&session_ref)?.repo_path;
    let author = resolve_cli_author(request_parts.username, config.as_ref(), &repo_path);
    let comment = store.add_comment(
        &session_ref,
        AddCommentRequest {
            target: target.clone(),
            content: request_parts.content,
            comment_type,
            author,
            commit_id: None,
        },
    )?;
    let output = CommentOutput::from_target(&target, &comment);
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

struct AddRequestParts {
    target: CommentTarget,
    comment_type: String,
    content: String,
    username: Option<String>,
}

/// Resolve the author for a CLI-authored comment.
///
/// Priority: explicit `--username` / JSON `username` ► config `username` ►
/// `Comment::DEFAULT_AUTHOR`. Trims whitespace so `--username " "` doesn't
/// produce an awkward all-whitespace badge.
fn resolve_cli_author(
    explicit: Option<String>,
    config: Option<&config::AppConfig>,
    repo: &Path,
) -> String {
    if let Some(name) = explicit.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    config::resolved_username(config, git_fallback(config), Some(repo))
}

/// Whether "you" may fall back to git's `user.name`, as in the TUI.
fn git_fallback(config: Option<&config::AppConfig>) -> bool {
    config
        .and_then(|cfg| cfg.forge.as_ref())
        .is_some_and(|forge| forge.extended_comments)
}

/// Warn when `--type` names a type that no `comment_types` entry defines.
///
/// `CommentType::from_id` turns any non-empty id into `Custom`, so a typo is
/// indistinguishable from a configured type: it is stored, exported as
/// `**[TYPO]**`, and shown with no badge in the TUI. Warning rather than
/// rejecting keeps the CLI usable when `comment_types` is unset — the default,
/// under which every id but `none` is technically undefined — and keeps
/// scripted callers from breaking on a config change they don't control.
fn unknown_comment_type_warning(
    comment_type: &CommentType,
    config: Option<&config::AppConfig>,
) -> Option<String> {
    if comment_type.is_none() {
        return None;
    }
    let configured = config.and_then(|cfg| cfg.comment_types.as_deref())?;
    if configured
        .iter()
        .any(|definition| definition.id == comment_type.id())
    {
        return None;
    }
    let known: Vec<&str> = configured
        .iter()
        .map(|definition| definition.id.as_str())
        .collect();
    Some(format!(
        "Warning: comment type '{}' is not configured; known types: {}",
        comment_type.id(),
        known.join(", ")
    ))
}

fn build_add_request_parts(options: AddCommentOptions) -> Result<AddRequestParts> {
    let mut comment_type = options.comment_type;
    let mut content = options.content;
    let mut file = options.file;
    let mut line = options.line;
    let mut end_line = options.end_line;
    let mut side = options.side;
    let mut username = options.username;
    let mut target = None;

    if let Some(input) = options.input {
        let payload = parse_add_payload(&read_json_input(&input)?)?;
        if let Some(payload_comment_type) = payload.comment_type {
            comment_type = payload_comment_type;
        }
        if payload.content.is_some() {
            content = payload.content;
        }
        if payload.username.is_some() {
            username = payload.username;
        }
        if let Some(payload_target) = payload.target {
            target = Some(payload_target.into_comment_target()?);
        } else {
            if let Some(payload_file) = payload.file {
                file = Some(payload_file);
            }
            if payload.line.is_some() || payload.start_line.is_some() {
                line = payload.line.or(payload.start_line);
            }
            if let Some(payload_end_line) = payload.end_line {
                end_line = Some(payload_end_line);
            }
            if let Some(payload_side) = payload.side {
                side = parse_line_side(&payload_side)?;
            }
        }
    }

    let content = content.ok_or_else(|| {
        TuicrError::InvalidInput(
            "comment text is required either as COMMENT or JSON field `content`".to_string(),
        )
    })?;
    let target = match target {
        Some(target) => target,
        None => build_comment_target(file, line, end_line, side)?,
    };

    Ok(AddRequestParts {
        target,
        comment_type,
        content,
        username,
    })
}

fn read_json_input(input: &str) -> Result<String> {
    if input == "-" {
        let mut contents = String::new();
        io::stdin().read_to_string(&mut contents)?;
        return Ok(contents);
    }
    if let Some(path) = input.strip_prefix('@') {
        return fs::read_to_string(path).map_err(TuicrError::Io);
    }
    Ok(input.to_string())
}

fn parse_add_payload(input: &str) -> Result<AddCommentPayload> {
    serde_json::from_str(input)
        .map_err(|err| TuicrError::InvalidInput(format!("invalid JSON review payload: {err}")))
}

#[derive(Debug, Deserialize)]
struct AddCommentPayload {
    #[serde(default, alias = "type")]
    comment_type: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    target: Option<JsonCommentTarget>,
    #[serde(default)]
    file: Option<PathBuf>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default, alias = "author")]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JsonCommentTarget {
    #[serde(default, rename = "type", alias = "kind")]
    target_type: Option<String>,
    #[serde(default)]
    file: Option<PathBuf>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    side: Option<String>,
}

impl JsonCommentTarget {
    fn into_comment_target(self) -> Result<CommentTarget> {
        let side = match self.side {
            Some(side) => parse_line_side(&side)?,
            None => LineSideArg::New,
        };
        let inferred_type = if self.file.is_none() {
            "review"
        } else if self.line.is_some() || self.start_line.is_some() {
            if self.end_line.is_some() {
                "line_range"
            } else {
                "line"
            }
        } else {
            "file"
        };
        let target_type = self
            .target_type
            .unwrap_or_else(|| inferred_type.to_string())
            .replace('-', "_")
            .to_ascii_lowercase();

        match target_type.as_str() {
            "review" => Ok(CommentTarget::Review),
            "file" => Ok(CommentTarget::File {
                path: required_file(self.file, "target.file")?,
            }),
            "line" => Ok(CommentTarget::Line {
                path: required_file(self.file, "target.file")?,
                line: required_line(self.line.or(self.start_line), "target.line")?,
                side: line_side_arg_to_model(side),
            }),
            "line_range" | "range" => Ok(CommentTarget::LineRange {
                path: required_file(self.file, "target.file")?,
                range: LineRange::new(
                    required_line(self.line.or(self.start_line), "target.start_line")?,
                    required_line(self.end_line, "target.end_line")?,
                ),
                side: line_side_arg_to_model(side),
            }),
            other => Err(TuicrError::InvalidInput(format!(
                "unknown JSON target type '{other}'"
            ))),
        }
    }
}

fn required_file(path: Option<PathBuf>, name: &str) -> Result<PathBuf> {
    path.ok_or_else(|| TuicrError::InvalidInput(format!("{name} is required")))
}

fn required_line(line: Option<u32>, name: &str) -> Result<u32> {
    let line = line.ok_or_else(|| TuicrError::InvalidInput(format!("{name} is required")))?;
    validate_line(line, name)?;
    Ok(line)
}

fn parse_line_side(side: &str) -> Result<LineSideArg> {
    match side.to_ascii_lowercase().as_str() {
        "old" => Ok(LineSideArg::Old),
        "new" => Ok(LineSideArg::New),
        other => Err(TuicrError::InvalidInput(format!(
            "unknown side '{other}', expected 'old' or 'new'"
        ))),
    }
}

fn line_side_arg_to_model(side: LineSideArg) -> LineSide {
    match side {
        LineSideArg::Old => LineSide::Old,
        LineSideArg::New => LineSide::New,
    }
}

fn show_comments(
    session: &str,
    repo: &Path,
    needs_reply: Option<&str>,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    // The agent reading: stamped first, so a `:submit` after it stays open.
    if needs_reply.is_some() {
        store.mark_agent_read(&session_ref)?;
    }
    let session = store.get_review(&session_ref)?;
    let mut comments = collect_comments(&session);
    if let Some(agent) = needs_reply {
        comments = threads_waiting_on(comments, agent);
    }
    serde_json::to_writer_pretty(&mut *out, &comments)?;
    writeln!(out)?;
    Ok(())
}

fn resolve_session_ref(store: &ReviewStore, repo: &Path, session: &str) -> Result<SessionRef> {
    let direct_path = PathBuf::from(session);
    if direct_path.exists() || direct_path.is_absolute() || session.ends_with(".json") {
        return Ok(SessionRef::from_path(direct_path));
    }

    // PR sessions are keyed by forge coordinates, not a local checkout, so they
    // resolve from the manifest by slug rather than the per-repo listing.
    if matches!(session.parse::<Slug>(), Ok(Slug::Pr(_))) {
        return match store.resolve_pr_session(session)? {
            Some(session_ref) => Ok(session_ref),
            None => Err(TuicrError::InvalidInput(format!(
                "no PR session found for '{session}'. Run `tuicr review list --all` to see available sessions."
            ))),
        };
    }

    let matches: Vec<_> = store
        .list_sessions_for_repo(repo)?
        .into_iter()
        .filter(|summary| summary.slug == session)
        .collect();
    match matches.as_slice() {
        [summary] => Ok(summary.session_ref.clone()),
        [] => Err(TuicrError::InvalidInput(format!(
            "session '{session}' was not found for repo {}. Run `tuicr review list --repo {}` to see available sessions.",
            repo.display(),
            repo.display()
        ))),
        _ => Err(TuicrError::InvalidInput(format!(
            "session '{session}' is ambiguous for repo {}",
            repo.display()
        ))),
    }
}

fn build_comment_target(
    file: Option<PathBuf>,
    line: Option<u32>,
    end_line: Option<u32>,
    side: LineSideArg,
) -> Result<CommentTarget> {
    let side = match side {
        LineSideArg::Old => LineSide::Old,
        LineSideArg::New => LineSide::New,
    };

    match (file, line, end_line) {
        (None, None, None) => Ok(CommentTarget::Review),
        (Some(path), None, None) => Ok(CommentTarget::File { path }),
        (Some(path), Some(line), None) => {
            validate_line(line, "--line")?;
            Ok(CommentTarget::Line { path, line, side })
        }
        (Some(path), Some(start), Some(end)) => {
            validate_line(start, "--line")?;
            validate_line(end, "--end-line")?;
            Ok(CommentTarget::LineRange {
                path,
                range: LineRange::new(start, end),
                side,
            })
        }
        (None, Some(_), _) => Err(TuicrError::InvalidInput(
            "--line requires --target-file for review comments".to_string(),
        )),
        (None, None, Some(_)) => Err(TuicrError::InvalidInput(
            "--end-line requires --line and --target-file".to_string(),
        )),
        (Some(_), None, Some(_)) => Err(TuicrError::InvalidInput(
            "--end-line requires --line".to_string(),
        )),
    }
}

fn validate_line(line: u32, name: &str) -> Result<()> {
    if line == 0 {
        return Err(TuicrError::InvalidInput(format!(
            "{name} must be greater than zero"
        )));
    }
    Ok(())
}

fn collect_comments(session: &ReviewSession) -> Vec<CommentOutput> {
    let mut comments = Vec::new();
    for comment in &session.review_comments {
        comments.push(CommentOutput::from_parts(
            "review".to_string(),
            None,
            None,
            None,
            None,
            comment,
        ));
    }

    let mut files: Vec<_> = session.files.iter().collect();
    files.sort_by_key(|(path, _)| path.as_os_str().to_os_string());
    for (path, review) in files {
        let path_display = path.to_string_lossy().to_string();
        for comment in &review.file_comments {
            comments.push(CommentOutput::from_parts(
                path_display.clone(),
                Some(path_display.clone()),
                None,
                None,
                None,
                comment,
            ));
        }

        let mut line_comments: Vec<_> = review.line_comments.iter().collect();
        line_comments.sort_by_key(|(line, _)| *line);
        for (line, line_comments) in line_comments {
            for comment in line_comments {
                let (start_line, end_line) = comment
                    .line_range
                    .map(|range| (range.start, range.end))
                    .unwrap_or((*line, *line));
                let location = line_location(&path_display, start_line, end_line, comment.side);
                comments.push(CommentOutput::from_parts(
                    location,
                    Some(path_display.clone()),
                    Some(start_line),
                    Some(end_line),
                    comment.side,
                    comment,
                ));
            }
        }
    }

    comments
}

/// Whole threads whose last message is not `agent`'s: its reply answers a
/// thread, anyone's next message reopens it. `comments` holds every thread's
/// messages, in order.
fn threads_waiting_on(comments: Vec<CommentOutput>, agent: &str) -> Vec<CommentOutput> {
    let thread = |comment: &CommentOutput| {
        comment
            .reply_to
            .clone()
            .unwrap_or_else(|| comment.id.clone())
    };
    let mut last_author = std::collections::HashMap::new();
    for comment in &comments {
        last_author.insert(thread(comment), comment.author.clone());
    }
    comments
        .into_iter()
        .filter(|comment| last_author.get(&thread(comment)).map(String::as_str) != Some(agent))
        .collect()
}

fn line_location(path: &str, start_line: u32, end_line: u32, side: Option<LineSide>) -> String {
    let line = if start_line == end_line {
        start_line.to_string()
    } else {
        format!("{start_line}-{end_line}")
    };
    match side {
        Some(LineSide::Old) => format!("{path}:{line} [old]"),
        _ => format!("{path}:{line}"),
    }
}

fn target_location(target: &CommentTarget) -> String {
    match target {
        CommentTarget::Review => "review".to_string(),
        CommentTarget::File { path } => path.display().to_string(),
        CommentTarget::Line { path, line, side } => {
            line_location(&path.to_string_lossy(), *line, *line, Some(*side))
        }
        CommentTarget::LineRange { path, range, side } => {
            line_location(&path.to_string_lossy(), range.start, range.end, Some(*side))
        }
    }
}

fn side_id(side: Option<LineSide>) -> Option<&'static str> {
    match side {
        Some(LineSide::Old) => Some("old"),
        Some(LineSide::New) => Some("new"),
        None => None,
    }
}

fn lifecycle_id(state: CommentLifecycleState) -> &'static str {
    match state {
        CommentLifecycleState::LocalDraft => "local_draft",
        CommentLifecycleState::PushedDraft => "pushed_draft",
        CommentLifecycleState::Submitted => "submitted",
    }
}

#[derive(Debug, Serialize)]
struct SessionSummaryOutput {
    slug: String,
    kind: &'static str,
    path: String,
    updated_at: String,
    comment_count: usize,
    reviewed_count: usize,
    file_count: usize,
    anchor: String,
    active: bool,
    /// Pid of the tuicr that has the session open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    /// The agent `tuicr review attach` registered, while a tuicr has it open.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
}

impl From<SessionSummary> for SessionSummaryOutput {
    fn from(summary: SessionSummary) -> Self {
        Self {
            slug: summary.slug,
            kind: summary.kind.id(),
            path: summary.session_ref.path().display().to_string(),
            updated_at: summary.updated_at.to_rfc3339(),
            comment_count: summary.comment_count,
            reviewed_count: summary.reviewed_count,
            file_count: summary.file_count,
            anchor: summary.anchor,
            active: summary.active,
            pid: None,
            agent: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct CommentOutput {
    id: String,
    location: String,
    path: Option<String>,
    start_line: Option<u32>,
    end_line: Option<u32>,
    side: Option<&'static str>,
    comment_type: String,
    author: String,
    lifecycle_state: &'static str,
    created_at: String,
    content: String,
    /// The first comment of the thread this one answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
}

impl CommentOutput {
    fn from_target(target: &CommentTarget, comment: &Comment) -> Self {
        let (path, start_line, end_line, side) = match target {
            CommentTarget::Review => (None, None, None, None),
            CommentTarget::File { path } => (Some(path.display().to_string()), None, None, None),
            CommentTarget::Line { path, line, side } => (
                Some(path.display().to_string()),
                Some(*line),
                Some(*line),
                Some(*side),
            ),
            CommentTarget::LineRange { path, range, side } => (
                Some(path.display().to_string()),
                Some(range.start),
                Some(range.end),
                Some(*side),
            ),
        };
        Self::from_parts(
            target_location(target),
            path,
            start_line,
            end_line,
            side,
            comment,
        )
    }

    fn from_parts(
        location: String,
        path: Option<String>,
        start_line: Option<u32>,
        end_line: Option<u32>,
        side: Option<LineSide>,
        comment: &Comment,
    ) -> Self {
        Self {
            id: comment.id.clone(),
            location,
            path,
            start_line,
            end_line,
            side: side_id(side),
            comment_type: comment.comment_type.id().to_string(),
            author: comment.author.clone(),
            lifecycle_state: lifecycle_id(comment.lifecycle_state),
            created_at: comment.created_at.to_rfc3339(),
            content: comment.content.clone(),
            reply_to: comment.reply_to.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    use crate::model::{FileStatus, SessionDiffSource};

    fn test_session(repo_path: PathBuf) -> ReviewSession {
        let mut session = ReviewSession::new(
            repo_path,
            "abc1234".to_string(),
            Some("main".to_string()),
            SessionDiffSource::WorkingTree,
        );
        session.add_file(PathBuf::from("src/main.rs"), FileStatus::Modified, 0);
        session
    }

    #[test]
    fn should_build_review_comment_target_by_default() {
        let target = build_comment_target(None, None, None, LineSideArg::New).unwrap();
        assert!(matches!(target, CommentTarget::Review));
    }

    #[test]
    fn should_build_line_range_comment_target() {
        let target = build_comment_target(
            Some(PathBuf::from("src/main.rs")),
            Some(12),
            Some(10),
            LineSideArg::Old,
        )
        .unwrap();

        assert!(matches!(
            target,
            CommentTarget::LineRange {
                range: LineRange { start: 10, end: 12 },
                side: LineSide::Old,
                ..
            }
        ));
    }

    #[test]
    fn should_reject_zero_line() {
        let err = build_comment_target(
            Some(PathBuf::from("src/main.rs")),
            Some(0),
            None,
            LineSideArg::New,
        )
        .unwrap_err();
        assert!(matches!(err, TuicrError::InvalidInput(_)));
    }

    #[test]
    fn should_build_add_request_from_flat_json_payload() {
        let parts = build_add_request_parts(AddCommentOptions {
            input: Some(
                r#"{"file":"src/main.rs","line":42,"side":"old","type":"issue","content":"fix it"}"#
                    .to_string(),
            ),
            comment_type: "note".to_string(),
            file: None,
            line: None,
            end_line: None,
            side: LineSideArg::New,
            username: None,
            content: None,
        })
        .unwrap();

        assert_eq!(parts.comment_type, "issue");
        assert_eq!(parts.content, "fix it");
        assert!(matches!(
            parts.target,
            CommentTarget::Line {
                path,
                line: 42,
                side: LineSide::Old,
            } if path.as_path() == Path::new("src/main.rs")
        ));
    }

    #[test]
    fn should_build_add_request_from_nested_json_payload() {
        let parts = build_add_request_parts(AddCommentOptions {
            input: Some(
                r#"{"comment_type":"suggestion","content":"collapse this","target":{"type":"line_range","file":"src/main.rs","start_line":5,"end_line":7}}"#
                    .to_string(),
            ),
            comment_type: "note".to_string(),
            file: None,
            line: None,
            end_line: None,
            side: LineSideArg::New,
            username: None,
            content: None,
        })
        .unwrap();

        assert_eq!(parts.comment_type, "suggestion");
        assert!(matches!(
            parts.target,
            CommentTarget::LineRange {
                range: LineRange { start: 5, end: 7 },
                side: LineSide::New,
                ..
            }
        ));
    }

    /// `tuicr review attach` against this thread's test reviews dir.
    fn attach(session: &Path, agent: &str) -> Result<String> {
        let mut out = Vec::new();
        run_with_writer(
            ReviewCommand::Attach {
                session: session.display().to_string(),
                repo: PathBuf::from("."),
                agent: agent.to_string(),
            },
            &mut out,
        )?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn should_attach_an_agent_only_while_a_tuicr_has_the_session_open() {
        // given: a saved local session
        let temp = tempdir().unwrap();
        storage::set_test_reviews_dir(Some(temp.path().join("reviews")));
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let session = test_session(repo.clone());
        let path = storage::save_session(&session).unwrap();

        // when: nothing has it open, attach is refused
        let err = attach(&path, "w1N:p1").unwrap_err();
        assert!(err.to_string().contains("no running tuicr"), "{err}");

        // when: a tuicr opens it
        storage::mark_session_active(&session, &path).unwrap();
        let out = attach(&path, "w1N:p1").unwrap();

        // then: attach succeeds, and the listing names the agent
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["agent"], "w1N:p1");
        let mut listed = Vec::new();
        list_sessions(&repo, false, &mut listed).unwrap();
        let listed: serde_json::Value = serde_json::from_slice(&listed).unwrap();
        assert_eq!(listed[0]["active"], true);
        assert_eq!(listed[0]["pid"], std::process::id());
        assert_eq!(listed[0]["agent"], "w1N:p1");
        storage::set_test_reviews_dir(None);
    }

    #[test]
    fn should_refuse_to_attach_an_agent_to_a_pr_session() {
        let temp = tempdir().unwrap();
        let reviews = temp.path().join("reviews");
        storage::set_test_reviews_dir(Some(reviews.clone()));
        let store = ReviewStore::with_reviews_dir(reviews);
        let session_ref = save_pr_session(&store);

        let err = attach(session_ref.path(), "w1N:p1").unwrap_err();

        assert!(err.to_string().contains("local reviews only"), "{err}");
        storage::set_test_reviews_dir(None);
    }

    fn run(command: ReviewCommand) -> Result<serde_json::Value> {
        let mut out = Vec::new();
        run_with_writer(command, &mut out)?;
        Ok(serde_json::from_slice(&out).unwrap())
    }

    fn reply(
        session: &Path,
        parent: &str,
        username: &str,
        text: &str,
    ) -> Result<serde_json::Value> {
        run(ReviewCommand::Add {
            session: session.display().to_string(),
            input: None,
            repo: PathBuf::from("."),
            comment_type: "none".to_string(),
            file: None,
            line: None,
            end_line: None,
            side: LineSideArg::New,
            username: Some(username.to_string()),
            reply_to: Some(parent.to_string()),
            content: Some(text.to_string()),
        })
    }

    fn comments(session: &Path, needs_reply: Option<&str>) -> Vec<String> {
        let listed = run(ReviewCommand::Comments {
            session: session.display().to_string(),
            repo: PathBuf::from("."),
            needs_reply: needs_reply.map(str::to_string),
        })
        .unwrap();
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                format!(
                    "{}: {}",
                    c["author"].as_str().unwrap(),
                    c["content"].as_str().unwrap()
                )
            })
            .collect()
    }

    #[test]
    fn should_reply_in_a_thread_and_list_only_the_threads_waiting_on_the_agent() {
        // given: two line comments by the user
        let temp = tempdir().unwrap();
        storage::set_test_reviews_dir(Some(temp.path().join("reviews")));
        let mut session = test_session(temp.path().join("repo"));
        let review = session.get_file_mut(&PathBuf::from("src/main.rs")).unwrap();
        let first = Comment::new(
            "rename it".into(),
            CommentType::default(),
            Some(LineSide::New),
        );
        let first_id = first.id.clone();
        review.add_line_comment(7, first);
        review.add_line_comment(
            9,
            Comment::new(
                "test it".into(),
                CommentType::default(),
                Some(LineSide::New),
            ),
        );
        let path = storage::save_session(&session).unwrap();
        let waiting = || comments(&path, Some("ai-agent"));
        assert_eq!(waiting(), ["user: rename it", "user: test it"]);

        // when: the agent answers the first
        let answer = reply(&path, &first_id, "ai-agent", "renamed").unwrap();

        // then: the reply is on its thread's line, and that thread is done
        assert_eq!(answer["location"], "src/main.rs:7");
        assert_eq!(answer["reply_to"], first_id.as_str());
        assert_eq!(waiting(), ["user: test it"]);

        // when: a teammate answers back, the thread waits again, whole
        reply(
            &path,
            answer["id"].as_str().unwrap(),
            "alice",
            "not like that",
        )
        .unwrap();
        assert_eq!(
            waiting(),
            [
                "user: rename it",
                "ai-agent: renamed",
                "alice: not like that",
                "user: test it"
            ]
        );
        storage::set_test_reviews_dir(None);
    }

    #[test]
    fn should_not_hand_the_agent_its_own_comments() {
        // given: a comment the agent started, nobody has answered
        let temp = tempdir().unwrap();
        storage::set_test_reviews_dir(Some(temp.path().join("reviews")));
        let mut session = test_session(temp.path().join("repo"));
        let review = session.get_file_mut(&PathBuf::from("src/main.rs")).unwrap();
        review.add_line_comment(
            7,
            Comment::new("found a bug".into(), CommentType::default(), None)
                .with_author("ai-agent".to_string()),
        );
        let path = storage::save_session(&session).unwrap();

        // then: nothing waits on the agent, though the user's view still has it
        assert!(comments(&path, Some("ai-agent")).is_empty());
        assert_eq!(comments(&path, None), ["ai-agent: found a bug"]);
        storage::set_test_reviews_dir(None);
    }

    #[test]
    fn should_refuse_a_reply_in_a_pr_session_or_to_a_missing_comment() {
        let temp = tempdir().unwrap();
        let reviews = temp.path().join("reviews");
        storage::set_test_reviews_dir(Some(reviews.clone()));
        let store = ReviewStore::with_reviews_dir(reviews);
        let pr = save_pr_session(&store);
        let local = storage::save_session(&test_session(temp.path().join("repo"))).unwrap();

        let in_pr = reply(pr.path(), "any", "Claude", "hi").unwrap_err();
        let missing = reply(&local, "nope", "Claude", "hi").unwrap_err();

        assert!(in_pr.to_string().contains("local comments only"), "{in_pr}");
        assert!(
            missing.to_string().contains("no comment with id"),
            "{missing}"
        );
        storage::set_test_reviews_dir(None);
    }

    #[test]
    fn should_wait_until_the_session_is_submitted() {
        // given: a local session a tuicr (this process) has open
        let temp = tempdir().unwrap();
        let reviews = temp.path().join("reviews");
        storage::set_test_reviews_dir(Some(reviews.clone()));
        let session = test_session(temp.path().join("repo"));
        let path = storage::save_session(&session).unwrap();
        storage::mark_session_active(&session, &path).unwrap();

        // when: `:submit` lands while the agent waits
        let submit = std::thread::spawn({
            let reviews = reviews.clone();
            move || {
                std::thread::sleep(WAIT_POLL * 3);
                storage::notify_waiter_for_pid_in_dir(&reviews, std::process::id()).unwrap()
            }
        });
        let mut out = Vec::new();
        wait_for_submit(&path.display().to_string(), Path::new("."), &mut out).unwrap();

        // then: the wait returns with the session
        assert!(submit.join().unwrap(), "the submit reached a waiter");
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(value["submitted_at"].is_string());
        storage::set_test_reviews_dir(None);
    }

    #[test]
    fn should_stop_waiting_when_the_tuicr_closes() {
        // given: a local session a tuicr (this process) has open
        let temp = tempdir().unwrap();
        let reviews = temp.path().join("reviews");
        storage::set_test_reviews_dir(Some(reviews.clone()));
        let session = test_session(temp.path().join("repo"));
        let path = storage::save_session(&session).unwrap();
        storage::mark_session_active(&session, &path).unwrap();

        // when: the tuicr quits while the agent waits
        let quit = std::thread::spawn({
            let reviews = reviews.clone();
            move || {
                std::thread::sleep(WAIT_POLL * 3);
                storage::clear_active_session_for_pid_in_dir(&reviews).unwrap();
            }
        });
        let err = wait_for_submit(&path.display().to_string(), Path::new("."), &mut Vec::new())
            .unwrap_err();
        quit.join().unwrap();

        // then
        assert!(err.to_string().contains("was closed"), "{err}");
        storage::set_test_reviews_dir(None);
    }

    #[test]
    fn should_report_done_covering_the_agents_last_read() {
        // given: a local session a tuicr (this process) has open
        let temp = tempdir().unwrap();
        let reviews = temp.path().join("reviews");
        storage::set_test_reviews_dir(Some(reviews.clone()));
        let session = test_session(temp.path().join("repo"));
        let path = storage::save_session(&session).unwrap();
        storage::mark_session_active(&session, &path).unwrap();

        // when: the agent reads what waits on it, then reports done
        comments(&path, Some("ai-agent"));
        let done = run(ReviewCommand::Done {
            session: path.display().to_string(),
            repo: PathBuf::from("."),
            message: Some("3 threads answered".to_string()),
        })
        .unwrap();

        // then: the done covers that read
        assert_eq!(done["message"], "3 threads answered");
        let progress = storage::agent_progress_for_pid_in_dir(&reviews, std::process::id())
            .unwrap()
            .unwrap();
        let done = progress.done.unwrap();
        assert!(done.covers.is_some());
        assert!(done.covers <= Some(done.at));
        assert_eq!(done.message.as_deref(), Some("3 threads answered"));
        storage::set_test_reviews_dir(None);
    }

    #[test]
    fn should_refuse_done_for_a_pr_or_unopened_session() {
        let temp = tempdir().unwrap();
        let reviews = temp.path().join("reviews");
        storage::set_test_reviews_dir(Some(reviews.clone()));
        let store = ReviewStore::with_reviews_dir(reviews);
        let pr = save_pr_session(&store);
        let local = storage::save_session(&test_session(temp.path().join("repo"))).unwrap();
        let done = |session: &Path| {
            run(ReviewCommand::Done {
                session: session.display().to_string(),
                repo: PathBuf::from("."),
                message: None,
            })
            .unwrap_err()
            .to_string()
        };

        assert!(done(pr.path()).contains("local reviews only"));
        assert!(done(&local).contains("no running tuicr"));
        storage::set_test_reviews_dir(None);
    }

    #[test]
    fn should_refuse_to_wait_on_a_pr_or_unopened_session() {
        let temp = tempdir().unwrap();
        let reviews = temp.path().join("reviews");
        storage::set_test_reviews_dir(Some(reviews.clone()));
        let store = ReviewStore::with_reviews_dir(reviews);
        let pr = save_pr_session(&store);
        let local = storage::save_session(&test_session(temp.path().join("repo"))).unwrap();

        let in_pr = wait_for_submit(
            &pr.path().display().to_string(),
            Path::new("."),
            &mut Vec::new(),
        )
        .unwrap_err();
        let unopened = wait_for_submit(
            &local.display().to_string(),
            Path::new("."),
            &mut Vec::new(),
        )
        .unwrap_err();

        assert!(in_pr.to_string().contains("local reviews only"), "{in_pr}");
        assert!(
            unopened.to_string().contains("no running tuicr"),
            "{unopened}"
        );
        storage::set_test_reviews_dir(None);
    }

    fn save_pr_session(store: &ReviewStore) -> SessionRef {
        use crate::forge::traits::{ForgeRepository, PrSessionKey};

        let key = PrSessionKey::new(
            ForgeRepository::github("github.com", "slatedb", "slatedb"),
            1745,
            "43e3566924690c06a45b2177b4dd2df59a0f09c6".to_string(),
        );
        let mut session = ReviewSession::new(
            PathBuf::from("forge:github.com/slatedb/slatedb"),
            key.head_sha.clone(),
            Some("reviews".to_string()),
            SessionDiffSource::PullRequest,
        );
        session.pr_session_key = Some(key);
        store.save_review(&session).unwrap()
    }

    #[test]
    fn should_find_pr_session_by_repo_coordinate() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        let session_ref = save_pr_session(&store);

        // A bare repo coordinate surfaces the PR session and emits its slug.
        let listed = store
            .list_sessions_for_repo(Path::new("slatedb/slatedb"))
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].slug, "gh:slatedb/slatedb/pr/1745");
        assert_eq!(listed[0].kind, crate::review_store::SessionKind::Pr);

        // The emitted slug resolves the same way regardless of --repo.
        let resolved =
            resolve_session_ref(&store, Path::new("slatedb/slatedb"), &listed[0].slug).unwrap();
        assert_eq!(resolved, session_ref);
    }

    #[test]
    fn should_match_pr_session_via_forge_repo_path_coordinate() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        save_pr_session(&store);

        // The `forge:host/owner/repo` form (as stored on disk) also resolves.
        let listed = store
            .list_sessions_for_repo(Path::new("forge:github.com/slatedb/slatedb"))
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].slug, "gh:slatedb/slatedb/pr/1745");
    }

    #[test]
    fn should_not_match_pr_session_for_unrelated_repo() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        save_pr_session(&store);

        assert!(
            store
                .list_sessions_for_repo(Path::new("other/project"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn should_list_pr_session_in_list_all() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        save_pr_session(&store);

        let all = store.list_all_sessions().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].slug, "gh:slatedb/slatedb/pr/1745");
    }

    #[test]
    fn should_resolve_pr_session_by_slug_without_repo() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        let session_ref = save_pr_session(&store);

        // PR slugs are self-contained: `--repo` is irrelevant.
        let resolved =
            resolve_session_ref(&store, Path::new("."), "gh:slatedb/slatedb/pr/1745").unwrap();
        assert_eq!(resolved, session_ref);
    }

    #[test]
    fn should_error_for_unknown_pr_slug() {
        let temp = tempdir().unwrap();
        let reviews = temp.path().join("reviews");
        let store = ReviewStore::with_reviews_dir(&reviews);
        let err = resolve_session_ref(&store, Path::new("."), "gh:nope/nope/pr/9999").unwrap_err();
        assert!(matches!(err, TuicrError::InvalidInput(_)));
    }

    #[test]
    fn should_list_add_and_show_comments() {
        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let reviews = temp.path().join("reviews");
        let store = ReviewStore::with_reviews_dir(&reviews);
        let session = test_session(repo.clone());
        let session_ref = store.save_review(&session).unwrap();

        let mut out = Vec::new();
        let sessions = store.list_sessions_for_repo(&repo).unwrap();
        assert_eq!(sessions.len(), 1);
        let slug = sessions[0].slug.clone();

        let resolved = resolve_session_ref(&store, &repo, &slug).unwrap();
        assert_eq!(resolved, session_ref);

        let comment = store
            .add_comment(
                &resolved,
                AddCommentRequest {
                    target: CommentTarget::Line {
                        path: PathBuf::from("src/main.rs"),
                        line: 42,
                        side: LineSide::New,
                    },
                    content: "check this".to_string(),
                    comment_type: CommentType::from_id("issue"),
                    author: "review-agent".to_string(),
                    commit_id: None,
                },
            )
            .unwrap();

        let loaded = store.get_review(&session_ref).unwrap();
        let comments = collect_comments(&loaded);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, comment.id);
        assert_eq!(comments[0].location, "src/main.rs:42");
        assert_eq!(comments[0].comment_type, "issue");
        assert_eq!(comments[0].author, "review-agent");

        show_comments(
            &session_ref.path().display().to_string(),
            &repo,
            None,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value[0]["comment_type"], "issue");
        assert_eq!(value[0]["author"], "review-agent");
        assert_eq!(value[0]["location"], "src/main.rs:42");
        assert_eq!(value[0]["content"], "check this");
    }

    /// Every CLI command that can empty a session must leave it in place
    /// while a running tuicr has it open (#769).
    #[test]
    fn should_keep_an_open_session_when_any_cli_command_empties_it() {
        type MakeCommand = fn(String, String, PathBuf) -> ReviewCommand;
        let cases: [(&str, MakeCommand); 3] = [
            ("delete", |session, comment_id, repo| {
                ReviewCommand::Delete {
                    session,
                    comment_id,
                    repo,
                }
            }),
            ("clearc", |session, _, repo| ReviewCommand::Clearc {
                session,
                repo,
            }),
            ("clear", |session, _, repo| ReviewCommand::Clear {
                session,
                repo,
            }),
        ];
        for (name, make_command) in cases {
            // given — a session with one comment, open in a running TUI
            let temp = tempdir().unwrap();
            let repo = temp.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            let reviews = temp.path().join("reviews");
            storage::set_test_reviews_dir(Some(reviews.clone()));
            let store = ReviewStore::with_reviews_dir(&reviews);
            let session = test_session(repo.clone());
            let session_ref = store.save_review(&session).unwrap();
            let comment = store
                .add_comment(
                    &session_ref,
                    AddCommentRequest {
                        target: CommentTarget::Review,
                        content: "question?".to_string(),
                        comment_type: CommentType::from_id("note"),
                        author: "reviewer".to_string(),
                        commit_id: None,
                    },
                )
                .unwrap();
            storage::mark_session_active_in_dir(&session, session_ref.path(), &reviews).unwrap();
            let session_arg = session_ref.path().display().to_string();

            // when — an agent removes the last comment through the CLI
            let result = run_with_writer(
                make_command(session_arg, comment.id, repo.clone()),
                &mut Vec::new(),
            );

            // then — the file stays, so the TUI and the agent keep sharing it
            let kept = session_ref.path().exists();
            storage::set_test_reviews_dir(None);
            result.unwrap_or_else(|e| panic!("review {name} failed: {e}"));
            assert!(kept, "review {name} must keep an open session");
            assert!(
                store
                    .get_review(&session_ref)
                    .unwrap()
                    .review_comments
                    .is_empty(),
                "review {name} must still remove the comment"
            );
        }
    }

    fn config_with_types(ids: &[&str]) -> config::AppConfig {
        config::AppConfig {
            comment_types: Some(
                ids.iter()
                    .map(|id| config::CommentTypeConfig {
                        id: (*id).to_string(),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn should_warn_when_type_is_not_configured() {
        // given a config declaring note and issue
        let config = config_with_types(&["note", "issue"]);

        // when a typo slips through
        let warning = unknown_comment_type_warning(&CommentType::from_id("isue"), Some(&config));

        // then the id and the valid ids are both named
        let warning = warning.expect("an unconfigured type should warn");
        assert!(warning.contains("'isue'"), "got {warning}");
        assert!(warning.contains("note, issue"), "got {warning}");
    }

    #[test]
    fn should_not_warn_for_a_configured_type() {
        let config = config_with_types(&["note", "issue"]);
        assert!(
            unknown_comment_type_warning(&CommentType::from_id("issue"), Some(&config)).is_none()
        );
    }

    #[test]
    fn should_not_warn_when_comment_types_are_unconfigured() {
        // `comment_types` is unset by default, which leaves every id but
        // `none` undefined. Warning there would fire on every typed comment
        // the agent skill documents, so an absent list means no opinion.
        assert!(unknown_comment_type_warning(&CommentType::from_id("issue"), None).is_none());
        assert!(
            unknown_comment_type_warning(
                &CommentType::from_id("issue"),
                Some(&config::AppConfig::default())
            )
            .is_none()
        );
    }

    #[test]
    fn should_not_warn_for_the_untyped_default() {
        // `--type none` is always valid and never carries a badge.
        let config = config_with_types(&["issue"]);
        assert!(
            unknown_comment_type_warning(&CommentType::from_id("none"), Some(&config)).is_none()
        );
    }

    #[test]
    fn should_still_store_a_comment_whose_type_is_unconfigured() {
        // given a session and a type no config declares
        let dir = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(dir.path());
        let session = test_session(PathBuf::from("/tmp/repo"));
        let session_ref = store.save_review(&session).unwrap();

        // when
        let comment = store
            .add_comment(
                &session_ref,
                AddCommentRequest {
                    target: CommentTarget::File {
                        path: PathBuf::from("src/main.rs"),
                    },
                    content: "body".to_string(),
                    comment_type: CommentType::from_id("isue"),
                    author: "Codex".to_string(),
                    commit_id: None,
                },
            )
            .expect("an unconfigured type must not block the write");

        // then the warning is advisory only — the comment is still stored
        assert_eq!(comment.comment_type.id(), "isue");
    }

    #[test]
    fn should_include_author_in_add_comment_output() {
        let comment = Comment::new(
            "check this".to_string(),
            CommentType::from_id("issue"),
            Some(LineSide::New),
        )
        .with_author("review-agent");
        let value = serde_json::to_value(CommentOutput::from_target(
            &CommentTarget::Line {
                path: PathBuf::from("src/main.rs"),
                line: 42,
                side: LineSide::New,
            },
            &comment,
        ))
        .unwrap();
        assert_eq!(value["author"], "review-agent");
    }
}
