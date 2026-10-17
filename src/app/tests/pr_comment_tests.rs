use super::pr_info_tests::build_pr_app;
use crate::app::{App, DiffSource, InputMode, PrCommentEvent};
use crate::forge::traits::PullRequestIssueComment;

fn pr_app() -> App {
    let mut app = build_pr_app();
    app.forge_config.extended_comments = true;
    app
}

fn message(app: &App) -> String {
    app.message
        .as_ref()
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// Hand `app` a finished post, as the worker thread would.
fn land_post(app: &mut App, result: Result<PullRequestIssueComment, (String, String)>) {
    let DiffSource::PullRequest(pr) = &app.diff_source else {
        panic!("not a PR app");
    };
    let key = pr.key.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(PrCommentEvent::Done { key, result }).unwrap();
    app.pr_comment_rx = Some(rx);
}

#[test]
fn should_open_the_pr_comment_box() {
    let mut app = pr_app();

    app.enter_pr_comment_mode();

    assert_eq!(app.input_mode, InputMode::Comment);
    assert!(app.comment_is_pr_comment);
    assert!(app.comment_is_review_level);
}

#[test]
fn should_not_open_with_extended_comments_off() {
    let mut app = pr_app();
    app.forge_config.extended_comments = false;

    app.enter_pr_comment_mode();

    assert_ne!(app.input_mode, InputMode::Comment);
    assert!(message(&app).contains("extended_comments"));
}

#[test]
fn should_not_open_in_a_local_review() {
    let mut app = pr_app();
    app.diff_source = DiffSource::WorkingTree;

    app.enter_pr_comment_mode();

    assert_ne!(app.input_mode, InputMode::Comment);
}

#[test]
fn should_not_open_while_a_post_is_in_flight() {
    let mut app = pr_app();
    let (_tx, rx) = std::sync::mpsc::channel();
    app.pr_comment_rx = Some(rx);

    app.enter_pr_comment_mode();

    assert_ne!(app.input_mode, InputMode::Comment);
}

#[test]
fn should_keep_the_comment_type_fixed() {
    let mut app = pr_app();
    app.enter_pr_comment_mode();
    let before = app.comment_type.clone();

    app.cycle_comment_type();

    assert_eq!(app.comment_type, before);
}

#[test]
fn should_show_a_posted_comment_in_the_conversation() {
    let mut app = pr_app();
    let comment = PullRequestIssueComment {
        author: Some("octocat".to_string()),
        body: "/retest".to_string(),
        url: None,
        created_at: None,
    };
    land_post(&mut app, Ok(comment.clone()));

    assert!(app.poll_pr_comment_events());

    let info = app.pr_info.as_ref().unwrap();
    assert_eq!(info.issue_comments.last(), Some(&comment));
    assert!(app.pr_comment_rx.is_none());
}

#[test]
fn should_reopen_with_the_text_of_a_failed_post() {
    let mut app = pr_app();
    land_post(&mut app, Err(("boom".to_string(), "/retest".to_string())));

    app.poll_pr_comment_events();
    assert!(message(&app).contains("boom"));

    app.enter_pr_comment_mode();
    assert_eq!(app.comment_buffer, "/retest");
    assert_eq!(app.comment_cursor, "/retest".len());
    assert!(app.pr_comment_unsent.is_none());
}
