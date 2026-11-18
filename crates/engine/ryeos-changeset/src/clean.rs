//! Sanitising of diff text and paths.
//!
//! A diff is code written by an untrusted party and drawn in a terminal or a
//! window, so everything that could move a cursor, recolour the screen or
//! reorder what the reader sees is removed here, once, before any client
//! draws it.

/// Longest path, in characters, a change may name.
pub const MAX_PATH_CHARS: usize = 512;

/// Whether `c` is a bidirectional control or a Unicode line separator: either
/// can make drawn text differ from the text that is there.
fn is_reordering(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// The index after a terminal escape sequence that starts at `chars[at]` (an
/// escape character), or `None` when no complete sequence starts there.
///
/// Only a sequence that is properly terminated is recognised. An unterminated
/// one loses its escape character alone, so the text after it stays visible:
/// a reader must never be shown less than the file holds.
fn escape_end(chars: &[char], at: usize) -> Option<usize> {
    match chars.get(at + 1)? {
        // Control sequence: parameters and intermediates, then one final.
        '[' => {
            let mut index = at + 2;
            while let Some(c) = chars.get(index) {
                match c {
                    ' '..='?' => index += 1,
                    '@'..='~' => return Some(index + 1),
                    _ => return None,
                }
            }
            None
        }
        // String sequences end at a bell or at escape-backslash.
        ']' | 'P' | 'X' | '^' | '_' => {
            let mut index = at + 2;
            while let Some(c) = chars.get(index) {
                match c {
                    '\u{07}' => return Some(index + 1),
                    '\u{1b}' if chars.get(index + 1) == Some(&'\\') => return Some(index + 2),
                    _ => index += 1,
                }
            }
            None
        }
        _ => None,
    }
}

/// `text` without terminal escape sequences, control characters (a tab
/// stays), carriage returns, bidirectional controls and line separators, cut
/// at `cap` characters with an elision mark.
pub fn clean_line(text: &str, cap: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut kept = 0usize;
    let mut index = 0usize;
    while index < chars.len() {
        let c = chars[index];
        if c == '\u{1b}' {
            index = escape_end(&chars, index).unwrap_or(index + 1);
            continue;
        }
        index += 1;
        if (c.is_control() && c != '\t') || is_reordering(c) {
            continue;
        }
        if kept == cap {
            out.push('…');
            break;
        }
        out.push(c);
        kept += 1;
    }
    out
}

/// A relative path safe to show as text, or `None`.
///
/// The path is sanitised like a line, then refused when it is empty, longer
/// than [`MAX_PATH_CHARS`], absolute (a leading separator or a drive prefix)
/// or has a `..` segment. Both separators count, whatever the host.
pub fn clean_path(raw: &str) -> Option<String> {
    let path = clean_line(raw, MAX_PATH_CHARS + 1);
    let path = path.trim();
    if path.is_empty() || path.chars().count() > MAX_PATH_CHARS {
        return None;
    }
    if path.starts_with(['/', '\\']) {
        return None;
    }
    let mut leading = path.chars();
    if let (Some(drive), Some(':')) = (leading.next(), leading.next())
        && drive.is_ascii_alphabetic()
    {
        return None;
    }
    if path
        .split(['/', '\\'])
        .any(|segment| segment.trim() == "..")
    {
        return None;
    }
    Some(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminated_escape_sequences_are_removed_whole() {
        assert_eq!(clean_line("a\u{1b}[31mred\u{1b}[0m b", 100), "ared b");
        assert_eq!(
            clean_line("x\u{1b}]0;title\u{07}y\u{1b}]8;;http://h\u{1b}\\z", 100),
            "xyz"
        );
    }

    #[test]
    fn an_unterminated_escape_never_hides_the_text_after_it() {
        assert_eq!(clean_line("a\u{1b}]0;rm -rf", 100), "a]0;rm -rf");
        assert_eq!(clean_line("a\u{1b}[31", 100), "a[31");
        assert_eq!(clean_line("a\u{1b}c", 100), "ac");
    }

    #[test]
    fn controls_and_reordering_characters_are_dropped_and_tabs_stay() {
        assert_eq!(
            clean_line("a\u{0}b\rc\u{202e}d\u{2066}e\u{2028}f\tg\u{9b}h", 100),
            "abcdef\tgh"
        );
    }

    #[test]
    fn a_long_line_is_cut_with_an_elision_mark() {
        assert_eq!(clean_line("abcdef", 3), "abc…");
        assert_eq!(clean_line("abc", 3), "abc");
        // Dropped characters do not count toward the cap.
        assert_eq!(clean_line("a\u{0}b\u{0}c\u{0}", 3), "abc");
    }

    #[test]
    fn relative_paths_pass_and_everything_else_is_refused() {
        assert_eq!(clean_path("src/main.rs").as_deref(), Some("src/main.rs"));
        assert_eq!(clean_path("a/./b").as_deref(), Some("a/./b"));
        assert_eq!(clean_path("a..b/c..").as_deref(), Some("a..b/c.."));
        for refused in [
            "",
            "   ",
            "/etc/passwd",
            "\\\\host\\share",
            "C:\\Windows",
            "c:/x",
            "../x",
            "a/../../x",
            "a\\..\\x",
            "a/ .. /x",
            // Hidden characters cannot disguise a parent segment.
            ".\u{202e}./x",
            "a/.\u{0}./x",
        ] {
            assert_eq!(clean_path(refused), None, "{refused:?}");
        }
        assert_eq!(clean_path(&"p".repeat(MAX_PATH_CHARS + 1)), None);
        assert_eq!(
            clean_path("a\u{1b}[31mb\u{202e}.rs").as_deref(),
            Some("ab.rs")
        );
    }
}
