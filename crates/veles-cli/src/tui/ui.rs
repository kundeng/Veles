//! Render functions for the TUI.
//!
//! Pure presentation: every function takes `&App` (or `&mut App` only when
//! it needs to clamp scroll offsets to the actual viewport) and writes
//! widgets into the frame. No I/O, no channel work, no state machines.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;
use veles_core::types::SearchMode;

use crate::tui::app::{App, ResultsKind};

// ── Palette ──────────────────────────────────────────────────────────
//
// Tuned to look good on dark terminals while still being readable on
// light backgrounds. Pure ANSI colors are used as fallbacks where Rgb
// truecolor would otherwise clash with the user's theme.

const BORDER: Color = Color::Rgb(72, 80, 110);
const BORDER_FOCUS: Color = Color::Rgb(125, 207, 255);
const TITLE: Color = Color::Rgb(180, 220, 255);
const FAINT: Color = Color::Rgb(120, 130, 150);
const DIM: Color = Color::DarkGray;
const TEXT: Color = Color::Rgb(220, 224, 235);
const ACCENT: Color = Color::Rgb(125, 207, 255);
const HIGH: Color = Color::Rgb(160, 230, 130);
const MID: Color = Color::Rgb(255, 200, 80);
const SEL_BG: Color = Color::Rgb(48, 60, 90);
const HEADER_BG: Color = Color::Rgb(28, 32, 46);
const CHUNK_BG: Color = Color::Rgb(34, 40, 58);

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top bar
            Constraint::Length(3), // query box
            Constraint::Min(5),    // body
            Constraint::Length(1), // keys hint
        ])
        .split(area);

    render_top_bar(f, layout[0], app);
    render_query(f, layout[1], app);
    render_body(f, layout[2], app);
    render_keys(f, layout[3], app);

    if app.help_open {
        render_help(f, area, app);
    }
}

// ── Top bar ──────────────────────────────────────────────────────────

