//! Text surface — 2D cell buffer for tile content.
//!
//! Pure core data structure. Terminal and web adapters render from this.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Color
// ---------------------------------------------------------------------------

/// Platform-neutral color. Renderers convert to ANSI or CSS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    Default,
    Index(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    pub fn is_default(self) -> bool {
        matches!(self, Color::Default)
    }

    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::Rgb(r, g, b)
    }

    pub fn index(i: u8) -> Self {
        Self::Index(i)
    }
}

// ---------------------------------------------------------------------------
// Attr
// ---------------------------------------------------------------------------

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Attr: u8 {
        const BOLD      = 0b0000_0001;
        const ITALIC    = 0b0000_0010;
        const UNDERLINE = 0b0000_0100;
        const REVERSE   = 0b0000_1000;
    }
}

impl Default for Attr {
    fn default() -> Self {
        Self::empty()
    }
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attr: Attr,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            attr: Attr::empty(),
        }
    }
}

impl Style {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fg(mut self, color: Color) -> Self {
        self.fg = color;
        self
    }

    pub fn bg(mut self, color: Color) -> Self {
        self.bg = color;
        self
    }

    pub fn bold(mut self) -> Self {
        self.attr |= Attr::BOLD;
        self
    }

    pub fn italic(mut self) -> Self {
        self.attr |= Attr::ITALIC;
        self
    }

    pub fn underline(mut self) -> Self {
        self.attr |= Attr::UNDERLINE;
        self
    }

    pub fn reverse(mut self) -> Self {
        self.attr |= Attr::REVERSE;
        self
    }
}

// ---------------------------------------------------------------------------
// Cell
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub rune: char,
    pub width: u8,
    pub fg: Color,
    pub bg: Color,
    pub attr: Attr,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            rune: ' ',
            width: 1,
            fg: Color::Default,
            bg: Color::Default,
            attr: Attr::empty(),
        }
    }
}

// ---------------------------------------------------------------------------
// Border
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Border {
    None,
    Rounded,
    Sharp,
    Double,
    Thick,
    Ascii,
}

#[derive(Debug, Clone, Copy)]
pub struct BorderChars {
    pub tl: char,
    pub tr: char,
    pub bl: char,
    pub br: char,
    pub h: char,
    pub v: char,
}

pub fn border_chars(style: Border) -> BorderChars {
    match style {
        Border::None => BorderChars {
            tl: ' ',
            tr: ' ',
            bl: ' ',
            br: ' ',
            h: ' ',
            v: ' ',
        },
        Border::Rounded => BorderChars {
            tl: '\u{256D}', // ╭
            tr: '\u{256E}', // ╮
            bl: '\u{2570}', // ╰
            br: '\u{256F}', // ╯
            h: '\u{2500}',  // ─
            v: '\u{2502}',  // │
        },
        Border::Sharp => BorderChars {
            tl: '\u{250C}', // ┌
            tr: '\u{2510}', // ┐
            bl: '\u{2514}', // └
            br: '\u{2518}', // ┘
            h: '\u{2500}',  // ─
            v: '\u{2502}',  // │
        },
        Border::Double => BorderChars {
            tl: '\u{2554}', // ╔
            tr: '\u{2557}', // ╗
            bl: '\u{255A}', // ╚
            br: '\u{255D}', // ╝
            h: '\u{2550}',  // ═
            v: '\u{2551}',  // ║
        },
        Border::Thick => BorderChars {
            tl: '\u{250F}', // ┏
            tr: '\u{2513}', // ┓
            bl: '\u{2517}', // ┗
            br: '\u{251B}', // ┛
            h: '\u{2501}',  // ━
            v: '\u{2503}',  // ┃
        },
        Border::Ascii => BorderChars {
            tl: '+',
            tr: '+',
            bl: '+',
            br: '+',
            h: '-',
            v: '|',
        },
    }
}

// ---------------------------------------------------------------------------
// TextSurface
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextSurface {
    pub cells: Vec<Cell>,
    pub width: usize,
    pub height: usize,
}

impl TextSurface {
    pub fn new(width: usize, height: usize) -> Self {
        let cells = vec![Cell::default(); width * height];
        Self {
            cells,
            width,
            height,
        }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        let mut new_cells = vec![Cell::default(); width * height];
        let copy_w = self.width.min(width);
        let copy_h = self.height.min(height);
        for y in 0..copy_h {
            for x in 0..copy_w {
                new_cells[y * width + x] = self.cells[y * self.width + x];
            }
        }
        self.cells = new_cells;
        self.width = width;
        self.height = height;
    }

    pub fn clear(&mut self) {
        for cell in self.cells.iter_mut() {
            *cell = Cell::default();
        }
    }

    pub fn get(&self, x: usize, y: usize) -> Cell {
        if x >= self.width || y >= self.height {
            return Cell::default();
        }
        self.cells[y * self.width + x]
    }

