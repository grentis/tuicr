use std::path::PathBuf;

use crate::app::tests::render_perf_tests::{app_with, file};
use crate::app::*;

/// `a.rs`, `b.rs` and `c.rs` in the diff, `b.rs` marked reviewed.
fn app_with_a_reviewed_file(open_reviewed_from_tree: bool) -> App {
    let mut app = app_with(vec![file("a.rs", 3), file("b.rs", 3), file("c.rs", 3)]);
    app.open_reviewed_from_tree = open_reviewed_from_tree;
    app.session
        .get_file_mut(&PathBuf::from("b.rs"))
        .unwrap()
        .reviewed = true;
    app.rebuild_annotations();
    app
}

fn shows_lines_of(app: &App, file_idx: usize) -> bool {
    app.line_annotations.iter().any(|annotation| {
        matches!(annotation, AnnotatedLine::DiffLine { file_idx: idx, .. } if *idx == file_idx)
    })
}

/// The file of the row under the cursor, from its hunk or diff line.
fn cursor_file(app: &App) -> Option<usize> {
    match app.line_annotations.get(app.diff_state.cursor_line)? {
        AnnotatedLine::FileHeader { file_idx }
        | AnnotatedLine::HunkHeader { file_idx, .. }
        | AnnotatedLine::DiffLine { file_idx, .. } => Some(*file_idx),
        _ => None,
    }
}

#[test]
fn should_show_a_reviewed_file_opened_from_the_tree_without_unmarking_it() {
    let mut app = app_with_a_reviewed_file(true);
    assert!(!shows_lines_of(&app, 1));

    app.open_file_from_tree(1);

    assert!(!app.should_collapse_file(1));
    assert!(shows_lines_of(&app, 1));
    assert!(app.session.is_file_reviewed(&PathBuf::from("b.rs")));
    assert_eq!(app.diff_state.current_file_idx, 1);
    assert_eq!(cursor_file(&app), Some(1));
}

#[test]
fn should_fold_a_reviewed_file_again_when_another_file_is_opened_from_the_tree() {
    let mut app = app_with_a_reviewed_file(true);
    app.open_file_from_tree(1);

    app.open_file_from_tree(0);

    assert!(app.should_collapse_file(1));
    assert!(!shows_lines_of(&app, 1));
    assert_eq!(app.diff_state.current_file_idx, 0);
    assert_eq!(cursor_file(&app), Some(0));
}

#[test]
fn should_fold_a_reviewed_file_when_it_is_opened_again() {
    let mut app = app_with_a_reviewed_file(true);
    app.open_file_from_tree(1);

    app.open_file_from_tree(1);

    assert!(app.should_collapse_file(1));
    assert!(!shows_lines_of(&app, 1));
    assert!(app.session.is_file_reviewed(&PathBuf::from("b.rs")));
    assert_eq!(app.diff_state.current_file_idx, 1);
}

#[test]
fn should_keep_a_reviewed_file_collapsed_when_the_option_is_off() {
    let mut app = app_with_a_reviewed_file(false);

    app.open_file_from_tree(1);

    assert!(app.should_collapse_file(1));
    assert!(!shows_lines_of(&app, 1));
    assert_eq!(app.diff_state.current_file_idx, 1);
}

#[test]
fn should_fold_a_reviewed_file_when_the_cursor_moves_to_the_next_file() {
    let mut app = app_with_a_reviewed_file(true);
    app.open_file_from_tree(1);

    for _ in 0..50 {
        if app.diff_state.current_file_idx == 2 {
            break;
        }
        app.cursor_down(1);
    }

    assert_eq!(app.diff_state.current_file_idx, 2);
    assert!(app.should_collapse_file(1));
    assert!(!shows_lines_of(&app, 1));
    // The rows above went away; the cursor stays on c.rs.
    assert_eq!(cursor_file(&app), Some(2));
}

#[test]
fn should_fold_a_reviewed_file_when_the_tree_moves_to_another_file() {
    let mut app = app_with_a_reviewed_file(true);
    app.open_file_from_tree(1);

    app.file_list_up(1);

    assert!(app.should_collapse_file(1));
    assert!(app.session.is_file_reviewed(&PathBuf::from("b.rs")));
}