fn render_top_bar(f: &mut Frame, area: Rect, app: &App) {
    let bar_style = Style::default().bg(HEADER_BG).fg(TEXT);
    f.render_widget(Block::default().style(bar_style), area);

    let repo = app.repo_short.as_str();
    let stats = format!("{} chunks · {} files", app.total_chunks, app.total_files);

    let left_spans = vec![
        Span::styled(" ▎ ", Style::default().fg(ACCENT).bg(HEADER_BG)),
        Span::styled(
            "Veles",
            Style::default()
                .fg(TITLE)
                .bg(HEADER_BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", Style::default().fg(FAINT).bg(HEADER_BG)),
        Span::styled(repo.to_string(), Style::default().fg(TEXT).bg(HEADER_BG)),
        Span::styled("  ·  ", Style::default().fg(FAINT).bg(HEADER_BG)),
        Span::styled(stats, Style::default().fg(FAINT).bg(HEADER_BG)),
    ];

    let mode_color = mode_color(app.mode);
    let mode_label = format!(" {} ", mode_label(app.mode));
    let timing = if app.show_spinner() {
        format!(" {} searching ", spinner_frame(app.spinner_tick))
    } else if app.elapsed_ms > 0 || !app.results.is_empty() {
        format!(" {} ms ", app.elapsed_ms)
    } else {
        "       ".to_string()
    };

    let mut right_spans: Vec<Span<'static>> = Vec::new();
    // Filter chips render before timing/mode so they sit closer to the
    // centre — they're the most actionable state and shouldn't get
    // pushed off-screen on narrow terminals (mode/timing are always
    // visible).
    if let Some(lang) = &app.filter_lang {
        right_spans.push(Span::styled(
            format!(" lang:{lang} "),
            Style::default()
                .fg(Color::Black)
                .bg(MID)
                .add_modifier(Modifier::BOLD),
        ));
        right_spans.push(Span::styled(" ", Style::default().bg(HEADER_BG)));
    }
    if !app.filter_path.is_empty() {
        right_spans.push(Span::styled(
            format!(" path:{} ", app.filter_path),
            Style::default()
                .fg(Color::Black)
                .bg(HIGH)
                .add_modifier(Modifier::BOLD),
        ));
        right_spans.push(Span::styled(" ", Style::default().bg(HEADER_BG)));
    }
    right_spans.extend([
        Span::styled(timing, Style::default().fg(FAINT).bg(HEADER_BG)),
        Span::styled(
            mode_label,
            Style::default()
                .fg(Color::Black)
                .bg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", Style::default().bg(HEADER_BG)),
    ]);

    let left_w: usize = left_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let right_w: usize = right_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let total = area.width as usize;
    let pad = total.saturating_sub(left_w + right_w);

    let mut combined = left_spans;
    combined.push(Span::styled(
        " ".repeat(pad),
        Style::default().bg(HEADER_BG),
    ));
    combined.extend(right_spans);

    let line = Line::from(combined);
    f.render_widget(Paragraph::new(line).style(bar_style), area);
}

// ── Query input ──────────────────────────────────────────────────────

fn render_query(f: &mut Frame, area: Rect, app: &App) {
    // While the path filter is being edited, the box is repurposed for
    // glob input — the title, prompt, and active text all switch.
    let editing_path = app.editing_path_filter;
    let title = if editing_path {
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Path filter ",
                Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
            ),
            Span::styled("(glob; Esc/Enter applies)", Style::default().fg(FAINT)),
            Span::raw(" "),
        ])
    } else {
        match &app.results_kind {
            ResultsKind::Related { anchor } => Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "Find related",
                    Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" → ", Style::default().fg(FAINT)),
                Span::styled(anchor.clone(), Style::default().fg(ACCENT)),
                Span::raw(" "),
            ]),
            ResultsKind::Defs { name } => Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "Defs of ",
                    Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("`{name}`"), Style::default().fg(ACCENT)),
                Span::raw(" "),
            ]),
            ResultsKind::Refs { name, .. } => Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "Refs of ",
                    Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("`{name}`"), Style::default().fg(ACCENT)),
                Span::raw(" "),
            ]),
            ResultsKind::Query { .. } => Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "Search",
                    Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]),
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if editing_path { HIGH } else { BORDER_FOCUS }))
        .title_top(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let (prompt_glyph, prompt_color) = if editing_path {
        (" 🔎 ", HIGH)
    } else {
        (" ❯ ", ACCENT)
    };
    let prompt = Span::styled(
        prompt_glyph,
        Style::default()
            .fg(prompt_color)
            .add_modifier(Modifier::BOLD),
    );
    let (active_text, active_cursor) = if editing_path {
        (&app.filter_path, app.filter_path_cursor)
    } else {
        (&app.query, app.cursor_chars)
    };
    let prompt_cols: u16 = UnicodeWidthStr::width(prompt_glyph) as u16;
    // Available columns inside the box for the text after the prompt.
    let text_cols = inner.width.saturating_sub(prompt_cols).max(1) as usize;

    let text_span = if active_text.is_empty() {
        let placeholder = if editing_path {
            "type a glob (e.g. crates/**/*.rs) · Esc / Enter to apply · Ctrl-U clear"
        } else {
            "type to search · Ctrl-D defs · Ctrl-F refs · Ctrl-R related · ? help"
        };
        Span::styled(
            placeholder,
            Style::default().fg(FAINT).add_modifier(Modifier::ITALIC),
        )
    } else {
        // Horizontal viewport: pan the visible window so the cursor
        // always stays inside. Without this, long queries get silently
        // clipped at inner.width-1 and the tail (plus the cursor) sit
        // off-screen invisibly.
        let cursor_visual = visual_width_chars(active_text, active_cursor);
        let total_visual = UnicodeWidthStr::width(active_text.as_str());
        let view_start_col = if total_visual <= text_cols {
            0
        } else if cursor_visual >= text_cols {
            // Cursor would overflow the right edge — slide left so it
            // sits one column inside the right margin.
            cursor_visual + 1 - text_cols
        } else {
            0
        };
        let visible = visible_slice(active_text, view_start_col, text_cols);
        Span::styled(visible, Style::default().fg(TEXT))
    };
    let line = Line::from(vec![prompt, text_span]);
    f.render_widget(Paragraph::new(line), inner);

    // Cursor placement: same horizontal offset as the visible window so
    // it tracks where the user is typing instead of pinning to the
    // right edge.
    let cursor_visual = visual_width_chars(active_text, active_cursor);
    let total_visual = UnicodeWidthStr::width(active_text.as_str());
    let view_start_col = if total_visual <= text_cols {
        0
    } else if cursor_visual >= text_cols {
        cursor_visual + 1 - text_cols
    } else {
        0
    };
    let cursor_col_in_box = prompt_cols as usize + cursor_visual.saturating_sub(view_start_col);
    let cx = inner.x + (cursor_col_in_box as u16).min(inner.width.saturating_sub(1));
    let cy = inner.y;
    f.set_cursor_position(Position { x: cx, y: cy });
}

/// Slice a string so that the result starts at `skip_cols` (visual)
/// from the left and is at most `max_cols` wide. Used to pan the query
/// input horizontally when it overflows the box.
fn visible_slice(s: &str, skip_cols: usize, max_cols: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    let mut taken = 0usize;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if col < skip_cols {
            col += w;
            continue;
        }
        if taken + w > max_cols {
            break;
        }
        out.push(c);
        taken += w;
    }
    out
}

// ── Body (results + preview) ─────────────────────────────────────────

