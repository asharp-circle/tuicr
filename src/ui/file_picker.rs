//! Fuzzy file picker modal (`InputMode::FilePicker`).
//!
//! Renders an fzf-style fuzzy search popup listing all diff files in the
//! review. As the user types, matches update dynamically with matched
//! characters highlighted.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

use crate::app::App;
use crate::ui::styles;

const REVIEWED_BOX: &str = "\u{25a3}"; // ▣
const UNREVIEWED_BOX: &str = "\u{25a2}"; // ▢

pub fn render_file_picker(frame: &mut Frame, app: &mut App) {
    let theme = &app.theme;
    let area = centered_rect(65, 65, app.diff_area.unwrap_or(frame.area()));
    frame.render_widget(Clear, area);

    let match_count = app.file_picker.matches.len();
    let total_count = app.file_picker.candidates.len();
    let title = format!(" Files ({match_count}/{total_count}) ");

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .style(styles::popup_style(theme))
        .border_style(styles::border_style(theme, true));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [input_area, divider_area, list_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);

    // Input prompt: ` > <query>`
    let query = &app.file_picker.query;
    let input_line = Line::from(vec![
        Span::styled(
            " > ",
            Style::default()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(query, Style::default().fg(theme.fg_primary)),
    ]);
    frame.render_widget(Paragraph::new(input_line), input_area);

    // Thin separator line
    let divider = "─".repeat(divider_area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(divider, styles::dim_style(theme)))),
        divider_area,
    );

    // File list
    if app.file_picker.matches.is_empty() {
        let msg = if app.file_picker.candidates.is_empty() {
            "  No files in review"
        } else {
            "  No matching files"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, styles::dim_style(theme)))),
            list_area,
        );
    } else {
        let items: Vec<ListItem> = app
            .file_picker
            .matches
            .iter()
            .map(|m| {
                let candidate = &app.file_picker.candidates[m.candidate_idx];
                let checkbox = if candidate.is_reviewed {
                    REVIEWED_BOX
                } else {
                    UNREVIEWED_BOX
                };
                let checkbox_style = if candidate.is_reviewed {
                    styles::reviewed_style(theme)
                } else {
                    styles::pending_style(theme)
                };

                let mut spans = vec![
                    Span::raw(" "),
                    Span::styled(format!("{checkbox} "), checkbox_style),
                ];

                if !app.is_pristine_mode {
                    let status = candidate.status.as_char();
                    spans.push(Span::styled(
                        format!("{status} "),
                        styles::file_status_style(theme, status),
                    ));
                }

                // Highlight matched characters in the path
                let path_chars: Vec<char> = candidate.path.chars().collect();
                let matched_set: std::collections::HashSet<usize> =
                    m.matched_indices.iter().copied().collect();

                let mut chunk = String::new();
                let mut chunk_is_match = false;

                for (idx, &ch) in path_chars.iter().enumerate() {
                    let is_match = matched_set.contains(&idx);
                    if idx == 0 {
                        chunk_is_match = is_match;
                        chunk.push(ch);
                    } else if is_match == chunk_is_match {
                        chunk.push(ch);
                    } else {
                        // Flush previous chunk
                        if chunk_is_match {
                            spans.push(Span::styled(
                                chunk.clone(),
                                Style::default()
                                    .fg(theme.fg_primary)
                                    .bg(theme.search_match_bg)
                                    .add_modifier(Modifier::BOLD),
                            ));
                        } else {
                            spans.push(Span::styled(
                                chunk.clone(),
                                Style::default().fg(theme.fg_secondary),
                            ));
                        }
                        chunk.clear();
                        chunk_is_match = is_match;
                        chunk.push(ch);
                    }
                }
                if !chunk.is_empty() {
                    if chunk_is_match {
                        spans.push(Span::styled(
                            chunk,
                            Style::default()
                                .fg(theme.fg_primary)
                                .bg(theme.search_match_bg)
                                .add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        spans.push(Span::styled(chunk, Style::default().fg(theme.fg_secondary)));
                    }
                }

                ListItem::new(Line::from(spans))
            })
            .collect();

        let list = List::new(items)
            .style(styles::panel_style(theme))
            .highlight_style(styles::selected_style(theme));
        frame.render_stateful_widget(list, list_area, &mut app.file_picker.list_state);
    }

    // Footer
    let footer = "↑/↓ or C-j/C-k move · ↵ jump · esc cancel";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default()
                .fg(theme.fg_secondary)
                .add_modifier(Modifier::DIM),
        ))),
        footer_area,
    );

    // Position terminal cursor in query input
    let cursor_x = input_area.x + 3 + unicode_width::UnicodeWidthStr::width(query.as_str()) as u16;
    frame.set_cursor_position(Position {
        x: cursor_x.min(input_area.x + input_area.width.saturating_sub(1)),
        y: input_area.y,
    });
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([Constraint::Percentage(percent_y)]).flex(Flex::Center);
    let horizontal = Layout::horizontal([Constraint::Percentage(percent_x)]).flex(Flex::Center);
    let [vertical_area] = vertical.areas(area);
    let [centered] = horizontal.areas(vertical_area);
    centered
}
