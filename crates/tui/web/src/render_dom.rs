//! DOM renderer — converts TextSurface to HTML spans.
//!
//! Used by the web/WASM crate to render tile content as HTML.

use ryeos_tui_core::text_surface::{Attr, Color, TextSurface};

/// Convert a TextSurface to an HTML string.
pub fn generate_html(surface: &TextSurface) -> String {
    let mut html = String::with_capacity(surface.width * surface.height * 8);

    for y in 0..surface.height {
        // Find last non-empty cell in row
        let mut last_nonempty = surface.width;
        while last_nonempty > 0 {
            last_nonempty -= 1;
            if surface.get(last_nonempty, y).rune != ' ' {
                break;
            }
        }
        if last_nonempty == 0 && surface.get(0, y).rune == ' ' {
            html.push('\n');
            continue;
        }

        let mut x = 0;
        while x <= last_nonempty {
            let cell = surface.get(x, y);
            let style = CellStyle::from_cell(cell);

            // Find run length: consecutive cells with same style
            let mut run_end = x + 1;
            while run_end <= last_nonempty {
                let next_style = CellStyle::from_cell(surface.get(run_end, y));
                if next_style != style {
                    break;
                }
                run_end += 1;
            }

            // Emit <span>
            let css = style.to_css();
            if css.is_empty() {
                for cx in x..run_end {
                    html.push_str(&html_escape(surface.get(cx, y).rune));
                }
            } else {
                html.push_str("<span style=\"");
                html.push_str(&css);
                html.push_str("\">");
                for cx in x..run_end {
                    let rune = surface.get(cx, y).rune;
                    html.push_str(&html_escape(rune));
                }
                html.push_str("</span>");
            }
            x = run_end;
        }
        html.push('\n');
    }

    html
}

#[derive(Debug, Clone, PartialEq)]
struct CellStyle {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl CellStyle {
    fn from_cell(cell: ryeos_tui_core::text_surface::Cell) -> Self {
        Self {
            fg: if cell.fg != Color::Default {
                Some(cell.fg)
            } else {
                None
            },
            bg: if cell.bg != Color::Default {
                Some(cell.bg)
            } else {
                None
            },
            bold: cell.attr.contains(Attr::BOLD),
            italic: cell.attr.contains(Attr::ITALIC),
            underline: cell.attr.contains(Attr::UNDERLINE),
        }
    }

    fn to_css(&self) -> String {
        let mut css = String::new();
        if let Some(fg) = &self.fg {
            css.push_str("color:");
            css.push_str(&to_css_color(*fg));
            css.push(';');
        }
        if let Some(bg) = &self.bg {
            css.push_str("background-color:");
            css.push_str(&to_css_color(*bg));
            css.push(';');
        }
        if self.bold {
            css.push_str("font-weight:bold;");
        }
        if self.italic {
            css.push_str("font-style:italic;");
        }
        if self.underline {
            css.push_str("text-decoration:underline;");
        }
        css
    }
}

pub fn to_css_color(color: Color) -> String {
    match color {
        Color::Default => "inherit".into(),
        Color::Index(i) => format!("var(--ansi{})", i),
        Color::Rgb(r, g, b) => format!("#{:02x}{:02x}{:02x}", r, g, b),
    }
}

fn html_escape(ch: char) -> String {
    match ch {
        '<' => "&lt;".into(),
        '>' => "&gt;".into(),
        '&' => "&amp;".into(),
        '"' => "&quot;".into(),
        '\u{0}' => String::new(), // continuation cell
        _ => ch.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_tui_core::text_surface::{Color, Style};

    #[test]
    fn html_generation_basic_text() {
        let mut surface = TextSurface::new(10, 1);
        surface.draw_text(0, 0, "hello", Style::new().fg(Color::Rgb(0xff, 0xff, 0xff)));

        let html = generate_html(&surface);
        assert!(html.contains("hello"));
        assert!(html.contains("color:#ffffff"));
    }

    #[test]
    fn html_generation_escapes_special_chars() {
        let mut surface = TextSurface::new(10, 1);
        surface.draw_text(0, 0, "<&>", Style::new());

        let html = generate_html(&surface);
        assert!(html.contains("&lt;&amp;&gt;"));
    }

    #[test]
    fn html_generation_merges_same_style_runs() {
        let mut surface = TextSurface::new(10, 1);
        let style = Style::new().fg(Color::Rgb(0xff, 0x00, 0x00));
        surface.draw_text(0, 0, "abc", style);

        let html = generate_html(&surface);
        // Should be a single span, not three
        assert_eq!(html.matches("<span").count(), 1);
    }

    #[test]
    fn css_color_formats() {
        assert_eq!(to_css_color(Color::Default), "inherit");
        assert_eq!(to_css_color(Color::Rgb(0xfe, 0x80, 0x19)), "#fe8019");
        assert!(to_css_color(Color::Index(42)).starts_with("var(--ansi"));
    }
}