fn render_body(f: &mut Frame, area: Rect, app: &mut App) {
    let split_horizontally = area.width >= 100;

    let chunks = if split_horizontally {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(area)
    } else {
        // On narrow terminals, hide the preview pane.
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(area)
    };

    // Lifted from render_results / render_preview where it was computed
    // twice per frame. A single fold over `results` per render is plenty.
    let max_score = app
        .results
        .iter()
        .map(|r| r.score)
        .fold(f64::NEG_INFINITY, f64::max);

    render_results(f, chunks[0], app, max_score);
    if chunks.len() > 1 {
        render_preview(f, chunks[1], app, max_score);
    }
}

fn render_results(f: &mut Frame, area: Rect, app: &mut App, max_score: f64) {
    let total = app.results.len();
    let pos = if total == 0 { 0 } else { app.selected + 1 };
    let title = match &app.results_kind {
        ResultsKind::Related { .. } => Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Related",
                Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  (semantic — try Ctrl-F for refs)",
                Style::default().fg(FAINT).add_modifier(Modifier::ITALIC),
            ),
            Span::styled(format!("  {pos}/{total} "), Style::default().fg(FAINT)),
        ]),
        ResultsKind::Defs { name } => Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Defs",
                Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" `{name}`"), Style::default().fg(ACCENT)),
            Span::styled(format!("  {pos}/{total} "), Style::default().fg(FAINT)),
        ]),
        ResultsKind::Refs { name, def_count } => {
            // Refs results = first `def_count` rows are tree-sitter
            // definitions, the rest are BM25 reference hits. The header
            // shows both counts so the user sees "2 defs + 5 refs" at a
            // glance instead of just an opaque total.
            let ref_count = total.saturating_sub(*def_count);
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "Refs",
                    Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" `{name}`"), Style::default().fg(ACCENT)),
                Span::styled(
                    format!("  {def_count} defs + {ref_count} refs "),
                    Style::default().fg(FAINT),
                ),
            ])
        }
        ResultsKind::Query { .. } => Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Results",
                Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {pos}/{total} "), Style::default().fg(FAINT)),
        ]),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title_top(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.results.is_empty() {
        let msg = if app.show_spinner() {
            format!("{} searching ...", spinner_frame(app.spinner_tick))
        } else if app.searching {
            // In-flight but still under the spinner threshold: show
            // nothing (sub-frame searches blink in/out anyway).
            String::new()
        } else if app.query.is_empty() {
            "(start typing to search)".to_string()
        } else {
            "(no results)".to_string()
        };
        let p = Paragraph::new(msg)
            .style(Style::default().fg(FAINT).add_modifier(Modifier::ITALIC))
            .alignment(Alignment::Center);
        let h = inner.height.saturating_sub(1) / 2;
        let centered = Rect {
            x: inner.x,
            y: inner.y + h,
            width: inner.width,
            height: 1,
        };
        f.render_widget(p, centered);
        return;
    }

    // Clamp scroll offset so the selected row stays visible. The
    // viewport is only known after layout, so the renderer drives
    // the clamp — the actual mutation lives on `App` for clarity.
    let viewport = inner.height as usize;
    if viewport == 0 {
        return;
    }
    app.clamp_list_offset(viewport);
    let end = (app.list_offset + viewport).min(app.results.len());

    // Give trailing text (scope label / snippet) more room: cap path
    // padding at 40 cols rather than 60 so the right-hand label isn't
    // squeezed to nothing on terminals split 42/58 with a preview pane.
    let path_col = (inner.width as usize).saturating_sub(8 + 2 + 1).min(40); // budget for path

    // `max_score` (relative-colour reference) is passed in from
    // render_body — one fold per frame instead of two. Hybrid scores
    // are RRF-blended (max ≈ 0.02) while BM25 is unbounded, so a single
    // static threshold won't do — relative-against-max is the only
    // scheme that works across modes.

    let mut lines: Vec<Line> = Vec::with_capacity(end - app.list_offset);
    for idx in app.list_offset..end {
        let r = &app.results[idx];
        let selected = idx == app.selected;
        let arrow = if selected { " ▸ " } else { "   " };
        let row_bg = if selected { Some(SEL_BG) } else { None };
        let path_text = format!(
            "{}:{}-{}",
            r.chunk.file_path, r.chunk.start_line, r.chunk.end_line
        );
        let path_text = pad_or_truncate(&path_text, path_col);
        // Tree-sitter def rows have a placeholder score=1.0 from the
        // worker; replacing the number with a short tag makes the row
        // honest and saves the eye from comparing to real BM25 scores.
        let is_def = app.is_treesitter_row(idx);
        let score_text = if is_def {
            "  def".to_string()
        } else {
            format!("{:>5.3}", r.score)
        };

        // Prefer the tree-sitter scope label (`defines `Foo`` / `in `bar``)
        // when available — it's a more reliable "what is this" signal than
        // the chunk's first non-blank line. Fall back to the snippet when
        // the chunk doesn't sit inside any recognised symbol.
        //
        // Uses the pre-built ScopeIndex for O(symbols_in_file) lookup
        // instead of scanning the whole symbol table per row.
        let scope_label = app.scope_index.label(app.index.symbols(), &r.chunk);
        let (trailing_text, trailing_is_scope) = match scope_label {
            Some(label) => (label, true),
            None => (first_nonblank_line(&r.chunk.content).to_string(), false),
        };
        let trailing_max = (inner.width as usize)
            .saturating_sub(arrow.len() + path_text.len() + score_text.len() + 4);
        let trailing = truncate(&trailing_text, trailing_max);

        let row_style = |s: Style| -> Style {
            match row_bg {
                Some(bg) => s.bg(bg),
                None => s,
            }
        };

        let mut spans = Vec::with_capacity(6);
        spans.push(Span::styled(
            arrow.to_string(),
            row_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        ));
        spans.push(Span::styled(
            path_text,
            row_style(if selected {
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            }),
        ));
        spans.push(Span::styled("  ".to_string(), row_style(Style::default())));
        spans.push(Span::styled(
            score_text,
            row_style(
                Style::default()
                    .fg(if is_def {
                        ACCENT
                    } else {
                        score_color(r.score, max_score)
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ));
        spans.push(Span::styled("  ".to_string(), row_style(Style::default())));
        // Scope labels render in a slightly brighter colour than raw
        // code snippets so the metadata stands out at a glance. Italic
        // is intentionally avoided — many terminals don't render it.
        let trailing_style = if trailing_is_scope {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(FAINT)
        };
        spans.push(Span::styled(
            trailing.to_string(),
            row_style(trailing_style),
        ));

        // Pad rest of the line with spaces so the row-bg fills the row.
        let used: usize = spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        if let Some(bg) = row_bg {
            let pad = (inner.width as usize).saturating_sub(used);
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
            }
        }
        lines.push(Line::from(spans));
    }

    let p = Paragraph::new(lines).style(Style::default());
    f.render_widget(p, inner);
}

fn render_preview(f: &mut Frame, area: Rect, app: &App, max_score: f64) {
    let sel_is_def = app.is_treesitter_row(app.selected);
    let title = match app.results.get(app.selected) {
        Some(r) => {
            let badge = if sel_is_def {
                // Tree-sitter defs get a distinct badge — the worker's
                // placeholder score=1.0 / source=bm25 isn't meaningful
                // here. Same colour family as scope labels in the row
                // list so the visual story stays consistent.
                Span::styled(
                    " tree-sitter def ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    format!(" {:.3} ", r.score),
                    Style::default()
                        .fg(Color::Black)
                        .bg(score_color(r.score, max_score))
                        .add_modifier(Modifier::BOLD),
                )
            };
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    r.chunk.file_path.clone(),
                    Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(":{}-{} ", r.chunk.start_line, r.chunk.end_line),
                    Style::default().fg(FAINT),
                ),
                badge,
                Span::raw(" "),
            ])
        }
        None => Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Preview",
                Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]),
    };

    let bottom = match app.results.get(app.selected) {
        Some(r) => {
            let source_label = if sel_is_def {
                "source: tree-sitter".to_string()
            } else {
                format!("source: {}", r.source.as_str())
            };
            Line::from(vec![
                Span::raw(" "),
                Span::styled(source_label, Style::default().fg(FAINT)),
                match r.chunk.language.as_deref() {
                    Some(lang) => Span::styled(format!("  ·  {lang} "), Style::default().fg(FAINT)),
                    None => Span::raw(" "),
                },
            ])
        }
        None => Line::raw(""),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title_top(title)
        .title_bottom(bottom);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(sel) = app.results.get(app.selected) else {
        render_welcome(f, inner, app);
        return;
    };

    let chunk = &sel.chunk;
    let viewport_h = inner.height as usize;
    if viewport_h == 0 {
        return;
    }
    let terms = app.query_terms();
    let lower_terms = app.lower_terms();

    let lines: Vec<Line<'static>> = match &app.current_preview {
        Some(file_lines) if !file_lines.is_empty() => {
            let total = file_lines.len();

            // Compute the window. The user-driven "preview_scroll" offset
            // lets the reader pan ±viewport over the default window. The
            // default window centers on the match line inside the chunk
            // when one exists, otherwise it shows a few lines of context
            // above chunk.start_line. The anchor comes from the event-
            // loop-populated `cached_match_line` so re-renders skip the
            // per-line lowercase scan entirely.
            let anchor = app.cached_match_line.unwrap_or(chunk.start_line);
            let default_start = compute_preview_start(chunk, anchor, viewport_h, total);

            // Apply user scroll offset, clamped to file range.
            // `max_start` is the largest first-line that still lets `end`
            // reach the last line of the file: end = start + viewport - 1
            // = total ⇒ start = total - viewport + 1. The previous clamp
            // used `total - viewport` and hid the file's last line when
            // the user scrolled all the way down.
            let scroll = app.preview_scroll;
            let max_start = total.saturating_sub(viewport_h.saturating_sub(1)).max(1);
            let start = if scroll >= 0 {
                (default_start + scroll as usize).min(max_start)
            } else {
                default_start.saturating_sub((-scroll) as usize).max(1)
            };
            let end = (start + viewport_h.saturating_sub(1)).min(total);

            let mut out = Vec::with_capacity(end + 1 - start);
            for ln in start..=end {
                let content = file_lines.get(ln - 1).map(String::as_str).unwrap_or("");
                let in_chunk = ln >= chunk.start_line && ln <= chunk.end_line;
                out.push(format_preview_line(
                    ln,
                    content,
                    in_chunk,
                    terms,
                    lower_terms,
                ));
            }
            out
        }
        _ => {
            // File missing or empty: render the chunk content directly.
            chunk
                .content
                .lines()
                .enumerate()
                .map(|(i, content)| {
                    let ln = chunk.start_line + i;
                    format_preview_line(ln, content, true, terms, lower_terms)
                })
                .collect()
        }
    };

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

