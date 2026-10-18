use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::forge::traits::ForgeKind;
use crate::model::LineRange;
use crate::theme::Theme;
use crate::ui::styles;

/// Content prefix used on every comment-body line. 4 pad chars + `│` + 2 spaces.
/// The 4-char left pad keeps the box clear of the col-6 `▌` add/del prefix.
const BORDER_PREFIX: &str = "    │  ";
const BORDER_PREFIX_WIDTH: usize = 7;

/// Split `text` into segments whose display width each fits within `content_area`.
/// Returns a single-element vec when the text already fits. The returned slices
/// borrow from `text`, so the caller must keep `text` alive while iterating.
pub(crate) fn wrap_segments(text: &str, content_area: usize) -> Vec<&str> {
    if content_area == 0 || text.width() <= content_area {
        return vec![text];
    }
    let mut segments = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        let mut take_bytes = 0usize;
        let mut taken_width = 0usize;
        for c in remaining.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if taken_width + cw > content_area {
                break;
            }
            taken_width += cw;
            take_bytes += c.len_utf8();
        }
        // Single character wider than content_area — emit it anyway so we don't loop forever
        if take_bytes == 0 {
            take_bytes = remaining.chars().next().map_or(0, |c| c.len_utf8());
        }
        let (seg, rest) = remaining.split_at(take_bytes);
        segments.push(seg);
        remaining = rest;
    }
    segments
}

fn comment_content_width(width: usize) -> usize {
    // Reserve the prefix, a cell to prevent ratatui wrapping exact-fit rows,
    // and a cell for the cursor at the end of a segment.
    width.saturating_sub(BORDER_PREFIX_WIDTH + 2)
}

/// Move one displayed row using the same box width and wrapping as
/// `format_comment_input_lines`. Retain the preferred screen column when
/// the destination row is shorter.
pub(crate) fn move_comment_cursor(
    buffer: &str,
    cursor: usize,
    width: usize,
    down: bool,
    preferred_column: &mut Option<usize>,
) -> usize {
    let mut rows = Vec::new();
    let mut start = 0;
    for line in buffer.split('\n') {
        for segment in wrap_segments(line, comment_content_width(width)) {
            rows.push(start..start + segment.len());
            start += segment.len();
        }
        start += 1;
    }
    let current = rows
        .iter()
        .rposition(|row| row.start <= cursor)
        .unwrap_or(0);
    let target = if down {
        current.checked_add(1)
    } else {
        current.checked_sub(1)
    };
    let Some(target) = target.filter(|&index| index < rows.len()) else {
        return cursor;
    };
    let row = &rows[target];
    let column =
        *preferred_column.get_or_insert_with(|| buffer[rows[current].start..cursor].width());
    let text = &buffer[row.clone()];
    // At a soft-wrap boundary, the cursor belongs to the following row.
    // A cursor can sit after a row's text only at a newline or the buffer's end.
    let end_stop = (!rows
        .get(target + 1)
        .is_some_and(|next| next.start == row.end))
    .then_some(text.len());
    // Presentation selectors can shrink prefix widths; choose the last fitting boundary.
    let byte = text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain(end_stop)
        .rfind(|&byte| text[..byte].width() <= column)
        .unwrap_or(0);
    row.start + byte
}

/// Emit spans covering `line_text[start..end)` using the per-line markdown
/// highlight `runs` (concatenation of run text equals `line_text`). Falls back
/// to a single unstyled span when highlighting is unavailable. Offsets are byte
/// positions; callers pass char-boundary offsets (segment/cursor boundaries).
fn highlighted_window_spans(
    runs: Option<&[(Style, String)]>,
    line_text: &str,
    start: usize,
    end: usize,
) -> Vec<Span<'static>> {
    if start >= end {
        return Vec::new();
    }
    let Some(runs) = runs else {
        return vec![Span::raw(line_text[start..end].to_string())];
    };
    let mut out = Vec::new();
    let mut run_start = 0usize;
    for (style, text) in runs {
        let run_end = run_start + text.len();
        let lo = start.max(run_start);
        let hi = end.min(run_end);
        if lo < hi {
            out.push(Span::styled(
                text[lo - run_start..hi - run_start].to_string(),
                *style,
            ));
        }
        run_start = run_end;
        if run_start >= end {
            break;
        }
    }
    out
}

/// Information about where the cursor should be positioned within comment input
#[derive(Debug, Clone)]
pub struct CommentCursorInfo {
    /// Which line within the formatted output contains the cursor (0-indexed, relative to content start)
    /// This is the line index within the Vec<Line> returned by format_comment_input_lines,
    /// where 0 = header line, 1+ = content lines, last = footer line.
    /// The cursor is only on content lines (1 to n-2 inclusive for n total lines).
    pub line_offset: usize,
    /// Column offset (display width) from start of line where cursor should be
    pub column: u16,
}

#[derive(Debug, Clone)]
pub struct CommentTypePresentation {
    pub label: String,
    pub color: Color,
}

