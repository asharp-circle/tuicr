use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem},
};

use crate::app::App;
use crate::forge::remote_comments::GITHUB_REACTIONS;
use crate::ui::styles;

pub fn render_reaction_picker(frame: &mut Frame, app: &App) {
    let [area] = Layout::vertical([Constraint::Length(12)])
        .flex(Flex::Center)
        .areas(frame.area());
    let [area] = Layout::horizontal([Constraint::Length(34)])
        .flex(Flex::Center)
        .areas(area);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" React to comment ")
        .borders(Borders::ALL)
        .style(styles::popup_style(&app.theme));
    let existing = app.reaction_target.as_ref().and_then(|(_, id)| {
        app.forge_review_threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .find(|comment| &comment.id == id)
    });
    let items = GITHUB_REACTIONS
        .iter()
        .enumerate()
        .map(|(index, (content, emoji))| {
            let label = match *content {
                "THUMBS_UP" => "+1",
                "THUMBS_DOWN" => "-1",
                other => other,
            };
            let selected = index == app.reaction_cursor;
            let own = existing.is_some_and(|comment| {
                comment
                    .reactions
                    .iter()
                    .any(|reaction| reaction.content == *content && reaction.viewer_has_reacted)
            });
            let text = format!(
                "{} {emoji} {label}{}",
                if selected { "▶" } else { " " },
                if own { "  ✓" } else { "" }
            );
            ListItem::new(Line::styled(
                text,
                if selected {
                    Style::default()
                        .fg(app.theme.fg_primary)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(app.theme.fg_secondary)
                },
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items).block(block), area);
}