// `find_match_line` was replaced by `App::match_line_for_selection`,
// which caches the result and uses the pre-lowered cached terms.
// Removed here to avoid duplicating the per-frame lowercase scan.

/// Pick the first line of the preview viewport.
///
/// Strategy:
/// 1. Try the legacy default — 3 lines of context above `chunk.start_line`.
/// 2. If that puts the match `anchor` line outside the viewport, scroll
///    so the anchor lands roughly in the upper third (a third above,
///    two thirds below) so the reader sees both context and the match.
///
/// Without this scroll, a chunk larger than the viewport (e.g. 50-line
/// chunk in a 25-row terminal) hid matches near the end of the chunk
/// entirely — the user's reported bug.
fn compute_preview_start(
    chunk: &veles_core::types::Chunk,
    anchor: usize,
    viewport_h: usize,
    total: usize,
) -> usize {
    let context_above: usize = 3;
    let default_start = chunk.start_line.saturating_sub(context_above).max(1);
    let default_end = default_start + viewport_h.saturating_sub(1);

    if anchor >= default_start && anchor <= default_end {
        // Anchor is visible in the default window — keep it.
        return default_start;
    }

    // Scroll so anchor sits ~1/3 into the viewport. Reserves 1/3 of the
    // height for upward context, 2/3 for forward reading.
    let upper_context = viewport_h / 3;
    let proposed = anchor.saturating_sub(upper_context).max(1);
    // Don't scroll past the end of the file.
    let max_start = total.saturating_sub(viewport_h.saturating_sub(1)).max(1);
    proposed.min(max_start)
}

