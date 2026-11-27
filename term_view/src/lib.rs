//! A viewport of daemon-provided cells. No SSH, PTY, terminal parser, account,
//! filesystem operations, license policy, or feature execution lives here.

use std::collections::VecDeque;
use std::sync::Arc;

use alacritty_terminal::grid::Grid;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use term_proto::{FrameColor, FrameCursorShape, FramePayload, FrameSpan, HistoryScrollStep};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputModifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
}

/// User intentions only. The mirror sends them to the daemon for execution.
#[derive(Debug)]
pub enum BackendCommand {
    Write(Vec<u8>),
    Scroll(i32, InputModifiers, Point),
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
}

#[derive(Debug)]
pub enum ViewOutput {
    Bytes(Vec<u8>),
    Scroll { delta: i32, modifiers: InputModifiers, point: Point },
    HistoryScroll(HistoryScrollStep),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub num_cols: u16,
    pub num_lines: u16,
}

pub struct RenderableContent {
    pub grid: Arc<Grid<Cell>>,
    pub terminal_size: TerminalSize,
    pub terminal_mode: TermMode,
    pub cursor_shape: CursorShape,
    pub cursor_blinking: bool,
    /// Scrollback is owned by the daemon; it is never allocated in this grid.
    pub history_size: usize,
    pub display_offset: usize,
    pub content_scrolled: u64,
    pub bell_count: u32,
    pub highlight: Vec<FrameSpan>,
    pub highlight_total: usize,
    pub current_match: Option<FrameSpan>,
    pub highlight_current: Option<usize>,
}

pub struct TerminalView {
    content: RenderableContent,
    output: VecDeque<ViewOutput>,
    title: Option<String>,
    exit_code: Option<i32>,
    /// Why the transport ended, when the daemon reported one (serial unplug).
    exit_reason: Option<String>,
    progress: (u8, u8),
}

