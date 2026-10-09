//! Unfocused-pane dimming: blends every RGB cell color toward the panel background.

use ratatui::{buffer::Buffer, style::Color};

/// Percent of the original color kept for foregrounds.
const FG_KEEP_PERCENT: u16 = 55;
/// Percent of the original color kept for backgrounds.
const BG_KEEP_PERCENT: u16 = 40;

fn mix(color: Color, toward: Color, keep_percent: u16) -> Color {
    match (color, toward) {
        (Color::Rgb(r, g, b), Color::Rgb(tr, tg, tb)) => {
            let m = |c: u8, t: u8| {
                ((u16::from(c) * keep_percent + u16::from(t) * (100 - keep_percent)) / 100) as u8
            };
            Color::Rgb(m(r, tr), m(g, tg), m(b, tb))
        }
        _ => color,
    }
}

fn is_powerline_separator(symbol: &str) -> bool {
    symbol
        .chars()
        .next()
        .is_some_and(|c| ('\u{e0b0}'..='\u{e0bf}').contains(&c))
}

/// Dims all RGB colors in `buf` toward `bg`; non-RGB colors are left as-is.
pub fn dim_buffer(buf: &mut Buffer, bg: Color) {
    for cell in &mut buf.content {
        // Powerline separators paint a segment edge with their foreground, so
        // it must dim like a background or the triangle stays brighter than
        // the segment beside it.
        let fg_keep = if is_powerline_separator(cell.symbol()) {
            BG_KEEP_PERCENT
        } else {
            FG_KEEP_PERCENT
        };
        cell.fg = mix(cell.fg, bg, fg_keep);
        cell.bg = mix(cell.bg, bg, BG_KEEP_PERCENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{layout::Rect, style::Style};

    #[test]
    fn should_blend_rgb_cells_toward_background() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        buf[(0, 0)].set_style(
            Style::default()
                .fg(Color::Rgb(200, 200, 200))
                .bg(Color::Rgb(100, 100, 100)),
        );
        dim_buffer(&mut buf, Color::Rgb(0, 0, 0));
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(110, 110, 110));
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(40, 40, 40));
    }

    #[test]
    fn should_dim_powerline_separator_foreground_like_a_background() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        buf[(0, 0)].set_symbol("\u{e0b0}");
        buf[(0, 0)].set_style(Style::default().fg(Color::Rgb(100, 100, 100)));
        dim_buffer(&mut buf, Color::Rgb(0, 0, 0));
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(40, 40, 40));
    }

    #[test]
    fn should_leave_non_rgb_colors_unchanged() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        buf[(0, 0)].set_style(Style::default().fg(Color::Red));
        dim_buffer(&mut buf, Color::Rgb(0, 0, 0));
        assert_eq!(buf[(0, 0)].fg, Color::Red);
    }
}