fn render_welcome(f: &mut Frame, area: Rect, app: &App) {
    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "Veles TUI",
            Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(
            "live hybrid (BM25 + semantic) search",
            Style::default().fg(FAINT).add_modifier(Modifier::ITALIC),
        )]),
        Line::from(""),
        kbd("Tab", "cycle mode (hybrid · bm25 · semantic)"),
        kbd("↑ ↓", "navigate results"),
        kbd("Enter", "print path:line and quit"),
        kbd("Ctrl-O", "open in $EDITOR"),
        kbd("Ctrl-R", "find code related to selection"),
        kbd("Ctrl-D", "definitions of typed identifier"),
        kbd("Ctrl-F", "definitions + references of typed identifier"),
        kbd("?", "full keybinding help"),
        kbd("Esc", "quit"),
        Line::from(""),
        Line::from(vec![
            Span::styled("Indexed ", Style::default().fg(FAINT)),
            Span::styled(
                format!("{}", app.total_chunks),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" chunks across ", Style::default().fg(FAINT)),
            Span::styled(
                format!("{}", app.total_files),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" files.", Style::default().fg(FAINT)),
        ]),
    ];
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 0,
    });
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

// ── Status / keys hint ───────────────────────────────────────────────

fn render_keys(f: &mut Frame, area: Rect, app: &App) {
    // When a transient status message is live (e.g. key-debug echo),
    // it replaces the keys hint until it expires. Keeps the bottom row
    // single-line and avoids fighting for space.
    if let Some((msg, until)) = &app.status_msg
        && std::time::Instant::now() < *until
    {
        let line = Line::from(vec![
            Span::styled(" ▸ ", Style::default().fg(MID)),
            Span::styled(
                msg.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let mut spans = vec![
        Span::styled(" ↑↓", Style::default().fg(ACCENT)),
        Span::styled(" navigate  ", Style::default().fg(FAINT)),
        Span::styled("Enter", Style::default().fg(ACCENT)),
        Span::styled(" open  ", Style::default().fg(FAINT)),
        Span::styled("Tab", Style::default().fg(ACCENT)),
        Span::styled(" mode  ", Style::default().fg(FAINT)),
        Span::styled("Ctrl-R", Style::default().fg(ACCENT)),
        Span::styled(" related  ", Style::default().fg(FAINT)),
        Span::styled("Ctrl-D", Style::default().fg(ACCENT)),
        Span::styled(" defs  ", Style::default().fg(FAINT)),
        Span::styled("Ctrl-F", Style::default().fg(ACCENT)),
        Span::styled(" refs  ", Style::default().fg(FAINT)),
    ];
    // Only surface Alt-← / Alt-→ when there's somewhere to go — keeps the
    // bar uncluttered for first-time users and signals when history is
    // available at a glance.
    // Surface back/forward only when there's somewhere to go. Ctrl-B /
    // Ctrl-X are advertised because they're the only combos that work
    // on every common platform out of the box (Alt needs Mac config,
    // F-keys need Fn or system-pref toggle).
    if !app.history_back.is_empty() {
        spans.push(Span::styled("Ctrl-B", Style::default().fg(ACCENT)));
        spans.push(Span::styled(" back  ", Style::default().fg(FAINT)));
    }
    if !app.history_forward.is_empty() {
        spans.push(Span::styled("Ctrl-X", Style::default().fg(ACCENT)));
        spans.push(Span::styled(" fwd  ", Style::default().fg(FAINT)));
    }
    spans.extend([
        Span::styled("Ctrl-O", Style::default().fg(ACCENT)),
        Span::styled(" editor  ", Style::default().fg(FAINT)),
        Span::styled("?", Style::default().fg(ACCENT)),
        Span::styled(" help  ", Style::default().fg(FAINT)),
        Span::styled("Esc", Style::default().fg(ACCENT)),
        Span::styled(" quit", Style::default().fg(FAINT)),
    ]);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── Help overlay ─────────────────────────────────────────────────────

fn render_help(f: &mut Frame, area: Rect, app: &App) {
    // Generous modal sizing — the key list grew with history / filters /
    // Mac-friendly aliases, so reserve up to 40 rows when the terminal
    // allows it. Content beyond the viewport is reachable via scroll.
    let w = 72u16.min(area.width.saturating_sub(4));
    let h = 40u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    f.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER_FOCUS))
        .padding(Padding::horizontal(2))
        .title_top(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Keybindings",
                Style::default().fg(TITLE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]))
        .title_bottom(Line::from(vec![
            Span::raw(" "),
            Span::styled("↑↓ / j k", Style::default().fg(ACCENT)),
            Span::styled(" scroll  ", Style::default().fg(FAINT)),
            Span::styled("any other key", Style::default().fg(ACCENT)),
            Span::styled(" dismiss ", Style::default().fg(FAINT)),
        ]));
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    let lines = vec![
        Line::from(""),
        section("Navigation"),
        kbd("↑ / ↓", "select prev / next result"),
        kbd("Ctrl-P / Ctrl-N", "select prev / next result (readline)"),
        kbd("PgUp / PgDn", "jump 10 results"),
        Line::from(""),
        section("Preview pane"),
        kbd("Shift-↑ / Shift-↓", "scroll one line (primary)"),
        kbd("Shift-PgUp / Shift-PgDn", "scroll 10 lines (primary)"),
        kbd("F5 / F6", "scroll one line (Mac fallback)"),
        kbd("F7 / F8", "scroll 10 lines (Mac fallback)"),
        Line::from(""),
        section("Search mode"),
        kbd("Tab / Shift-Tab", "cycle hybrid · bm25 · semantic"),
        Line::from(""),
        section("Actions"),
        kbd("Enter", "print path:line to stdout, then quit"),
        kbd("Ctrl-O", "open file in $EDITOR ($VISUAL > $EDITOR > vi)"),
        kbd(
            "Ctrl-R",
            "semantically similar chunks (struct→struct, fn→fn)",
        ),
        kbd("Ctrl-D", "defs of typed query, or selected symbol if empty"),
        kbd("Ctrl-F", "defs + refs (where is this used?)"),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Tip: Ctrl-R is structural similarity (small embedding",
                Style::default().fg(FAINT),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "model). For \"where is X used?\" prefer Ctrl-F.",
                Style::default().fg(FAINT),
            ),
        ]),
        Line::from(""),
        section("History (back / forward)"),
        kbd("Ctrl-B / Ctrl-X", "back / forward — works on all platforms"),
        kbd("F2 / F3", "back / forward (Linux; macOS needs Fn keys on)"),
        kbd("Alt-← / Alt-→", "back / forward (Linux + Mac w/ Meta)"),
        kbd("Alt-h / Alt-l", "back / forward (vim-style)"),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "macOS tip: ",
                Style::default().fg(FAINT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Use Ctrl-B / Ctrl-X — they work without any setup.",
                Style::default().fg(FAINT),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "(Alt-* needs Terminal → \"Use Option as Meta\";",
                Style::default().fg(FAINT),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                " F-keys need System Settings → Keyboard → F-as-standard.)",
                Style::default().fg(FAINT),
            ),
        ]),
        Line::from(""),
        section("Filters"),
        kbd("Ctrl-T", "cycle language filter (none → rust → ...)"),
        kbd("Ctrl-Y", "edit path glob (Esc/Enter applies)"),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Cycle only visits languages present in the index. Pass",
                Style::default().fg(FAINT),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "--include-text-files at startup to also index .md/.toml/etc.",
                Style::default().fg(FAINT),
            ),
        ]),
        Line::from(""),
        section("Query editing"),
        kbd("type / Backspace / Delete", "edit query at cursor"),
        kbd("← / →", "move cursor one character"),
        kbd("Ctrl-← / Ctrl-→", "move cursor one word"),
        kbd("Ctrl-A / Home · Ctrl-E / End", "start / end of line"),
        kbd("Ctrl-W", "delete word backward"),
        kbd("Ctrl-U", "clear query"),
        kbd("Ctrl-K", "kill to end of line"),
        kbd("? (when query empty)", "open this help panel"),
        Line::from(""),
        section("Query history (recall)"),
        kbd("Ctrl-↑ / Ctrl-↓", "previous / next recalled query"),
        kbd(
            "Alt-P / Alt-N",
            "same, for terminals that swallow Ctrl+Arrow",
        ),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Queries are recorded at Enter, Ctrl-O, Ctrl-R/D/F, Ctrl-U.",
                Style::default().fg(FAINT),
            ),
        ]),
        Line::from(""),
        section("Help panel"),
        kbd("↑ / ↓ · j / k", "scroll one line"),
        kbd("PgUp / PgDn", "scroll 10 lines"),
        kbd("Home", "back to top"),
        kbd("any other key", "dismiss"),
        Line::from(""),
        section("Quit / cancel"),
        kbd("Esc · Ctrl-C", "quit the TUI"),
        kbd("Ctrl-G", "cancel in-flight search (readline convention)"),
        Line::from(""),
        section("Diagnostics"),
        kbd(
            "veles tui --debug-keys",
            "echo every keypress to the status line",
        ),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Use this to verify which keys your terminal forwards —",
                Style::default().fg(FAINT),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "helpful when Alt-* or F-keys don't seem to fire.",
                Style::default().fg(FAINT),
            ),
        ]),
    ];
    // Clamp scroll so the user can't pan past the last line — Paragraph
    // would happily render off the bottom otherwise.
    let total = lines.len() as u16;
    let viewport = inner.height;
    let max_scroll = total.saturating_sub(viewport);
    let scroll = app.help_scroll.min(max_scroll);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        inner,
    );
}