/// Format a comment input as multiple lines with a box border for inline editing.
/// This mimics the normal comment display but shows it's being edited.
///
/// Returns a tuple of (lines, cursor_info) where cursor_info contains the position
/// of the cursor within the formatted output for IME positioning.
/// The frame of a local comment box: the colour forge threads are drawn
/// in, for every author. A comment's type keeps its colour on its badge.
fn local_box_style(theme: &Theme) -> Style {
    Style::default().fg(theme.diff_hunk_header)
}

#[allow(clippy::too_many_arguments)]
pub fn format_comment_input_lines(
    theme: &Theme,
    comment_type: CommentTypePresentation,
    buffer: &str,
    cursor_pos: usize,
    line_range: Option<LineRange>,
    action: &str,
    threads: bool,
    width: usize,
    vim_mode: Option<(&str, bool)>,
    supports_keyboard_enhancement: bool,
) -> (Vec<Line<'static>>, CommentCursorInfo) {
    let type_style = styles::comment_type_style(theme, comment_type.color);
    let border_style = if threads {
        local_box_style(theme)
    } else {
        styles::comment_border_style(theme, comment_type.color)
    };
    let cursor_style = Style::default()
        .fg(theme.cursor_color)
        .add_modifier(Modifier::UNDERLINED);

    let line_info = match line_range {
        Some(range) if range.is_single() => format!("L{} ", range.start),
        Some(range) => format!("L{}-L{} ", range.start, range.end),
        None => String::new(),
    };

    let newline_hint = if supports_keyboard_enhancement {
        "Shift-Enter"
    } else {
        "Alt-Enter"
    };

    let content_area = comment_content_width(width);

    let mut result = Vec::new();
    let mut cursor_line_offset: usize = 1;
    let mut cursor_column: u16 = BORDER_PREFIX_WIDTH as u16;

    // Top-left corner becomes `├` when a line range is present — the bar
    // painter then draws `│` going up through the range and a `╭` at the
    // topmost covered line, so the tee reads as the bar joining the box.
    // Threads draw no bar.
    let top_corner = if line_range.is_some() && !threads {
        '├'
    } else {
        '╭'
    };
    let top_prefix = format!("    {top_corner}── ");

    // Top border with type label and hints. In vim mode the hints describe the
    // modal bindings and a `[MODE]` tag is shown after the type label.
    let hint = match vim_mode {
        Some(_) => "(i:insert  Alt-Enter:save  Esc:normal  :w save  :q discard)".to_string(),
        None => format!("(Tab/S-Tab:type Enter:save {newline_hint}:newline Esc:cancel)"),
    };
    let mut header_spans = vec![
        Span::styled(top_prefix, border_style),
        Span::styled(format!("{action} "), styles::dim_style(theme)),
    ];
    // `None` has an empty label — show no `[TYPE]` badge while composing.
    if !comment_type.label.is_empty() {
        header_spans.push(Span::styled(
            format!("[{}] ", comment_type.label),
            type_style,
        ));
    }
    if let Some((mode, warn)) = vim_mode {
        // The cancel-confirm hint is painted red to flag the destructive action.
        let mode_style = if warn {
            Style::default()
                .fg(theme.comment_issue)
                .add_modifier(Modifier::BOLD)
        } else {
            type_style
        };
        header_spans.push(Span::styled(format!("[{mode}] "), mode_style));
    }
    header_spans.push(Span::styled(line_info, styles::dim_style(theme)));
    header_spans.push(Span::styled(hint, styles::dim_style(theme)));
    result.push(Line::from(header_spans));

    // Content lines with cursor
    if buffer.is_empty() {
        // Show placeholder with cursor at start
        result.push(Line::from(vec![
            Span::styled(BORDER_PREFIX, border_style),
            Span::styled(" ", cursor_style),
            Span::styled("Type your comment...", styles::dim_style(theme)),
        ]));
        // cursor_line_offset is already 1 (first content line)
        // cursor_column is already BORDER_PREFIX_WIDTH (cursor at start of content)
    } else {
        let buffer_lines: Vec<&str> = buffer.split('\n').collect();
        // Markdown-highlight the in-progress text; colors come from the active
        // syntect theme (same engine/theme as diff code highlighting).
        let highlighted = theme.syntax_highlighter().highlight_markdown_body(buffer);
        let mut byte_offset = 0;
        // Tracks how many visual lines have been pushed so far (not counting the header).
        let mut total_visual_lines: usize = 0;

        for (line_idx, text) in buffer_lines.iter().enumerate() {
            let line_start = byte_offset;
            let line_end = byte_offset + text.len();
            let is_last_logical = line_idx + 1 == buffer_lines.len();

            // Check if cursor is on this line
            let cursor_on_this_line = cursor_pos >= line_start
                && (cursor_pos <= line_end || (is_last_logical && cursor_pos == buffer.len()));

            // Pre-wrap this logical line into segments so ratatui never wraps it.
            // Short lines come back as a single-element vec.
            let segments = wrap_segments(text, content_area);
            let mut seg_byte_start = 0usize;

            for (seg_idx, seg) in segments.iter().enumerate() {
                let seg_start = line_start + seg_byte_start;
                let seg_end = seg_start + seg.len();
                let is_last_seg = seg_idx + 1 == segments.len();
                let cursor_in_seg = cursor_on_this_line
                    && cursor_pos >= seg_start
                    && (cursor_pos < seg_end || is_last_seg);

                let mut line_spans = vec![Span::styled(BORDER_PREFIX, border_style)];
                let line_runs = highlighted.get(line_idx).and_then(|o| o.as_deref());
                let seg_end_in_line = seg_byte_start + seg.len();

                if cursor_in_seg {
                    let cursor_pos_in_seg = (cursor_pos - seg_start).min(seg.len());
                    let (before, after) = seg.split_at(cursor_pos_in_seg);
                    let cursor_in_line = seg_byte_start + cursor_pos_in_seg;

                    // Track cursor position for IME
                    cursor_line_offset = 1 + total_visual_lines;
                    cursor_column = BORDER_PREFIX_WIDTH as u16 + before.width() as u16;

                    // before-cursor text, highlighted
                    line_spans.extend(highlighted_window_spans(
                        line_runs,
                        text,
                        seg_byte_start,
                        cursor_in_line,
                    ));
                    // Cursor char gets the cursor style; the rest stays highlighted.
                    // No trailing cell at end-of-segment — the terminal cursor
                    // (set_cursor_position) handles that position.
                    if let Some(cursor_char) = after.chars().next() {
                        line_spans.push(Span::styled(cursor_char.to_string(), cursor_style));
                        line_spans.extend(highlighted_window_spans(
                            line_runs,
                            text,
                            cursor_in_line + cursor_char.len_utf8(),
                            seg_end_in_line,
                        ));
                    }
                } else {
                    line_spans.extend(highlighted_window_spans(
                        line_runs,
                        text,
                        seg_byte_start,
                        seg_end_in_line,
                    ));
                }

                result.push(Line::from(line_spans));
                total_visual_lines += 1;
                seg_byte_start += seg.len();
            }

            // Account for newline character (except for last line)
            byte_offset = line_end + 1;
        }
    }

    // Bottom border — "    ╰" = 5 chars, fill to width
    result.push(Line::from(vec![Span::styled(
        "    ╰".to_string() + &"─".repeat(width.saturating_sub(5)),
        border_style,
    )]));

    let cursor_info = CommentCursorInfo {
        line_offset: cursor_line_offset,
        column: cursor_column,
    };

    (result, cursor_info)
}

