//! The commit pop-up: title line, wrapped description, staged files and key hints.

use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, CommitField, InputMode};
use crate::ui::styles;

/// Rows of description text shown before it scrolls.
const DESCRIPTION_ROWS: u16 = 8;

pub fn render_commit_dialog(frame: &mut Frame, app: &App) {
    if app.input_mode != InputMode::CommitMessage {
        return;
    }
    let Some(draft) = app.dialog_draft() else {
        return;
    };
    let rename_target = app.dialog_rename_target();
    let screen = frame.area();
    let width = screen.width.saturating_sub(4).min(80);
    // Title box, description box, staged line, keys line, outer border.
    let height = (3 + DESCRIPTION_ROWS + 2 + 2 + 2).min(screen.height);
    if width < 20 || height < 10 {
        return;
    }
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let heading = match rename_target {
        Some(id) => format!(" Rename {} ", id.chars().take(7).collect::<String>()),
        None => " Commit ".to_string(),
    };
    let outer = Block::default()
        .title(heading)
        .borders(Borders::ALL)
        .style(styles::panel_style(&app.theme))
        .border_style(styles::border_style(&app.theme, true));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let title_area = Rect::new(inner.x, inner.y, inner.width, 3);
    let description_area = Rect::new(inner.x, inner.y + 3, inner.width, DESCRIPTION_ROWS + 2);
    let staged_area = Rect::new(inner.x, description_area.bottom(), inner.width, 1);
    let keys_area = Rect::new(inner.x, staged_area.bottom(), inner.width, 1);

    // Title: one line, scrolled so the cursor stays in view.
    let title_focused = draft.field == CommitField::Title;
    let title_block = field_block(app, " Title ", title_focused);
    let title_inner = title_block.inner(title_area);
    let (title_text, title_col) =
        scroll_line(&draft.title, draft.title_cursor, title_inner.width as usize);
    frame.render_widget(Paragraph::new(title_text).block(title_block), title_area);

    // Description: wrapped, scrolled so the cursor's row stays in view.
    let description_block = field_block(app, " Description ", !title_focused);
    let description_inner = description_block.inner(description_area);
    let (rows, (cursor_row, cursor_col)) = wrap_with_cursor(
        &draft.description,
        draft.description_cursor,
        description_inner.width as usize,
    );
    let visible = description_inner.height as usize;
    let first = cursor_row.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = rows
        .into_iter()
        .skip(first)
        .take(visible)
        .map(Line::from)
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(description_block),
        description_area,
    );

    let info_line = match rename_target {
        Some(_) => "Only the message changes \u{00b7} later commits get new ids".to_string(),
        None => {
            let staged = app.staged_paths();
            let names: Vec<String> = staged
                .iter()
                .map(|path| {
                    path.file_name().map_or_else(
                        || path.display().to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    )
                })
                .collect();
            format!("Staged ({}): {}", staged.len(), names.join(", "))
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            info_line,
            styles::dim_style(&app.theme),
        ))),
        staged_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Enter (in title) / Ctrl-S save \u{00b7} Tab switch \u{00b7} Esc close",
            styles::dim_style(&app.theme),
        ))),
        keys_area,
    );

    let cursor = if title_focused {
        Position::new(title_inner.x + title_col as u16, title_inner.y)
    } else {
        Position::new(
            description_inner.x + cursor_col as u16,
            description_inner.y + (cursor_row - first) as u16,
        )
    };
    frame.set_cursor_position(cursor);
}

fn field_block<'a>(app: &App, title: &'a str, focused: bool) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(styles::border_style(&app.theme, focused))
        .style(Style::default())
}

/// The slice of single-line `text` that fits `width` columns with the cursor in view, and the
/// cursor's column in it.
pub(crate) fn scroll_line(text: &str, cursor: usize, width: usize) -> (String, usize) {
    let cursor = cursor.min(text.len());
    let before = &text[..cursor];
    let before_width: usize = before.chars().map(|c| c.width().unwrap_or(0)).sum();
    if width == 0 || before_width < width {
        return (text.to_string(), before_width);
    }
    // Drop leading characters until the cursor fits in the last column.
    let mut skipped = 0;
    let mut start = 0;
    for (idx, c) in before.char_indices() {
        if before_width - skipped < width {
            break;
        }
        skipped += c.width().unwrap_or(0);
        start = idx + c.len_utf8();
    }
    (text[start..].to_string(), before_width - skipped)
}

/// Wrap `text` to `width` columns at line breaks and overflow; return the cursor's (row, column).
pub(crate) fn wrap_with_cursor(
    text: &str,
    cursor: usize,
    width: usize,
) -> (Vec<String>, (usize, usize)) {
    let width = width.max(1);
    let cursor = cursor.min(text.len());
    let mut rows = vec![String::new()];
    let mut row_width = 0;
    let mut cursor_at = (0, 0);
    for (idx, c) in text.char_indices() {
        if idx == cursor {
            cursor_at = (rows.len() - 1, row_width);
        }
        if c == '\n' {
            rows.push(String::new());
            row_width = 0;
            continue;
        }
        let w = c.width().unwrap_or(0);
        if row_width + w > width {
            rows.push(String::new());
            row_width = 0;
            if idx == cursor {
                cursor_at = (rows.len() - 1, 0);
            }
        }
        rows.last_mut().expect("at least one row").push(c);
        row_width += w;
    }
    if cursor == text.len() {
        cursor_at = if row_width >= width {
            rows.push(String::new());
            (rows.len() - 1, 0)
        } else {
            (rows.len() - 1, row_width)
        };
    }
    (rows, cursor_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_wrap_at_breaks_and_width_and_place_the_cursor() {
        let (rows, cursor) = wrap_with_cursor("abcdef\ngh", 4, 4);
        assert_eq!(rows, ["abcd", "ef", "gh"]);
        assert_eq!(cursor, (1, 0));

        let (_, cursor) = wrap_with_cursor("abcdef\ngh", 9, 4);
        assert_eq!(cursor, (2, 2));

        let (rows, cursor) = wrap_with_cursor("", 0, 4);
        assert_eq!(rows, [""]);
        assert_eq!(cursor, (0, 0));
    }

    #[test]
    fn should_scroll_a_long_title_to_keep_the_cursor_in_view() {
        assert_eq!(scroll_line("short", 5, 10), ("short".to_string(), 5));
        let (shown, col) = scroll_line("0123456789", 10, 4);
        assert_eq!(col, 3);
        assert!(shown.starts_with("789"), "{shown}");
    }
}
