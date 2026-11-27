//! Grid → ratatui `Buffer` drawing for the mirror backend.
//!
//! Reads the local mirror's [`RenderableContent`] (an alacritty `Grid<Cell>`)
//! and paints it into a [`ratatui::buffer::Buffer`]. Colors come from
//! alacritty cell `Color` (vte ansi: Named / Spec(Rgb) / Indexed); Named and
//! Indexed map through a small color table (C1 scope), Spec passes RGB
//! straight through. Wide-char spacer cells are skipped, mirroring the GUI
//! renderer.

use alacritty_terminal::term::cell::Flags;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use term_view::RenderableContent;
use vte::ansi::{Color as AnsiColor, NamedColor};

/// Draw one terminal screen into `buf` at `area`. `area` may be smaller than
/// the grid; we clamp and let ratatui own empty space.
pub fn draw_grid(content: &RenderableContent, buf: &mut Buffer, area: Rect) {
    let rows = area.height as usize;
    let cols = area.width as usize;
    if rows == 0 || cols == 0 {
        return;
    }

    for indexed in content.grid.display_iter() {
        let line = indexed.point.line.0;
        if line < 0 || line as usize >= rows {
            continue; // off-screen scrollback area
        }
        let column = indexed.point.column.0 as usize;
        if column >= cols {
            continue;
        }
        let cell = &indexed.cell;

        // Wide-char spacer: the leading wide char already painted this cell.
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }

        let x = area.left() + column as u16;
        let y = area.top() + line as u16;
        let ch = cell.c;
        if ch == '\0' {
            continue;
        }
        let Some(bcell) = buf.cell_mut((x, y)) else { continue };

        let mut style = Style::default();
        style = style.fg(map_color(cell.fg));
        style = style.bg(map_color(cell.bg));
        if cell.flags.contains(Flags::INVERSE) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if cell.flags.intersects(Flags::ALL_UNDERLINES) {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if cell.flags.contains(Flags::BOLD) || cell.flags.contains(Flags::BOLD_ITALIC) {
            style = style.add_modifier(Modifier::BOLD);
        }
        if cell.flags.contains(Flags::DIM) || cell.flags.contains(Flags::DIM_BOLD) {
            style = style.add_modifier(Modifier::DIM);
        }
        if cell.flags.contains(Flags::ITALIC) {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if cell.flags.contains(Flags::STRIKEOUT) {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }

        bcell.set_symbol(&ch.to_string());
        bcell.set_style(style);
    }
}

/// Map a vte ANSI color to a ratatui color. Indexed colors use a minimal
/// 16-color ANSI table (C1; a real theme comes with the GUI parity work).
fn map_color(color: AnsiColor) -> Color {
    match color {
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Named(named) => match named {
            NamedColor::Black => Color::Black,
            NamedColor::Red => Color::Red,
            NamedColor::Green => Color::Green,
            NamedColor::Yellow => Color::Yellow,
            NamedColor::Blue => Color::Blue,
            NamedColor::Magenta => Color::Magenta,
            NamedColor::Cyan => Color::Cyan,
            NamedColor::White => Color::Gray,
            NamedColor::BrightBlack => Color::DarkGray,
            NamedColor::BrightRed => Color::LightRed,
            NamedColor::BrightGreen => Color::LightGreen,
            NamedColor::BrightYellow => Color::LightYellow,
            NamedColor::BrightBlue => Color::LightBlue,
            NamedColor::BrightMagenta => Color::LightMagenta,
            NamedColor::BrightCyan => Color::LightCyan,
            NamedColor::BrightWhite => Color::White,
            _ => Color::Reset,
        },
        AnsiColor::Indexed(i) => indexed_to_color(i),
    }
}

/// Minimal 16-color + 256-color mapping. Values < 16 use ANSI names;
/// 16–255 use an RGB ramp (256-color cube), good enough for C1.
fn indexed_to_color(i: u8) -> Color {
    if i < 16 {
        const BRIGHT: [Color; 8] = [
            Color::DarkGray,
            Color::LightRed,
            Color::LightGreen,
            Color::LightYellow,
            Color::LightBlue,
            Color::LightMagenta,
            Color::LightCyan,
            Color::White,
        ];
        const DIM: [Color; 8] = [
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::Gray,
        ];
        if i < 8 {
            DIM[i as usize]
        } else {
            BRIGHT[(i - 8) as usize]
        }
    } else if (16..232).contains(&i) {
        // 6x6x6 color cube.
        let i = i - 16;
        let r = (i / 36) % 6;
        let g = (i / 6) % 6;
        let b = i % 6;
        let chan = |v: u8| -> u8 { if v == 0 { 0 } else { 55 + v * 40 } };
        Color::Rgb(chan(r), chan(g), chan(b))
    } else {
        // Grayscale ramp 232..=255.
        let v = 8 + (i - 232) * 10;
        Color::Rgb(v, v, v)
    }
}
