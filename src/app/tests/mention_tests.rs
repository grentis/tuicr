use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::pr_info_tests::build_pr_app;
use crate::app::{App, DiffSource, InputMode};
use crate::forge::traits::MentionCandidate;
use crate::handler::{handle_comment_action, handle_mention_key};
use crate::input::Action;

/// A PR app with `[forge] extended_comments` on and the comment box open. A
/// lookup is marked running so typing never calls `gh`; results are seeded by
/// hand.
fn commenting_app() -> App {
    let mut app = build_pr_app();
    app.forge_config.extended_comments = true;
    app.mention_lookup.in_flight = Some("blocked".to_string());
    app.enter_review_comment_mode();
    app
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        handle_comment_action(app, Action::InsertChar(c));
    }
}

fn press(app: &mut App, code: KeyCode) -> bool {
    handle_mention_key(app, KeyEvent::new(code, KeyModifiers::NONE))
}

fn listed(app: &App) -> Vec<String> {
    app.mention
        .as_ref()
        .map(|state| state.candidates.iter().map(|c| c.login.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn should_list_participants_with_the_author_first_and_highlighted() {
    let mut app = commenting_app();

    type_text(&mut app, "@");

    assert_eq!(listed(&app), ["alice", "bob", "carol", "dave"]);
    assert_eq!(app.mention.as_ref().map(|state| state.selected), Some(0));
}

#[test]
fn should_leave_the_viewer_out() {
    let mut app = commenting_app();
    app.mention_lookup.viewer_login = Some("Alice".to_string());

    type_text(&mut app, "@");

    assert_eq!(listed(&app), ["bob", "carol", "dave"]);
}

#[test]
fn should_insert_the_author_on_enter() {
    let mut app = commenting_app();
    type_text(&mut app, "thanks @");

    assert!(press(&mut app, KeyCode::Enter));

    assert_eq!(app.comment_buffer, "thanks @alice ");
    assert_eq!(app.comment_cursor, app.comment_buffer.len());
    assert_eq!(app.input_mode, InputMode::Comment);
    assert!(app.mention.is_none());
}

#[test]
fn should_insert_the_highlighted_login_on_tab() {
    let mut app = commenting_app();
    type_text(&mut app, "@");

    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Tab);

    assert_eq!(app.comment_buffer, "@carol ");
}

#[test]
fn should_add_forge_matches_after_participants_as_the_name_is_typed() {
    let mut app = commenting_app();
    app.mention_lookup.results.insert(
        "ca".to_string(),
        vec![
            MentionCandidate {
                login: "carol".to_string(),
                name: None,
            },
            MentionCandidate {
                login: "cameron".to_string(),
                name: Some("Cam Eron".to_string()),
            },
        ],
    );

    type_text(&mut app, "@ca");
    assert_eq!(listed(&app), ["carol", "cameron"]);

    // `cam` has not been looked up; `ca`'s results still narrow down.
    type_text(&mut app, "m");
    assert_eq!(listed(&app), ["cameron"]);
}

#[test]
fn should_keep_the_list_closed_after_esc_until_a_new_mention() {
    let mut app = commenting_app();
    type_text(&mut app, "@");

    assert!(press(&mut app, KeyCode::Esc));
    assert!(app.mention.is_none());
    assert_eq!(app.input_mode, InputMode::Comment);

    type_text(&mut app, "b");
    assert!(app.mention.is_none());

    type_text(&mut app, " @");
    assert_eq!(listed(&app), ["alice", "bob", "carol", "dave"]);
}

#[test]
fn should_leave_keys_to_the_editor_when_no_list_is_open() {
    let mut app = commenting_app();
    type_text(&mut app, "no mention");

    assert!(!press(&mut app, KeyCode::Enter));
}

#[test]
fn should_not_suggest_in_a_local_review() {
    let mut app = commenting_app();
    app.diff_source = DiffSource::WorkingTree;

    type_text(&mut app, "@");

    assert!(app.mention.is_none());
}

#[test]
fn should_not_suggest_with_extended_comments_off() {
    let mut app = commenting_app();
    app.forge_config.extended_comments = false;

    type_text(&mut app, "@");

    assert!(app.mention.is_none());
}

#[test]
fn should_not_open_on_an_email_address() {
    let mut app = commenting_app();

    type_text(&mut app, "mail bob@");

    assert!(app.mention.is_none());
}

#[test]
fn should_insert_into_the_vim_editor_too() {
    let mut app = commenting_app();
    app.comment_vim_enabled = true;
    app.ensure_comment_vim_editor();
    app.comment_vim_feed_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE));

    assert!(press(&mut app, KeyCode::Enter));
    app.comment_vim_feed_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));

    assert_eq!(app.comment_buffer, "@alice !");
}