/// Format an entire remote (read-only) forge review thread as one fused
/// box so it reads as a single discussion unit. Root comment opens the
/// box; replies appear as `├─ ↳ @author ──` separator headers within the
/// same box; the bottom rule appears once at the end.
///
/// Visually distinct from local drafts: the `[forge @author]` badge on
/// the root header, and a muted palette throughout for resolved/outdated
/// threads.
pub fn format_remote_thread_lines(
    theme: &Theme,
    thread: &crate::forge::remote_comments::RemoteReviewThread,
    muted: bool,
    forge_kind: Option<ForgeKind>,
    threads: bool,
) -> Vec<Line<'static>> {
    // Anchored on a line, the box opens with a tee the bar joins; threads
    // draw no bar.
    let top_prefix = if threads {
        "    ╭── "
    } else {
        "    ├── "
    };
    let (badge_fg, border_fg, body_fg) = if muted {
        (theme.fg_dim, theme.fg_dim, theme.fg_dim)
    } else {
        (
            theme.diff_hunk_header,
            theme.diff_hunk_header,
            theme.fg_secondary,
        )
    };

    let badge_style = Style::default().fg(badge_fg).add_modifier(Modifier::BOLD);
    let reply_badge_style = Style::default().fg(badge_fg);
    let border_style = Style::default().fg(border_fg);
    let body_style = Style::default().fg(body_fg);

    let line_info = match thread.line.map(LineRange::single) {
        Some(range) if range.is_single() => format!("L{} ", range.start),
        Some(range) => format!("L{}-L{} ", range.start, range.end),
        None => String::new(),
    };

    let mut result = Vec::new();
    let mut iter = thread.comments.iter().peekable();
    let mut is_first = true;
    while let Some(comment) = iter.next() {
        let author = comment.author.as_deref().unwrap_or("unknown");
        if is_first {
            let mut badge_text = format!("[{} @{author}", forge_badge_label(forge_kind));
            if thread.is_resolved {
                badge_text.push_str(" resolved");
            } else if thread.is_outdated {
                badge_text.push_str(" outdated");
            }
            badge_text.push_str("] ");
            result.push(Line::from(vec![
                Span::styled(top_prefix.to_string(), border_style),
                Span::styled(badge_text, badge_style),
                Span::styled(line_info.clone(), styles::dim_style(theme)),
                Span::styled("─".repeat(20), border_style),
            ]));
        } else {
            result.push(Line::from(vec![
                Span::styled("    ├── ".to_string(), border_style),
                Span::styled(format!("↳ @{author} "), reply_badge_style),
                Span::styled("─".repeat(28), border_style),
            ]));
        }

        for line in comment.body.split('\n') {
            result.push(Line::from(vec![
                Span::styled("    │  ".to_string(), border_style),
                Span::styled(line.to_string(), body_style),
            ]));
        }

        is_first = false;
        let _ = iter.peek();
    }

    result.push(Line::from(vec![Span::styled(
        "    ╰".to_string() + &"─".repeat(39),
        border_style,
    )]));

    result
}

