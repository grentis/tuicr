use std::path::PathBuf;
use std::time::Instant;

use super::pr_info_tests::build_pr_app;
use crate::app::{AnnotatedLine, App, DiffSource, DiffViewMode, InputMode, SubmitInFlightState};
use crate::forge::remote_comments::{
    PrCommentsVisibility, RemoteCommentSide, RemoteReviewComment, RemoteReviewThread,
};
use crate::forge::submit::SubmitEvent;
use crate::forge::traits::{ForgeKind, ForgeRepository, GhCreateReviewResponse};
use crate::handler::{handle_comment_action, handle_diff_action};
use crate::input::Action;
use crate::model::comment::CommentLifecycleState;
use crate::model::{DiffFile, DiffHunk, DiffLine, FileStatus, LineOrigin, LineSide};

fn thread(id: &str, line: Option<u32>) -> RemoteReviewThread {
    RemoteReviewThread {
        id: id.to_string(),
        path: "src/lib.rs".to_string(),
        line,
        side: RemoteCommentSide::Right,
        is_resolved: false,
        is_outdated: line.is_none(),
        comments: vec![RemoteReviewComment {
            id: format!("{id}-root"),
            author: Some("alice".to_string()),
            body: "Why this?".to_string(),
            created_at: None,
            in_reply_to: None,
            url: "https://github.com/owner/repo/pull/42#discussion_r1".to_string(),
        }],
    }
}

/// The PR app with `[forge] extended_comments` on, three lines of
/// `src/lib.rs` in the diff and `threads` rendered, the cursor on the first
/// thread.
fn app_with_threads(threads: Vec<RemoteReviewThread>) -> App {
    let mut app = build_pr_app();
    app.forge_config.extended_comments = true;
    let lines = (1..=3)
        .map(|n| DiffLine {
            origin: LineOrigin::Context,
            content: format!("line {n}"),
            old_lineno: Some(n),
            new_lineno: Some(n),
            highlighted_spans: None,
        })
        .collect();
    let hunks = vec![DiffHunk {
        header: "@@ -1,3 +1,3 @@".to_string(),
        lines,
        old_start: 1,
        old_count: 3,
        new_start: 1,
        new_count: 3,
    }];
    app.diff_files = vec![DiffFile {
        old_path: None,
        new_path: Some(PathBuf::from("src/lib.rs")),
        status: FileStatus::Modified,
        content_hash: DiffFile::compute_content_hash(&hunks),
        hunks,
        is_binary: false,
        is_too_large: false,
        is_commit_message: false,
    }];
    app.session.remote_comments_visibility = PrCommentsVisibility::All;
    app.forge_review_threads = threads;
    app.rebuild_annotations();
    app.diff_state.cursor_line = app
        .line_annotations
        .iter()
        .position(|line| matches!(line, AnnotatedLine::RemoteThreadLine { .. }))
        .expect("thread is rendered");
    app
}

fn reply(app: &mut App, text: &str) {
    handle_diff_action(app, Action::AddLineComment);
    for c in text.chars() {
        handle_comment_action(app, Action::InsertChar(c));
    }
    handle_comment_action(app, Action::SubmitInput);
}

#[test]
fn should_open_a_reply_on_the_thread_line() {
    let mut app = app_with_threads(vec![thread("PRRT_1", Some(2))]);

    handle_diff_action(&mut app, Action::AddLineComment);

    assert_eq!(app.input_mode, InputMode::Comment);
    assert_eq!(app.comment_line, Some((2, LineSide::New)));
    assert_eq!(
        app.comment_reply.as_ref().map(|t| t.thread_id.as_str()),
        Some("PRRT_1")
    );
    assert!(app.message.as_ref().unwrap().content.contains("@alice"));
}

#[test]
fn should_save_the_reply_as_a_draft_attached_to_the_thread() {
    let mut app = app_with_threads(vec![thread("PRRT_1", Some(2))]);

    reply(&mut app, "Because of X");

    assert_eq!(app.input_mode, InputMode::Normal);
    let review = app.session.files.get(&PathBuf::from("src/lib.rs")).unwrap();
    let saved = &review.line_comments[&2][0];
    assert_eq!(saved.content, "Because of X");
    assert_eq!(saved.reply_to_thread.as_deref(), Some("PRRT_1"));
    assert_eq!(saved.side, Some(LineSide::New));
    assert_eq!(saved.lifecycle_state, CommentLifecycleState::LocalDraft);
    assert_eq!(
        app.reply_label(saved).as_deref(),
        Some("\u{21b3} reply to @alice ")
    );
}

#[test]
fn should_attach_a_reply_to_an_outdated_thread_at_file_level() {
    let mut app = app_with_threads(vec![thread("PRRT_old", None)]);

    reply(&mut app, "Still relevant");

    let review = app.session.files.get(&PathBuf::from("src/lib.rs")).unwrap();
    assert!(review.line_comments.is_empty());
    assert_eq!(
        review.file_comments[0].reply_to_thread.as_deref(),
        Some("PRRT_old")
    );
}