fn section(label: &str) -> Line<'static> {
    Line::from(vec![Span::styled(
        label.to_string(),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )])
}

fn kbd(keys: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {keys:<28}"),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc.to_string(), Style::default().fg(FAINT)),
    ])
}

// ── Helpers ──────────────────────────────────────────────────────────

fn format_preview_line(
    ln: usize,
    content: &str,
    in_chunk: bool,
    _terms: &[String],
    lower_terms: &[String],
) -> Line<'static> {
    let line_no = format!("{ln:>5} ");
    let gutter_style = if in_chunk {
        Style::default().fg(ACCENT).bg(CHUNK_BG)
    } else {
        Style::default().fg(DIM)
    };
    let bar = "│ ";
    let bar_style = if in_chunk {
        Style::default().fg(ACCENT).bg(CHUNK_BG)
    } else {
        Style::default().fg(DIM)
    };

    let body_normal = if in_chunk {
        Style::default().fg(TEXT).bg(CHUNK_BG)
    } else {
        Style::default().fg(FAINT)
    };
    let body_match = if in_chunk {
        Style::default()
            .fg(ACCENT)
            .bg(CHUNK_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    };

    let mut spans = vec![
        Span::styled(line_no, gutter_style),
        Span::styled(bar.to_string(), bar_style),
    ];
    spans.extend(highlight_terms(
        content,
        lower_terms,
        body_normal,
        body_match,
    ));
    Line::from(spans)
}

