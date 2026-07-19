//! Rendering for the chat TUI. Pure functions over the [`App`] state; the
//! wrapping helpers are width-aware (CJK chars are double-width) and
//! unit-tested — ratatui's own `Paragraph::wrap` can't report how many lines
//! it produced, which the bottom-anchored scroll needs.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use super::app::{App, Role};
use super::markdown::markdown_lines;

const SPINNER: [&str; 4] = ["⠇", "⠋", "⠙", "⠸"];

pub fn render(frame: &mut Frame, app: &App) {
    let [transcript_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    render_transcript(frame, app, transcript_area);
    render_status(frame, app, status_area);
    render_input(frame, app, input_area);
    if let Some(prompt) = &app.modal {
        render_modal(frame, prompt, frame.area());
    }
}

fn render_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.max(1) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for entry in &app.entries {
        // Agent replies render as markdown; everything else is plain text
        // behind a colored role prefix.
        if entry.role == Role::Agent {
            for logical in markdown_lines(&entry.text) {
                lines.extend(wrap_spans(logical.spans, width));
            }
            lines.push(Line::default());
            continue;
        }
        let (prefix, style) = match entry.role {
            Role::You => ("❯ ", Style::new().fg(Color::Cyan)),
            Role::Agent => ("", Style::new()),
            Role::Info => ("· ", Style::new().fg(Color::DarkGray)),
            Role::Error => ("✗ ", Style::new().fg(Color::Red)),
        };
        for (i, wrapped) in wrap_text(&entry.text, width.saturating_sub(prefix.chars().count()))
            .into_iter()
            .enumerate()
        {
            let head = if i == 0 {
                prefix.to_string()
            } else {
                " ".repeat(prefix.chars().count())
            };
            lines.push(Line::from(vec![
                Span::styled(head, style.add_modifier(Modifier::BOLD)),
                Span::styled(wrapped, style),
            ]));
        }
        // A blank separator between messages keeps the transcript scannable.
        lines.push(Line::default());
    }

    // Bottom-anchored scroll: 0 = follow the tail; scrolling up moves the
    // window back through the wrapped lines, clamped at the top.
    let height = area.height as usize;
    let max_offset = lines.len().saturating_sub(height);
    let offset = (app.scroll_from_bottom as usize).min(max_offset);
    let start = max_offset - offset;
    let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    frame.render_widget(Paragraph::new(visible), area);
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let status = if app.in_flight && app.awaiting_answer {
        Line::from(vec![
            Span::styled(
                " ❓ 等待你的回答 — 直接输入并回车 ",
                Style::new().fg(Color::Cyan),
            ),
            Span::styled(
                format!("session {}", app.session_id),
                Style::new().fg(Color::DarkGray),
            ),
        ])
    } else if app.in_flight {
        Line::from(vec![
            Span::styled(
                format!(" {} thinking… ", SPINNER[app.spinner % SPINNER.len()]),
                Style::new().fg(Color::Yellow),
            ),
            Span::styled(
                format!("session {}", app.session_id),
                Style::new().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(Span::styled(
            format!(
                " session {} · Enter 发送 · /new 新会话 · ↑↓ 滚动 · Ctrl-C 退出",
                app.session_id
            ),
            Style::new().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(status), area);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::new().borders(Borders::ALL).title(" message ");
    let inner = block.inner(area);
    let inner_width = inner.width.max(1) as usize;

    // Keep the cursor visible: show the window of the input ending no earlier
    // than the cursor (simple left-trim by display width).
    let before: String = app.input.chars().take(app.cursor).collect();
    let cursor_col = display_width(&before);
    let skip_cols = cursor_col.saturating_sub(inner_width.saturating_sub(1));
    let (visible, skipped) = trim_left_cols(&app.input, skip_cols);

    frame.render_widget(Paragraph::new(visible).block(block), area);
    // `skipped` can overshoot `skip_cols` by one column (a double-width char is
    // dropped whole), so saturate rather than underflow.
    let x = inner.x + cursor_col.saturating_sub(skipped) as u16;
    frame.set_cursor_position((x.min(inner.x + inner.width.saturating_sub(1)), inner.y));
}

fn render_modal(frame: &mut Frame, prompt: &super::approver::ApprovalPrompt, screen: Rect) {
    let (title, border) = if prompt.dangerous {
        (" 🛑 需要审批(危险操作) ", Style::new().fg(Color::Red))
    } else {
        (" ⚠ 需要审批 ", Style::new().fg(Color::Yellow))
    };
    let width = screen.width.saturating_sub(8).clamp(20, 80);
    let inner_width = width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = wrap_text(&prompt.summary, inner_width)
        .into_iter()
        .map(Line::from)
        .collect();
    if let Some(detail) = &prompt.detail {
        lines.push(Line::default());
        for l in wrap_text(detail, inner_width) {
            lines.push(Line::from(Span::styled(
                l,
                Style::new().fg(Color::DarkGray),
            )));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "[y] 允许一次   [s] 本会话内同类操作   [n/Esc] 拒绝",
        Style::new().add_modifier(Modifier::BOLD),
    )));

    let height = (lines.len() as u16 + 2).min(screen.height.saturating_sub(2));
    let rect = centered(screen, width, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(border)
                .title(title),
        ),
        rect,
    );
}

fn centered(screen: Rect, width: u16, height: u16) -> Rect {
    let x = screen.x + screen.width.saturating_sub(width) / 2;
    let y = screen.y + screen.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(screen.width), height.min(screen.height))
}

/// Display width of a string (CJK double-width aware).
fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Drop columns from the left until at least `cols` display columns are gone,
/// returning the remainder and how many columns were actually dropped.
fn trim_left_cols(s: &str, cols: usize) -> (String, usize) {
    if cols == 0 {
        return (s.to_string(), 0);
    }
    let mut dropped = 0usize;
    let mut out = String::new();
    let mut trimming = true;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if trimming && dropped < cols {
            dropped += w;
        } else {
            trimming = false;
            out.push(c);
        }
    }
    (out, dropped)
}

/// Hard-wrap `text` (which may contain newlines) to `width` display columns.
/// Splits on char boundaries with CJK width awareness — no word-break
/// cleverness, but never overflows and never loses content. An empty input
/// still yields one empty line so the entry occupies a row.
pub(super) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for logical in text.split('\n') {
        let mut line = String::new();
        let mut cols = 0usize;
        for c in logical.chars() {
            let w = c.width().unwrap_or(0);
            if cols + w > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                cols = 0;
            }
            line.push(c);
            cols += w;
        }
        out.push(line);
    }
    out
}

