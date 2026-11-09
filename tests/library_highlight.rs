//! The coloring a program using tuicr as a library reaches: a hunk read
//! uncolored comes back with spans, and a container grammar is left alone.

use std::path::Path;

use tuicr::model::{DiffHunk, DiffLine, LineOrigin};
use tuicr::syntax::SyntaxHighlighter;

fn line(origin: LineOrigin, content: &str) -> DiffLine {
    DiffLine {
        origin,
        content: content.into(),
        old_lineno: None,
        new_lineno: None,
        highlighted_spans: None,
    }
}

fn hunk() -> DiffHunk {
    DiffHunk {
        header: "@@ -1,2 +1,2 @@".into(),
        lines: vec![
            line(LineOrigin::Context, "fn main() {"),
            line(LineOrigin::Deletion, "    let x = 1;"),
            line(LineOrigin::Addition, "    let x = 2;"),
        ],
        old_start: 1,
        old_count: 2,
        new_start: 1,
        new_count: 2,
    }
}

#[test]
fn should_color_every_line_of_a_hunk_from_outside_the_crate() {
    let mut h = hunk();

    SyntaxHighlighter::default().rehighlight_hunk_in_place(&mut h, Path::new("main.rs"));

    for l in &h.lines {
        let spans = l.highlighted_spans.as_ref().expect("a colored line");
        let text: String = spans.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(text, l.content);
        assert!(spans.iter().any(|(style, _)| style.fg.is_some()));
    }
}

#[test]
fn should_leave_a_container_grammar_to_full_file_coloring() {
    let mut h = hunk();

    SyntaxHighlighter::default().rehighlight_hunk_in_place(&mut h, Path::new("App.vue"));

    assert!(h.lines.iter().all(|l| l.highlighted_spans.is_none()));
}