/// Format a remote review summary (the body of a `PullRequestReview`) as a
/// box with a `[forge @author <state>]` header. Renders at review scope —
/// no line anchor — so the top corner is `╭`, not the line-anchored `├`.
pub fn format_remote_review_summary_lines(
    theme: &Theme,
    summary: &crate::forge::remote_comments::RemoteReviewSummary,
    forge_kind: Option<ForgeKind>,
) -> Vec<Line<'static>> {
    let badge_fg = theme.diff_hunk_header;
    let border_fg = theme.diff_hunk_header;
    let body_fg = theme.fg_secondary;

    let badge_style = Style::default().fg(badge_fg).add_modifier(Modifier::BOLD);
    let border_style = Style::default().fg(border_fg);
    let body_style = Style::default().fg(body_fg);

    let author = summary.author.as_deref().unwrap_or("unknown");
    let mut badge_text = format!("[{} @{author}", forge_badge_label(forge_kind));
    if let Some(state_label) = summary.state.badge_label() {
        badge_text.push(' ');
        badge_text.push_str(state_label);
    }
    badge_text.push_str("] ");

    let mut result = Vec::new();
    result.push(Line::from(vec![
        Span::styled("    ╭── ".to_string(), border_style),
        Span::styled(badge_text, badge_style),
        Span::styled("─".repeat(28), border_style),
    ]));

    for line in summary.body.split('\n') {
        result.push(Line::from(vec![
            Span::styled("    │  ".to_string(), border_style),
            Span::styled(line.to_string(), body_style),
        ]));
    }

    result.push(Line::from(vec![Span::styled(
        "    ╰".to_string() + &"─".repeat(39),
        border_style,
    )]));

    result
}

fn forge_badge_label(kind: Option<ForgeKind>) -> &'static str {
    match kind {
        Some(ForgeKind::GitHub) => "github",
        Some(ForgeKind::GitLab) => "gitlab",
        Some(ForgeKind::Gitea) => "gitea",
        Some(ForgeKind::Bitbucket) => "bitbucket",
        Some(ForgeKind::AzureDevOps) => "azure",
        Some(ForgeKind::Gerrit) => "gerrit",
        None => "forge",
    }
}

/// Render `content` as markdown-highlighted, pre-wrapped lines. Colors come
/// from the active syntect theme.
pub(crate) fn markdown_body_lines(
    theme: &Theme,
    content: &str,
    content_area: usize,
) -> Vec<Line<'static>> {
    let lines: Vec<&str> = content.split('\n').collect();
    // Highlight the body as a whole so multi-line constructs (e.g. fenced code)
    // carry state across lines.
    let highlighted = theme.syntax_highlighter().highlight_markdown_body(content);

    let mut out = Vec::new();
    for (idx, text) in lines.iter().enumerate() {
        let runs = highlighted.get(idx).and_then(|o| o.as_deref());
        let mut seg_start = 0usize;
        for seg in wrap_segments(text, content_area) {
            let seg_end = seg_start + seg.len();
            out.push(Line::from(highlighted_window_spans(
                runs, text, seg_start, seg_end,
            )));
            seg_start = seg_end;
        }
    }
    out
}

/// Format a comment as multiple lines with a box border (themed version).
///
/// `author` advertises the comment's author in the top-row badge and tints
/// the box border. Callers pass `Some(name)` for non-self comments — the
/// resulting badge reads `[TYPE @name]`, mirroring the remote forge badge
/// format used for remote PR threads. `None` keeps the existing neutral
/// `[TYPE]` badge and theme border.
/// Where a local comment sits in its thread, which decides its frame: a
/// reply opens with an `↳ @author` divider instead of a header, and a
/// comment a reply follows leaves the closing rule to the thread's last
/// message. A thread so reads as one box, as a forge thread does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadChrome {
    /// Draw local comments as threads (`comment_style = "threads"`); otherwise each
    /// comment is its own box, as upstream draws it.
    pub threads: bool,
    /// The reply's author, when the comment answers a thread.
    pub reply_author: Option<String>,
    /// A reply follows in the same thread.
    pub continues: bool,
}