/// Highlight the lowercased `lower_terms` inside `content`, splitting it
/// into a sequence of normal / matched spans. The terms must already be
/// lowercased by the caller (see `App::cached_lower_terms`) so this hot
/// path doesn't allocate one Vec<String> per visible preview line.
fn highlight_terms(
    content: &str,
    lower_terms: &[String],
    normal: Style,
    matched: Style,
) -> Vec<Span<'static>> {
    if lower_terms.is_empty() || content.is_empty() {
        return vec![Span::styled(content.to_string(), normal)];
    }
    let lower = content.to_ascii_lowercase();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < content.len() {
        // Find the earliest match starting at >= i.
        let mut earliest: Option<(usize, usize)> = None;
        for term in lower_terms {
            if let Some(rel) = lower[i..].find(term.as_str()) {
                let start = i + rel;
                let end = start + term.len();
                earliest = match earliest {
                    Some((s, _)) if start >= s => earliest,
                    _ => Some((start, end)),
                };
            }
        }
        match earliest {
            Some((s, e)) => {
                if s > i {
                    spans.push(Span::styled(content[i..s].to_string(), normal));
                }
                spans.push(Span::styled(content[s..e].to_string(), matched));
                i = e;
            }
            None => {
                spans.push(Span::styled(content[i..].to_string(), normal));
                break;
            }
        }
    }
    spans
}