impl TerminalView {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            content: RenderableContent {
                grid: Arc::new(Grid::new(rows as usize, cols as usize, 0)),
                terminal_size: TerminalSize { num_cols: cols, num_lines: rows },
                terminal_mode: TermMode::empty(),
                cursor_shape: CursorShape::Hidden,
                cursor_blinking: false,
                history_size: 0,
                display_offset: 0,
                content_scrolled: 0,
                bell_count: 0,
                highlight: Vec::new(),
                highlight_total: 0,
                current_match: None,
                highlight_current: None,
            },
            output: VecDeque::new(),
            title: None,
            exit_code: None,
            exit_reason: None,
            progress: (0, 0),
        }
    }

    /// Keep the locally requested dimensions while a daemon resize is in flight.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = TerminalSize { num_cols: cols.max(1), num_lines: rows.max(1) };
        if self.content.terminal_size == size { return; }
        let old = &self.content.grid;
        let mut grid = Grid::new(size.num_lines as usize, size.num_cols as usize, 0);
        for line in 0..size.num_lines.min(self.content.terminal_size.num_lines) {
            for col in 0..size.num_cols.min(self.content.terminal_size.num_cols) {
                let point = Point::new(Line(line as i32), Column(col as usize));
                grid[point] = old[point].clone();
            }
        }
        self.content.grid = Arc::new(grid);
        self.content.terminal_size = size;
        self.content.cursor_shape = CursorShape::Hidden;
    }

    /// Apply absolute damaged rows. Trailing cells of each row are blanked,
    /// including the spacer occupied by a CJK glyph replaced by a narrow glyph.
    pub fn apply_frame(&mut self, frame: &FramePayload) {
        let cols = self.content.terminal_size.num_cols as usize;
        let lines = self.content.terminal_size.num_lines as usize;
        let grid = Arc::make_mut(&mut self.content.grid);
        if frame.full {
            for line in 0..lines {
                for col in 0..cols {
                    grid[Point::new(Line(line as i32), Column(col))] = Cell::default();
                }
            }
        }
        for row in &frame.damage {
            if row.line < 0 || row.line as usize >= lines || row.line as u16 >= frame.screen_lines { continue; }
            for col in 0..cols {
                grid[Point::new(Line(row.line), Column(col))] = Cell::default();
            }
            let mut col = 0;
            for cell in &row.cells {
                if col >= cols.min(frame.cols as usize) { break; }
                let flags = Flags::from_bits_truncate(cell.flags);
                grid[Point::new(Line(row.line), Column(col))] = Cell {
                    c: cell.ch, fg: color(cell.fg), bg: color(cell.bg), flags, extra: None,
                };
                if flags.contains(Flags::WIDE_CHAR) {
                    if col + 1 < cols.min(frame.cols as usize) {
                        grid[Point::new(Line(row.line), Column(col + 1))] = Cell {
                            flags: Flags::WIDE_CHAR_SPACER, ..Cell::default()
                        };
                    }
                    col += 2;
                } else { col += 1; }
            }
        }
        self.content.cursor_shape = CursorShape::Hidden;
        if let Some((line, col)) = frame.cursor {
            if line >= 0 && (line as usize) < lines.min(frame.screen_lines as usize)
                && (col as usize) < cols.min(frame.cols as usize) {
                grid.cursor.point = Point::new(Line(line), Column(col as usize));
                self.content.cursor_shape = match frame.cursor_shape {
                    FrameCursorShape::Block => CursorShape::Block,
                    FrameCursorShape::Beam => CursorShape::Beam,
                    FrameCursorShape::Underline => CursorShape::Underline,
                    FrameCursorShape::HollowBlock => CursorShape::HollowBlock,
                    FrameCursorShape::Hidden => CursorShape::Hidden,
                };
            }
        }
        self.content.cursor_blinking = frame.cursor_blinking;
        self.content.terminal_mode = TermMode::from_bits_truncate(frame.mode);
        self.content.history_size = frame.history;
        self.content.display_offset = frame.display_offset;
        self.content.content_scrolled = frame.content_scrolled;
        self.content.highlight.clone_from(&frame.highlight);
        self.content.highlight_total = frame.highlight_total;
        self.content.current_match = frame.current_match;
        self.content.highlight_current = frame.highlight_current;
    }

    pub fn process_command(&mut self, command: BackendCommand) {
        let step = match command {
            BackendCommand::Write(bytes) => {
                if !bytes.is_empty() { self.output.push_back(ViewOutput::Bytes(bytes)); }
                return;
            }
            BackendCommand::Scroll(delta, modifiers, point) => {
                self.output.push_back(ViewOutput::Scroll { delta, modifiers, point });
                return;
            }
            BackendCommand::ScrollPageUp => HistoryScrollStep::PageUp,
            BackendCommand::ScrollPageDown => HistoryScrollStep::PageDown,
            BackendCommand::ScrollToTop => HistoryScrollStep::Top,
            BackendCommand::ScrollToBottom => HistoryScrollStep::Bottom,
        };
        self.output.push_back(ViewOutput::HistoryScroll(step));
    }

    pub fn next_output(&mut self) -> Option<ViewOutput> { self.output.pop_front() }
    pub fn bracketed_paste_active(&self) -> bool { self.content.terminal_mode.contains(TermMode::BRACKETED_PASTE) }
    pub fn sync(&mut self) -> &RenderableContent { &self.content }
    pub fn mark_dirty(&mut self) {} // Cells are applied synchronously, no reader thread.
    pub fn on_title(&mut self, title: String) { self.title = Some(title); }
    pub fn on_reset_title(&mut self) { self.title = None; }
    pub fn on_bell(&mut self) { self.content.bell_count = self.content.bell_count.wrapping_add(1); }
    pub fn on_progress(&mut self, state: u8, progress: u8) { self.progress = (state, progress); }
    pub fn set_exit_code(&mut self, code: Option<i32>) { self.exit_code = code; }
    pub fn set_exit_reason(&mut self, reason: Option<String>) { self.exit_reason = reason; }
    pub fn exit_reason(&self) -> Option<&str> { self.exit_reason.as_deref() }
}