impl ThreadChrome {
    pub fn of(comment: &crate::model::Comment, threads: bool) -> Self {
        if !threads {
            return Self::default();
        }
        Self {
            threads,
            reply_author: comment.reply_to.is_some().then(|| comment.author.clone()),
            continues: comment.continues_thread.0,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn format_comment_lines(
    theme: &Theme,
    comment_type: CommentTypePresentation,
    content: &str,
    line_range: Option<LineRange>,
    width: usize,
    author: Option<&str>,
    reply_label: Option<String>,
    thread: ThreadChrome,
) -> Vec<Line<'static>> {
    let type_style = styles::comment_type_style(theme, comment_type.color);
    // A reply takes its thread's frame: its author is on its divider.
    let author = author.filter(|_| thread.reply_author.is_none());
    // Threads: one colour for every box, whoever wrote it, as a forge thread
    // has. Otherwise the author's colour, else the type's.
    let border_style = match author {
        _ if thread.threads => local_box_style(theme),
        Some(name) => Style::default()
            .fg(styles::author_color_for(name))
            .add_modifier(Modifier::BOLD),
        None => styles::comment_border_style(theme, comment_type.color),
    };

    // `None` comments have an empty label: drop the `[TYPE]` badge, keeping the
    // author tag when present so per-author coloring still reads.
    // `[TYPE @author]` as a forge thread heads itself: bold, in the box's
    // colour, with only the type word in the type's colour.
    let badge_style = border_style.add_modifier(Modifier::BOLD);
    let label = &comment_type.label;
    let badge: Vec<Span<'static>> = match (author, label.is_empty()) {
        (Some(name), true) if !thread.threads => {
            vec![Span::styled(format!("[@{name}] "), type_style)]
        }
        (Some(name), false) if !thread.threads => {
            vec![Span::styled(format!("[{label} @{name}] "), type_style)]
        }
        (Some(name), true) => vec![Span::styled(format!("[@{name}] "), badge_style)],
        (Some(name), false) => vec![
            Span::styled("[".to_string(), badge_style),
            Span::styled(label.clone(), type_style),
            Span::styled(format!(" @{name}] "), badge_style),
        ],
        (None, true) => Vec::new(),
        (None, false) => vec![Span::styled(format!("[{label}] "), type_style)],
    };
    let badge_width: usize = badge.iter().map(|span| span.content.width()).sum();

    let line_info = match line_range {
        Some(range) if range.is_single() => format!("L{} ", range.start),
        Some(range) => format!("L{}-L{} ", range.start, range.end),
        None => String::new(),
    };
    // A reply names the thread it answers, ahead of its line.
    let line_info = format!("{}{line_info}", reply_label.unwrap_or_default());

    // "    │  " is the per-line content prefix; everything past that is content.
    // Subtract two extra: one so ratatui never wraps an exact-fit line, and
    // one so the terminal cursor at end-of-segment stays clear of the border.
    let content_area = width.saturating_sub(BORDER_PREFIX_WIDTH + 2);

    let mut result = Vec::new();

    let top_corner = if line_range.is_some() && !thread.threads {
        '├'
    } else {
        '╭'
    };
    let top_prefix = format!("    {top_corner}── ");

    if let Some(reply_author) = &thread.reply_author {
        // The divider a forge thread draws between its messages.
        let label = format!("\u{21b3} @{reply_author} ");
        let fill = width.saturating_sub(8 + label.width());
        result.push(Line::from(vec![
            Span::styled("    ├── ".to_string(), border_style),
            Span::styled(label, border_style),
            Span::styled("─".repeat(fill), border_style),
        ]));
    } else {
        // Top border — fill dynamically so total line = width.
        // top_prefix = 8 cols, then the badge (width depends on author), then
        // optional line_info, then `─` fill out to `width`.
        let top_fill = width.saturating_sub(8 + badge_width + line_info.width());
        let mut spans = vec![Span::styled(top_prefix, border_style)];
        spans.extend(badge);
        spans.push(Span::styled(line_info, styles::dim_style(theme)));
        spans.push(Span::styled("─".repeat(top_fill), border_style));
        result.push(Line::from(spans));
    }

    // Content lines — markdown-highlighted, pre-wrapped at content_area.
    let mut body_lines = markdown_body_lines(theme, content, content_area);
    for line in &mut body_lines {
        line.spans
            .insert(0, Span::styled(BORDER_PREFIX, border_style));
    }
    result.extend(body_lines);

    // Bottom border — "    ╰" = 5 chars, fill to width. A comment a reply
    // follows leaves it to the thread's last message.
    if !thread.continues {
        result.push(Line::from(vec![Span::styled(
            "    ╰".to_string() + &"─".repeat(width.saturating_sub(5)),
            border_style,
        )]));
    }

    result
}

pub fn render_confirm_dialog(frame: &mut Frame, app: &App, message: &str) {
    let theme = &app.theme;
    let area = centered_rect(50, 20, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Confirm ")
        .borders(Borders::ALL)
        .style(styles::popup_style(theme))
        .border_style(styles::border_style(theme, true));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(""),
        Line::from(Span::raw(message)),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [Y]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("es    "),
            Span::styled("[N]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("o"),
        ]),
    ];

    let paragraph = Paragraph::new(lines)
        .style(styles::popup_style(theme))
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(paragraph, inner);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([Constraint::Percentage(percent_y)]).flex(Flex::Center);
    let horizontal = Layout::horizontal([Constraint::Percentage(percent_x)]).flex(Flex::Center);
    let [area] = vertical.areas(area);
    let [area] = horizontal.areas(area);
    area
}