fn first_nonblank_line(content: &str) -> &str {
    for line in content.lines() {
        let t = line.trim();
        if !t.is_empty() {
            return t;
        }
    }
    ""
}

fn truncate(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut acc = String::new();
    let mut cols = 0usize;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if cols + w > max_cols {
            if max_cols >= 1 {
                // Replace last char with ellipsis if there's room.
                acc.pop();
                acc.push('…');
            }
            return acc;
        }
        acc.push(c);
        cols += w;
    }
    acc
}

fn pad_or_truncate(s: &str, width: usize) -> String {
    let cols = UnicodeWidthStr::width(s);
    if cols == width {
        s.to_string()
    } else if cols < width {
        let mut out = s.to_string();
        out.push_str(&" ".repeat(width - cols));
        out
    } else {
        // Truncate with ellipsis from the LEFT — paths are more
        // distinguishable by their tail (file name) than their head.
        //
        // Single forward pass: scan char-indices left→right, keeping
        // the rightmost byte index that leaves room for the ellipsis
        // plus the visible suffix. The previous version reverse-iter'd
        // chars into a Vec and pushed them back — two passes and a
        // heap allocation that this loop avoids entirely.
        let mut acc = String::with_capacity(width + 1);
        let mut suffix_byte_start = s.len();
        let mut tail_cols = 0usize;
        let budget = width.saturating_sub(1); // reserve 1 col for `…`
        for (i, c) in s.char_indices().rev() {
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if tail_cols + w > budget {
                break;
            }
            tail_cols += w;
            suffix_byte_start = i;
        }
        acc.push('…');
        acc.push_str(&s[suffix_byte_start..]);
        acc
    }
}

fn visual_width_chars(s: &str, char_idx: usize) -> usize {
    s.chars()
        .take(char_idx)
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

fn spinner_frame(tick: u64) -> &'static str {
    SPINNER[(tick as usize) % SPINNER.len()]
}

fn mode_color(mode: SearchMode) -> Color {
    match mode {
        SearchMode::Hybrid => Color::Rgb(125, 207, 255),
        SearchMode::Bm25 => Color::Rgb(255, 200, 80),
        SearchMode::Semantic => Color::Rgb(200, 130, 255),
        SearchMode::Regex => Color::Rgb(120, 255, 160),
    }
}

fn mode_label(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Hybrid => "hybrid",
        SearchMode::Bm25 => "bm25",
        SearchMode::Semantic => "semantic",
        SearchMode::Regex => "regex",
    }
}

/// Pick a colour for a score against the current result set's maximum.
///
/// Relative thresholds: top ~70% gets HIGH, top ~40% MID, the rest
/// FAINT. The previous absolute thresholds (`0.7` / `0.4`) made every
/// hybrid result render FAINT — hybrid is RRF-blended with a max of
/// ~0.02 so it never crossed the bar. This relative scheme works for
/// every search mode without per-mode dispatch.
///
/// Falls back to FAINT when `max_score` isn't finite/positive (empty
/// or all-zero result set).
fn score_color(score: f64, max_score: f64) -> Color {
    if !max_score.is_finite() || max_score <= 0.0 {
        return FAINT;
    }
    let r = score / max_score;
    if r >= 0.70 {
        HIGH
    } else if r >= 0.40 {
        MID
    } else {
        FAINT
    }
}