/// Hard-wrap a logical line of styled spans to `width` display columns,
/// splitting spans at char boundaries — the styled counterpart of
/// [`wrap_text`], with the same CJK width rules. An empty input still yields
/// one empty line so the entry occupies a row.
fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut cols = 0usize;
    for span in spans {
        let mut buf = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if cols + w > width && cols > 0 {
                if !buf.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut buf), span.style));
                }
                out.push(Line::from(std::mem::take(&mut current)));
                cols = 0;
            }
            buf.push(c);
            cols += w;
        }
        if !buf.is_empty() {
            current.push(Span::styled(buf, span.style));
        }
    }
    out.push(Line::from(current));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_respects_cjk_double_width() {
        // 4 CJK chars = 8 columns; at width 4 that is 2 chars per line.
        let lines = wrap_text("你好世界", 4);
        assert_eq!(lines, vec!["你好", "世界"]);
    }

    #[test]
    fn wrap_preserves_newlines_and_empty_lines() {
        assert_eq!(wrap_text("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn wrap_never_loses_content() {
        let text = "abcdefghij";
        let rejoined: String = wrap_text(text, 3).concat();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn wrap_spans_keeps_styles_across_the_break() {
        let spans = vec![
            Span::styled("你好", Style::new().fg(Color::Cyan)),
            Span::styled("世界", Style::new().fg(Color::Red)),
        ];
        // 8 columns of CJK at width 4 = 2 chars per line, one span each.
        let lines = wrap_spans(spans, 4);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].content, "你好");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(lines[1].spans[0].content, "世界");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn wrap_spans_splits_one_span_without_losing_content() {
        let lines = wrap_spans(vec![Span::raw("abcdefghij")], 3);
        let rejoined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(rejoined, "abcdefghij");
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn trim_left_drops_whole_wide_chars() {
        // Trimming 1 column out of a double-width char drops the whole char
        // (2 columns) — the cursor math accounts for the actual drop.
        let (rest, dropped) = trim_left_cols("你ab", 1);
        assert_eq!(rest, "ab");
        assert_eq!(dropped, 2);
    }

    /// Headless render smoke: a full frame (transcript + status + input, then
    /// with an approval modal) draws without panicking and shows the content.
    #[test]
    fn renders_frame_and_modal_on_test_backend() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut app = App::new("sess-1".into());
        app.push(Role::Info, "Komo v0.1 — session sess-1");
        app.push(Role::You, "hello 你好");
        app.push(Role::Agent, "hi there");
        app.input = "draft".into();
        app.cursor = 5;

        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let content = format!("{:?}", terminal.backend().buffer());
        assert!(content.contains("hello"), "user entry rendered");
        assert!(content.contains("hi there"), "agent entry rendered");
        assert!(content.contains("draft"), "input draft rendered");

        // Modal overlays and captures the frame.
        app.modal = Some(super::super::approver::ApprovalPrompt {
            summary: "run shell command: rm -rf /tmp/x".into(),
            detail: Some("matched dangerous pattern".into()),
            dangerous: true,
            reply: None,
        });
        app.in_flight = true; // spinner path renders too
        terminal.draw(|f| render(f, &app)).unwrap();
        let content = format!("{:?}", terminal.backend().buffer());
        assert!(content.contains("rm -rf"), "modal summary rendered");
        assert!(content.contains("拒绝"), "modal key hints rendered");
    }
}