/// The `@` list under the comment cursor, aligned with the `@`, or above it.
pub fn render_mention_list(frame: &mut Frame, app: &App) {
    use crate::app::{InputMode, MENTION_LIST_MAX_ROWS};
    use crate::ui::commit_row::CURSOR_GLYPH;

    if app.input_mode != InputMode::Comment {
        return;
    }
    let (Some(state), Some((cursor_x, cursor_y))) =
        (app.mention.as_ref(), app.comment_cursor_screen_pos)
    else {
        return;
    };
    let screen = frame.area();
    let rows = state.candidates.len().min(MENTION_LIST_MAX_ROWS);
    if rows == 0 || screen.width < 8 {
        return;
    }
    let first = state.selected.saturating_sub(rows - 1);
    let visible = &state.candidates[first..first + rows];

    let content_width = visible
        .iter()
        .map(|c| {
            let name = c.name.as_deref().map_or(0, |n| n.width() + 2);
            c.login.width() + 3 + name
        })
        .max()
        .unwrap_or(0);
    let width = (content_width as u16 + 2).min(screen.width);
    let height = rows as u16 + 2;

    let at_x = cursor_x.saturating_sub(state.query.width() as u16 + 1);
    let x = at_x.min(screen.right().saturating_sub(width));
    // The bottom row is the status bar.
    let below = cursor_y + 1;
    let y = if below + height < screen.bottom() {
        below
    } else {
        cursor_y.saturating_sub(height)
    };
    let area = Rect::new(x, y, width, height).intersection(screen);

    let lines: Vec<Line<'_>> = visible
        .iter()
        .enumerate()
        .map(|(offset, candidate)| {
            let selected = first + offset == state.selected;
            let marker = if selected { CURSOR_GLYPH } else { " " };
            let style = if selected {
                styles::selected_style(&app.theme)
            } else {
                Style::default().fg(app.theme.fg_primary)
            };
            let mut spans = vec![Span::styled(
                format!("{marker} @{}", candidate.login),
                style,
            )];
            if let Some(name) = candidate.name.as_deref() {
                let name_style = if selected {
                    style
                } else {
                    Style::default().fg(app.theme.fg_secondary)
                };
                spans.push(Span::styled(format!("  {name}"), name_style));
            }
            Line::from(spans)
        })
        .collect();

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(styles::panel_style(&app.theme))
            .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use ratatui::style::Color;

    fn test_theme() -> Theme {
        Theme::default()
    }

    /// The renderer replaces off-screen comment boxes with exactly
    /// `App::comment_display_lines` blank rows, and the annotation builder sizes
    /// every comment the same way. If that count ever drifted from what
    /// `format_comment_lines` actually emits, the document would desync — the
    /// cursor would land on the wrong row and culled boxes would leave the wrong
    /// number of gaps. Pin the two together.
    #[test]
    fn comment_display_lines_matches_rendered_box_height() {
        let theme = test_theme();
        let bodies = [
            "",
            "single line",
            "first\nsecond\nthird",
            "trailing newline\n",
            "\n\nleading blanks",
            &"x".repeat(300),
            &"日本語のテキストです ".repeat(20),
            "`code` **bold** and a very long tail that will need to wrap at least once or twice",
        ];
        // Viewport widths, including degenerate ones narrower than the box chrome.
        for viewport_width in [9usize, 12, 40, 80, 120] {
            for body in bodies {
                let comment = crate::model::Comment::new(
                    body.to_string(),
                    crate::model::CommentType::from_id("note"),
                    None,
                );
                let rendered = format_comment_lines(
                    &theme,
                    CommentTypePresentation {
                        label: "NOTE".to_string(),
                        color: Color::Blue,
                    },
                    &comment.content,
                    None,
                    // What every call site passes: the viewport minus the
                    // cursor-indicator column.
                    viewport_width.saturating_sub(1),
                    None,
                    None,
                    ThreadChrome::default(),
                );
                assert_eq!(
                    App::comment_display_lines(&comment, viewport_width),
                    rendered.len(),
                    "width={viewport_width} body={body:?}"
                );
            }
        }
    }

    // -- wrap_segments tests --

    #[test]
    fn wrap_segments_returns_single_segment_when_text_fits() {
        // given
        let text = "hello";

        // when
        let segments = wrap_segments(text, 80);

        // then
        assert_eq!(segments, vec!["hello"]);
    }

    #[test]
    fn wrap_segments_returns_single_segment_for_empty_text() {
        // given
        let text = "";

        // when
        let segments = wrap_segments(text, 80);

        // then
        assert_eq!(segments, vec![""]);
    }

    #[test]
    fn wrap_segments_returns_text_unchanged_when_content_area_is_zero() {
        // given
        let text = "anything";

        // when
        let segments = wrap_segments(text, 0);

        // then
        assert_eq!(segments, vec!["anything"]);
    }

    #[test]
    fn wrap_segments_splits_long_ascii_at_content_area() {
        // given
        let text = "hello world";

        // when - content_area=5 means each segment is at most 5 display cols
        let segments = wrap_segments(text, 5);

        // then
        assert_eq!(segments, vec!["hello", " worl", "d"]);
    }

    #[test]
    fn wrap_segments_respects_cjk_display_width() {
        // given - each CJK char is 2 display cols, total = 8 cols, 12 bytes
        let text = "中文测试";

        // when - content_area=4 fits exactly 2 CJK chars per segment
        let segments = wrap_segments(text, 4);

        // then
        assert_eq!(segments, vec!["中文", "测试"]);
    }

    #[test]
    fn wrap_segments_handles_mixed_ascii_and_cjk() {
        // given - 'a'(1) + '中'(2) + 'b'(1) + '文'(2) = 6 display cols
        let text = "a中b文";

        // when - content_area=3 fits "a中" (1+2=3), then "b文" (1+2=3)
        let segments = wrap_segments(text, 3);

        // then
        assert_eq!(segments, vec!["a中", "b文"]);
    }

    #[test]
    fn wrap_segments_emits_oversized_char_to_avoid_infinite_loop() {
        // given - '中' is 2 cols wide but content_area only allows 1
        let text = "中a";

        // when
        let segments = wrap_segments(text, 1);

        // then - '中' is emitted even though it exceeds content_area
        assert_eq!(segments, vec!["中", "a"]);
    }

    #[test]
    fn wrap_segments_handles_exact_width_boundary() {
        // given - text exactly fills content_area
        let text = "12345";

        // when
        let segments = wrap_segments(text, 5);

        // then - single segment, no spurious empty trailing segment
        assert_eq!(segments, vec!["12345"]);
    }

    // -- format_comment_input_lines tests --

    #[test]
    fn should_return_cursor_at_start_for_empty_buffer() {
        // given
        let theme = test_theme();

        // when
        let (lines, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            "",
            0,
            None,
            "Add",
            false,
            80,
            None,
            true,
        );

        // then
        assert_eq!(lines.len(), 3); // header + content + footer
        assert_eq!(cursor_info.line_offset, 1); // cursor on first content line
        assert_eq!(cursor_info.column, 7); // "     │ " = 7 chars
    }

    #[test]
    fn should_return_cursor_position_for_ascii_text() {
        // given
        let theme = test_theme();
        let buffer = "hello";
        let cursor_pos = 3; // cursor after "hel"

        // when
        let (_, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor_pos,
            None,
            "Add",
            false,
            80,
            None,
            true,
        );

        // then
        assert_eq!(cursor_info.line_offset, 1); // first content line
        assert_eq!(cursor_info.column, 7 + 3); // border + "hel"
    }

    #[test]
    fn should_return_cursor_position_for_multibyte_text() {
        // given
        let theme = test_theme();
        let buffer = "안녕"; // 2 multibyte chars, 6 bytes, 4 display columns
        let cursor_pos = 3; // cursor after first multibyte char (after "안")

        // when
        let (_, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor_pos,
            None,
            "Add",
            false,
            80,
            None,
            true,
        );

        // then
        assert_eq!(cursor_info.line_offset, 1);
        // "안" has display width 2, so cursor column = border(7) + 2 = 9
        assert_eq!(cursor_info.column, 7 + 2);
    }

    #[test]
    fn should_return_cursor_position_at_end_of_text() {
        // given
        let theme = test_theme();
        let buffer = "test";
        let cursor_pos = 4; // cursor at end

        // when
        let (_, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor_pos,
            None,
            "Add",
            false,
            80,
            None,
            true,
        );

        // then
        assert_eq!(cursor_info.line_offset, 1);
        assert_eq!(cursor_info.column, 7 + 4); // border + "test"
    }

    #[test]
    fn should_return_cursor_position_on_second_line() {
        // given
        let theme = test_theme();
        let buffer = "line1\nline2";
        let cursor_pos = 8; // cursor after "li" in "line2"

        // when
        let (lines, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor_pos,
            None,
            "Add",
            false,
            80,
            None,
            true,
        );

        // then
        assert_eq!(lines.len(), 4); // header + 2 content lines + footer
        assert_eq!(cursor_info.line_offset, 2); // second content line (0=header, 1=line1, 2=line2)
        assert_eq!(cursor_info.column, 7 + 2); // border + "li"
    }

    #[test]
    fn should_return_cursor_position_for_mixed_content() {
        // given
        let theme = test_theme();
        let buffer = "a좋b"; // 1 + 3 + 1 = 5 bytes, 1 + 2 + 1 = 4 display columns
        let cursor_pos = 4; // cursor after "a좋" (1 + 3 bytes)

        // when
        let (_, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor_pos,
            None,
            "Add",
            false,
            80,
            None,
            true,
        );

        // then
        assert_eq!(cursor_info.line_offset, 1);
        // "a" = 1 display width, "좋" = 2 display width, total = 3
        assert_eq!(cursor_info.column, 7 + 3);
    }

    #[test]
    fn should_show_shift_enter_hint_when_keyboard_enhancement_supported() {
        let theme = test_theme();
        let (lines, _) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            "",
            0,
            None,
            "Add",
            false,
            80,
            None,
            true,
        );
        let header = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(header.contains("Shift-Enter:newline"));
        assert!(!header.contains("Alt-Enter:newline"));
    }

    #[test]
    fn should_show_alt_enter_hint_when_keyboard_enhancement_not_supported() {
        let theme = test_theme();
        let (lines, _) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            "",
            0,
            None,
            "Add",
            false,
            80,
            None,
            false,
        );
        let header = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(header.contains("Alt-Enter:newline"));
        assert!(!header.contains("Shift-Enter:newline"));
    }

    // -- markdown highlighting tests --

    /// Reconstruct the buffer text from the rendered content lines: drop the
    /// header (first) and footer (last) lines, skip each line's BORDER_PREFIX
    /// span, concatenate the rest, and join visual lines with '\n'. With a wide
    /// width (no wrapping) each logical line is one visual line, so this must
    /// equal the original buffer regardless of how content is split into spans.
    fn reconstruct(lines: &[Line<'static>]) -> String {
        lines[1..lines.len() - 1]
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .skip(1) // BORDER_PREFIX
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_md(buffer: &str, cursor: usize) -> Vec<Line<'static>> {
        let theme = test_theme();
        format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor,
            None,
            "Add",
            false,
            80,
            None,
            true,
        )
        .0
    }

    #[test]
    fn markdown_render_preserves_all_text() {
        let buffer = "# Title\n**bold** and `code`\n- item";
        // cursor in the middle of the bold span
        let lines = render_md(buffer, 11);
        assert_eq!(reconstruct(&lines), buffer);
    }

    #[test]
    fn markdown_render_preserves_multibyte_text() {
        let buffer = "# 世界\n**bold** 좋아";
        for cursor in [0, buffer.find('世').unwrap(), buffer.len()] {
            let lines = render_md(buffer, cursor);
            assert_eq!(reconstruct(&lines), buffer, "cursor={cursor}");
        }
    }

    #[test]
    fn displayed_comment_is_markdown_highlighted_and_preserves_text() {
        let theme = test_theme();
        let content = "# Heading\n**bold** and `code`\n- item";
        let lines = format_comment_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            content,
            None,
            80,
            None,
            None,
            ThreadChrome::default(),
        );
        // Header + footer wrap the body; reconstruct must round-trip the text.
        assert_eq!(reconstruct(&lines), content);
        // The inline-code line should be split into multiple styled runs.
        let code_line = &lines[2]; // header, heading, **this**
        assert!(
            code_line.spans.len() - 1 > 1,
            "expected displayed markdown to be highlighted into multiple spans"
        );
    }

    #[test]
    fn markdown_highlighting_splits_line_into_runs() {
        // An inline-code line should yield multiple styled content spans (proof
        // the markdown grammar resolved and coloring is applied), not one raw
        // span. Cursor at end so no cursor cell splits the line artificially.
        let buffer = "plain `code` plain";
        let lines = render_md(buffer, buffer.len());
        let content_spans = lines[1].spans.len() - 1; // minus BORDER_PREFIX
        assert!(
            content_spans > 1,
            "expected markdown highlighting to split the line, got {content_spans} span(s)"
        );
    }

    #[test]
    fn remote_thread_badge_uses_gitlab_for_gitlab_comments() {
        let thread = crate::forge::remote_comments::RemoteReviewThread {
            id: "thread".to_string(),
            path: "src/lib.rs".to_string(),
            line: Some(1),
            side: crate::forge::remote_comments::RemoteCommentSide::Right,
            is_resolved: false,
            is_outdated: false,
            comments: vec![crate::forge::remote_comments::RemoteReviewComment {
                id: "comment".to_string(),
                author: Some("alice".to_string()),
                body: "body".to_string(),
                created_at: None,
                in_reply_to: None,
                url: String::new(),
            }],
        };

        let lines = format_remote_thread_lines(
            &test_theme(),
            &thread,
            false,
            Some(ForgeKind::GitLab),
            false,
        );
        let header = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(header.contains("[gitlab @alice]"));
        assert!(!header.contains("[github @alice]"));
    }

    #[test]
    fn remote_summary_badge_uses_github_for_github_comments() {
        let summary = crate::forge::remote_comments::RemoteReviewSummary {
            id: "summary".to_string(),
            author: Some("alice".to_string()),
            body: "body".to_string(),
            state: crate::forge::remote_comments::RemoteReviewState::Commented,
            created_at: None,
            url: String::new(),
        };

        let lines =
            format_remote_review_summary_lines(&test_theme(), &summary, Some(ForgeKind::GitHub));
        let header = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(header.contains("[github @alice]"));
    }

    fn rendered_box(thread: ThreadChrome, line_range: Option<LineRange>) -> Vec<String> {
        format_comment_lines(
            &test_theme(),
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            "done",
            line_range,
            40,
            Some("Claude"),
            None,
            thread,
        )
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
    }

    #[test]
    fn should_draw_a_local_reply_as_a_divider_inside_its_thread() {
        let mid = rendered_box(
            ThreadChrome {
                threads: true,
                reply_author: Some("Claude".to_string()),
                continues: true,
            },
            Some(LineRange::single(3)),
        );

        assert!(mid[0].starts_with("    \u{251c}\u{2500}\u{2500} \u{21b3} @Claude "));
        // a reply follows: no closing rule of its own
        assert_eq!(mid.len(), 2);
    }

    #[test]
    fn should_draw_a_local_comment_as_upstream_does_without_threads() {
        let plain = rendered_box(ThreadChrome::default(), Some(LineRange::single(3)));

        // the tee the marker line joins, the author in the badge, closed
        assert!(plain[0].starts_with("    \u{251c}\u{2500}\u{2500} [NOTE @Claude] L3 "));
        assert!(plain.last().unwrap().starts_with("    \u{2570}"));
        assert_eq!(plain.len(), 3);

        let thread = rendered_box(
            ThreadChrome {
                threads: true,
                ..ThreadChrome::default()
            },
            Some(LineRange::single(3)),
        );
        // with threads, no marker line: the box opens with a corner
        assert!(thread[0].starts_with("    \u{256d}\u{2500}\u{2500} [NOTE @Claude] L3 "));
    }
}