#[test]
fn should_submit_replies_apart_from_inline_comments() {
    let mut app = app_with_threads(vec![thread("PRRT_1", Some(2))]);
    reply(&mut app, "Because of X");

    app.start_submit(SubmitEvent::Comment);

    let state = app
        .submit_state
        .as_ref()
        .expect("a reply alone is enough to submit");
    assert!(state.mappable.is_empty());
    assert!(state.unmappable.is_empty());
    assert_eq!(state.replies.len(), 1);
    assert_eq!(state.replies[0].thread_id, "PRRT_1");
    assert_eq!(state.replies[0].body, "Because of X");
}

#[test]
fn should_lock_replies_once_submitted() {
    let mut app = app_with_threads(vec![thread("PRRT_1", Some(2))]);
    reply(&mut app, "Because of X");
    app.start_submit(SubmitEvent::Comment);
    let state = app.submit_state.take().unwrap();

    app.apply_submit_success(
        &SubmitInFlightState {
            event: SubmitEvent::Comment,
            mappable: Vec::new(),
            replies: state.replies,
            summary_comment_ids: Vec::new(),
            review_comment_ids: Vec::new(),
            moved_to_summary_count: 0,
            head_sha_snapshot: "abc1234567890".to_string(),
            repository: ForgeRepository::github("github.com", "owner", "repo"),
            pr_number: 42,
            started_at: Instant::now(),
        },
        &GhCreateReviewResponse {
            id: 7,
            html_url: String::new(),
            state: "COMMENT".to_string(),
        },
    );

    let review = app.session.files.get(&PathBuf::from("src/lib.rs")).unwrap();
    let saved = &review.line_comments[&2][0];
    assert_eq!(saved.lifecycle_state, CommentLifecycleState::Submitted);
    assert_eq!(saved.remote_review_id.as_deref(), Some("7"));
}

#[test]
fn should_not_reply_on_other_forges() {
    let mut app = app_with_threads(vec![thread("PRRT_1", Some(2))]);
    if let DiffSource::PullRequest(pr) = &mut app.diff_source {
        pr.key.repository.kind = ForgeKind::GitLab;
    }

    handle_diff_action(&mut app, Action::AddLineComment);

    assert_eq!(app.input_mode, InputMode::Normal);
    assert!(app.comment_reply.is_none());
    assert!(
        app.message
            .as_ref()
            .unwrap()
            .content
            .contains("not supported")
    );
}

#[test]
fn should_still_comment_on_a_diff_line() {
    let mut app = app_with_threads(vec![thread("PRRT_1", Some(2))]);
    app.diff_state.cursor_line = app
        .line_annotations
        .iter()
        .position(|line| matches!(line, AnnotatedLine::DiffLine { .. }))
        .unwrap();

    handle_diff_action(&mut app, Action::AddLineComment);

    assert_eq!(app.input_mode, InputMode::Comment);
    assert!(app.comment_reply.is_none());
}

#[test]
fn should_not_reply_with_extended_comments_off() {
    let mut app = app_with_threads(vec![thread("T1", Some(2))]);
    app.forge_config.extended_comments = false;

    handle_diff_action(&mut app, Action::AddLineComment);

    assert!(app.comment_reply.is_none());
}

/// What follows the diff lines, in order: forge thread rows and local comments.
fn notes_order(app: &App) -> Vec<&'static str> {
    let mut order: Vec<&'static str> = Vec::new();
    for line in &app.line_annotations {
        let note = match line {
            AnnotatedLine::RemoteThreadLine { .. } => "thread",
            AnnotatedLine::LineComment { .. } => "local",
            _ => continue,
        };
        if order.last() != Some(&note) {
            order.push(note);
        }
    }
    order
}

#[test]
fn should_put_forge_threads_before_local_comments() {
    for mode in [DiffViewMode::Unified, DiffViewMode::SideBySide] {
        let mut app = app_with_threads(vec![thread("T1", Some(2))]);
        reply(&mut app, "Because of X");
        app.diff_view_mode = mode;

        app.rebuild_annotations();

        assert_eq!(notes_order(&app), ["thread", "local"], "{mode:?}");
    }
}

#[test]
fn should_keep_local_comments_first_with_extended_comments_off() {
    for mode in [DiffViewMode::Unified, DiffViewMode::SideBySide] {
        let mut app = app_with_threads(vec![thread("T1", Some(2))]);
        reply(&mut app, "Because of X");
        app.forge_config.extended_comments = false;
        app.diff_view_mode = mode;

        app.rebuild_annotations();

        assert_eq!(notes_order(&app), ["local", "thread"], "{mode:?}");
    }
}
