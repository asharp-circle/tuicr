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

/// Dims all RGB colors in `buf` toward `bg`; non-RGB colors are left as-is.
pub fn dim_buffer(buf: &mut Buffer, bg: Color) {
    for cell in &mut buf.content {
        cell.fg = mix(cell.fg, bg, FG_KEEP_PERCENT);
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
    fn should_leave_non_rgb_colors_unchanged() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        buf[(0, 0)].set_style(Style::default().fg(Color::Red));
        dim_buffer(&mut buf, Color::Rgb(0, 0, 0));
        assert_eq!(buf[(0, 0)].fg, Color::Red);
    }
}