    pub fn set(&mut self, x: usize, y: usize, cell: Cell) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = cell;
        }
    }

    /// Draw a character at (x, y) with the given style. Returns display width.
    pub fn draw_char(&mut self, x: usize, y: usize, ch: char, style: Style) -> u8 {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1) as u8;
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = Cell {
                rune: ch,
                width: w,
                fg: style.fg,
                bg: style.bg,
                attr: style.attr,
            };
        }
        // Mark continuation cell for wide characters
        if w == 2 && x + 1 < self.width && y < self.height {
            self.cells[y * self.width + x + 1] = Cell {
                rune: '\u{0}',
                width: 0,
                fg: style.fg,
                bg: style.bg,
                attr: style.attr,
            };
        }
        w
    }

    /// Draw a string starting at (x, y). Returns columns consumed.
    pub fn draw_text(&mut self, x: usize, y: usize, text: &str, style: Style) -> usize {
        let mut col = x;
        for ch in text.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            if col + w > self.width {
                break;
            }
            self.draw_char(col, y, ch, style);
            col += w;
        }
        col.saturating_sub(x)
    }

    /// Draw a horizontal line.
    pub fn draw_hline(&mut self, x1: usize, y: usize, x2: usize, ch: char, style: Style) {
        for x in x1..=x2.min(self.width.saturating_sub(1)) {
            self.draw_char(x, y, ch, style);
        }
    }

    /// Draw a vertical line.
    pub fn draw_vline(&mut self, x: usize, y1: usize, y2: usize, ch: char, style: Style) {
        for y in y1..=y2.min(self.height.saturating_sub(1)) {
            self.draw_char(x, y, ch, style);
        }
    }

    /// Draw a box with the given border style.
    pub fn draw_box(
        &mut self,
        x1: usize,
        y1: usize,
        x2: usize,
        y2: usize,
        border: Border,
        style: Style,
    ) {
        let bc = border_chars(border);
        let x2 = x2.min(self.width.saturating_sub(1));
        let y2 = y2.min(self.height.saturating_sub(1));
        if x2 <= x1 || y2 <= y1 {
            return;
        }

        self.draw_char(x1, y1, bc.tl, style);
        self.draw_char(x2, y1, bc.tr, style);
        self.draw_char(x1, y2, bc.bl, style);
        self.draw_char(x2, y2, bc.br, style);
        self.draw_hline(x1 + 1, y1, x2 - 1, bc.h, style);
        self.draw_hline(x1 + 1, y2, x2 - 1, bc.h, style);
        self.draw_vline(x1, y1 + 1, y2 - 1, bc.v, style);
        self.draw_vline(x2, y1 + 1, y2 - 1, bc.v, style);
    }

    /// Fill a rectangular region.
    pub fn fill_rect(
        &mut self,
        x1: usize,
        y1: usize,
        x2: usize,
        y2: usize,
        ch: char,
        style: Style,
    ) {
        for y in y1..=y2.min(self.height.saturating_sub(1)) {
            for x in x1..=x2.min(self.width.saturating_sub(1)) {
                self.draw_char(x, y, ch, style);
            }
        }
    }

    /// Fill entire surface with a background style.
    pub fn fill(&mut self, style: Style) {
        for cell in self.cells.iter_mut() {
            cell.bg = style.bg;
            cell.fg = style.fg;
            cell.attr = style.attr;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_surface_draws_ascii() {
        let mut s = TextSurface::new(20, 1);
        let style = Style::new().fg(Color::rgb(255, 255, 255));
        let consumed = s.draw_text(0, 0, "hello", style);
        assert_eq!(consumed, 5);
        assert_eq!(s.get(0, 0).rune, 'h');
        assert_eq!(s.get(4, 0).rune, 'o');
    }

    #[test]
    fn text_surface_draws_wide_chars() {
        let mut s = TextSurface::new(20, 1);
        let style = Style::new();
        let consumed = s.draw_text(0, 0, "你好", style);
        assert_eq!(consumed, 4); // 2 wide chars × 2 columns each
        assert_eq!(s.get(0, 0).width, 2);
        assert_eq!(s.get(1, 0).width, 0); // continuation cell
    }

    #[test]
    fn text_surface_draws_rounded_box() {
        let mut s = TextSurface::new(10, 5);
        s.draw_box(0, 0, 9, 4, Border::Rounded, Style::new());
        assert_eq!(s.get(0, 0).rune, '╭');
        assert_eq!(s.get(9, 0).rune, '╮');
        assert_eq!(s.get(0, 4).rune, '╰');
        assert_eq!(s.get(9, 4).rune, '╯');
        assert_eq!(s.get(5, 0).rune, '─');
        assert_eq!(s.get(0, 2).rune, '│');
    }

    #[test]
    fn text_surface_clips_overflow() {
        let mut s = TextSurface::new(5, 1);
        let consumed = s.draw_text(0, 0, "hello world", Style::new());
        assert_eq!(consumed, 5); // clips at width
    }

    #[test]
    fn text_surface_resize_preserves_content() {
        let mut s = TextSurface::new(10, 2);
        s.draw_text(0, 0, "abc", Style::new());
        s.resize(20, 4);
        assert_eq!(s.width, 20);
        assert_eq!(s.height, 4);
        assert_eq!(s.get(0, 0).rune, 'a');
    }

    #[test]
    fn fill_rect_sets_cells() {
        let mut s = TextSurface::new(5, 3);
        s.fill_rect(1, 1, 3, 2, 'x', Style::new());
        assert_eq!(s.get(0, 0).rune, ' ');
        assert_eq!(s.get(2, 1).rune, 'x');
        assert_eq!(s.get(4, 2).rune, ' ');
    }
}