fn color(value: FrameColor) -> Color {
    match value {
        FrameColor::Indexed(index) => Color::Indexed(index),
        FrameColor::Spec(rgb) => Color::Spec(Rgb { r: (rgb >> 16) as u8, g: (rgb >> 8) as u8, b: rgb as u8 }),
        FrameColor::Named(index) => Color::Named(match index {
            0 => NamedColor::Black, 1 => NamedColor::Red, 2 => NamedColor::Green,
            3 => NamedColor::Yellow, 4 => NamedColor::Blue, 5 => NamedColor::Magenta,
            6 => NamedColor::Cyan, 7 => NamedColor::White, 8 => NamedColor::BrightBlack,
            9 => NamedColor::BrightRed, 10 => NamedColor::BrightGreen, 11 => NamedColor::BrightYellow,
            12 => NamedColor::BrightBlue, 13 => NamedColor::BrightMagenta, 14 => NamedColor::BrightCyan,
            15 => NamedColor::BrightWhite, 257 => NamedColor::Background, 258 => NamedColor::Cursor,
            259 => NamedColor::DimBlack, 260 => NamedColor::DimRed, 261 => NamedColor::DimGreen,
            262 => NamedColor::DimYellow, 263 => NamedColor::DimBlue, 264 => NamedColor::DimMagenta,
            265 => NamedColor::DimCyan, 266 => NamedColor::DimWhite, 267 => NamedColor::BrightForeground,
            268 => NamedColor::DimForeground, _ => NamedColor::Foreground,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Dimensions;
    use term_proto::{FrameCell, FrameRow};

    fn frame() -> FramePayload {
        FramePayload {
            cols: 8, screen_lines: 2, history: 500, display_offset: 13, content_scrolled: 55,
            cursor: Some((0, 3)), cursor_shape: FrameCursorShape::Beam, cursor_blinking: true,
            mode: TermMode::BRACKETED_PASTE.bits(), full: true, damage: Vec::new(),
            highlight: Vec::new(), highlight_total: 0, current_match: None, highlight_current: None,
            blocks: Vec::new(), graphics_added: Vec::new(), graphics_dropped: Vec::new(),
        }
    }
    fn cell(ch: char, flags: Flags) -> FrameCell {
        FrameCell { ch, fg: FrameColor::Spec(0x123456), bg: FrameColor::Named(257), flags: flags.bits(), graphic: None }
    }

    #[test]
    fn damage_replaces_wide_cells_and_preserves_other_rows() {
        let mut view = TerminalView::new(8, 2);
        let mut f = frame();
        f.damage = vec![FrameRow { line: 0, cells: vec![cell('中', Flags::WIDE_CHAR | Flags::BOLD), cell('A', Flags::ITALIC)] }, FrameRow { line: 1, cells: vec![cell('B', Flags::empty())] }];
        view.apply_frame(&f);
        let snapshot = view.sync().grid.clone();
        assert_eq!(snapshot[Point::new(Line(0), Column(2))].c, 'A');
        assert!(snapshot[Point::new(Line(0), Column(1))].flags.contains(Flags::WIDE_CHAR_SPACER));
        f.full = false;
        f.damage = vec![FrameRow { line: 0, cells: vec![cell('X', Flags::empty())] }];
        view.apply_frame(&f);
        let content = view.sync();
        assert_eq!(content.grid[Point::new(Line(0), Column(1))].c, ' ');
        assert_eq!(content.grid[Point::new(Line(1), Column(0))].c, 'B');
        assert_eq!(snapshot[Point::new(Line(0), Column(0))].c, '中', "render snapshots are immutable");
        assert_eq!((content.history_size, content.display_offset), (500, 13));
        assert_eq!(content.grid.history_size(), 0, "scrollback remains daemon-side");
    }

    #[test]
    fn frame_bounds_cursor_modes_and_resize_race() {
        let mut view = TerminalView::new(8, 2);
        let mut f = frame();
        f.damage = vec![FrameRow { line: -1, cells: vec![cell('!', Flags::empty())] }, FrameRow { line: 0, cells: vec![cell('Y', Flags::UNDERLINE)] }];
        view.apply_frame(&f);
        assert!(view.bracketed_paste_active());
        assert_eq!(view.sync().cursor_shape, CursorShape::Beam);
        view.resize(12, 4);
        view.apply_frame(&f);
        assert_eq!(view.sync().terminal_size.num_cols, 12, "old frame cannot undo requested resize");
        f.cursor = None;
        f.damage.clear();
        view.apply_frame(&f);
        assert_eq!(view.sync().cursor_shape, CursorShape::Hidden);
        assert_eq!(view.sync().grid[Point::new(Line(0), Column(0))].c, ' ');
    }
}
