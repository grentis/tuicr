use std::path::PathBuf;

use crate::app::tests::render_perf_tests::{app_with, file};
use crate::app::*;
use crate::model::review::FileReview;
use crate::model::{Comment, CommentType, FileStatus};

/// `kept.rs` is in the diff; `gone.rs` has a file comment and a line comment
/// but left the diff, as a renamed untracked file does.
fn app_with_a_gone_file() -> App {
    let mut app = app_with(vec![file("kept.rs", 3)]);
    let mut gone = FileReview::new(PathBuf::from("gone.rs"), FileStatus::Added, 0);
    gone.add_file_comment(Comment::new(
        "file note".to_string(),
        CommentType::default(),
        None,
    ));
    gone.add_line_comment(
        2,
        Comment::new(
            "rename this".to_string(),
            CommentType::default(),
            Some(LineSide::New),
        ),
    );
    app.session.files.insert(PathBuf::from("gone.rs"), gone);
    app.rebuild_annotations();
    app
}

fn rendered_text(app: &mut App) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, app))
        .expect("draw frame");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn first_detached_row(app: &App, comment_idx: usize) -> usize {
    app.line_annotations
        .iter()
        .position(|annotation| {
            matches!(annotation, AnnotatedLine::DetachedComment { comment_idx: idx } if *idx == comment_idx)
        })
        .expect("detached comment row")
}

#[test]
fn should_show_comments_on_a_file_no_longer_in_the_diff() {
    for mode in [DiffViewMode::Unified, DiffViewMode::SideBySide] {
        let mut app = app_with_a_gone_file();
        app.diff_view_mode = mode;
        app.rebuild_annotations();

        let text = rendered_text(&mut app);

        assert!(
            text.contains("gone.rs · not in the diff"),
            "{mode:?}\n{text}"
        );
        assert!(text.contains("file note"), "{mode:?}\n{text}");
        assert!(text.contains("rename this"), "{mode:?}\n{text}");
        // The section's height matches its rows, so the files below line up.
        let first_file = app
            .line_annotations
            .iter()
            .position(|annotation| matches!(annotation, AnnotatedLine::FileHeader { .. }))
            .unwrap();
        assert_eq!(app.review_comments_render_height(), first_file, "{mode:?}");
    }
}

#[test]
fn should_list_comments_on_a_file_no_longer_in_the_diff_in_the_navigator() {
    let mut app = app_with_a_gone_file();

    let items = app.build_comment_navigator_items();

    let locations: Vec<_> = items
        .iter()
        .map(|item| (item.path.clone(), item.line))
        .collect();
    assert_eq!(
        locations,
        vec![
            (Some("gone.rs".to_string()), None),
            (Some("gone.rs".to_string()), Some(2)),
        ]
    );
    app.comment_navigator_state.select(1);
    assert!(app.jump_to_selected_comment());
    assert_eq!(app.diff_state.cursor_line, first_detached_row(&app, 1));
}

#[test]
fn should_delete_a_comment_on_a_file_no_longer_in_the_diff() {
    let mut app = app_with_a_gone_file();
    app.diff_state.cursor_line = first_detached_row(&app, 1);

    assert_eq!(
        app.comment_content_at_cursor().as_deref(),
        Some("rename this")
    );
    assert!(app.delete_comment_at_cursor());

    let gone = &app.session.files[&PathBuf::from("gone.rs")];
    assert!(gone.line_comments.values().all(Vec::is_empty));
    assert_eq!(gone.file_comments.len(), 1);
}

#[test]
fn should_not_list_a_file_that_is_in_the_diff() {
    let mut app = app_with_a_gone_file();
    app.session.files.remove(&PathBuf::from("gone.rs"));
    app.session
        .files
        .get_mut(&PathBuf::from("kept.rs"))
        .unwrap()
        .add_file_comment(Comment::new(
            "here".to_string(),
            CommentType::default(),
            None,
        ));
    app.rebuild_annotations();

    assert!(app.detached_comments().is_empty());
    assert!(!app.show_review_comments_header());
}
