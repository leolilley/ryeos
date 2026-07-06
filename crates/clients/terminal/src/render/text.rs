//! Width-aware string shaping. All width math here is display columns
//! (unicode-width), never chars or bytes — the surface advances by
//! rendered width, so anything else bleeds through tile borders.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

pub fn truncate(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(value) <= max_width {
        return value.to_string();
    }
    let mut text = String::new();
    let mut width = 0;
    let target = max_width.saturating_sub(1);
    for ch in value.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > target {
            break;
        }
        text.push(ch);
        width += ch_width;
    }
    text.push('…');
    text
}

pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let word_w = display_width(word);
        if word_w > width {
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            out.extend(wrap_long_word(word, width));
            continue;
        }
        let line_w = display_width(&line);
        if line_w > 0 && line_w + 1 + word_w > width {
            out.push(line);
            line = String::new();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn wrap_long_word(word: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    let mut line_w = 0;
    for ch in word.chars() {
        let ch_w = ch.width().unwrap_or(0);
        if !line.is_empty() && line_w + ch_w > width {
            out.push(std::mem::take(&mut line));
            line_w = 0;
        }
        line.push(ch);
        line_w += ch_w;
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

pub fn join_with_right_meta(
    prefix: &str,
    primary: &str,
    meta: Option<&str>,
    width: usize,
) -> String {
    let left = format!("{prefix} {primary}");
    let Some(meta) = meta.filter(|m| !m.is_empty()) else {
        return left;
    };
    let left_w = display_width(&left);
    let meta_w = display_width(meta);
    if left_w + meta_w + 2 >= width {
        format!("{left} · {meta}")
    } else {
        format!("{left}{}{}", " ".repeat(width - left_w - meta_w), meta)
    }
}

pub fn letterspace(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| [ch.to_ascii_uppercase(), ' '])
        .collect::<String>()
        .trim_end()
        .to_string()
}

pub fn input_cursor_byte(value: &str, cursor: usize) -> usize {
    let mut byte = cursor.min(value.len());
    while byte > 0 && !value.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}
