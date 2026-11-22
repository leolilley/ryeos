//! `views.rs` holds no layout number of its own: every measure it lays a view
//! out with is a field of the shared layout table in `ryeos-client-base`.
//!
//! The same check as a command, from `crates/clients/rune/src`, lists the
//! numeric literals in the file's production half (everything before its test
//! module) other than 0, 1 and 2, which only ever appear as structural values
//! (a zero minimum, a one-part grow weight, a hairline, a halving):
//!
//! ```text
//! sed '/^#\[cfg(test)\]/,$d' views.rs | sed 's|//.*||' \
//!     | grep -nE '(^|[^A-Za-z_0-9.])[0-9]+(\.[0-9]+)?' | grep -vE '(^|[^0-9.])[012](\.0)?([^0-9.]|$)'
//! ```
//!
//! This test is that scan, kept honest by the compiler's own tokenising rules:
//! comments and string and char literals are skipped, and a digit run that
//! continues an identifier or follows a `.` (a field index) is not a literal.

const VIEWS: &str = include_str!("../src/views.rs");

/// The numbers that are structure, not measure.
const STRUCTURAL: [&str; 6] = ["0", "1", "2", "0.0", "1.0", "2.0"];

/// Every numeric literal in `source`, with its 1-based line.
fn literals(source: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = source.chars().collect();
    let mut found = Vec::new();
    let mut line = 1;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                if chars.get(i) == Some(&'\n') {
                    line += 1;
                }
                i += 1;
            }
            i += 1;
        } else if c == '\'' && chars.get(i + 2) == Some(&'\'') {
            i += 3;
        } else if c.is_ascii_digit() {
            let previous = if i == 0 { ' ' } else { chars[i - 1] };
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let mut end = i;
            // A fraction: `.` then digits (not a method call or a range).
            if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(char::is_ascii_digit) {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                end = i;
            }
            let continues_identifier = previous.is_ascii_alphanumeric() || previous == '_';
            if !continues_identifier && previous != '.' {
                found.push((line, chars[start..end].iter().collect()));
            }
        } else if c.is_ascii_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    found
}

#[test]
fn views_rs_holds_no_layout_literal() {
    let production = VIEWS
        .split("\n#[cfg(test)]")
        .next()
        .expect("views.rs has a production half");
    let stray: Vec<String> = literals(production)
        .into_iter()
        .filter(|(_, literal)| !STRUCTURAL.contains(&literal.as_str()))
        .map(|(line, literal)| format!("views.rs:{line}: {literal}"))
        .collect();
    assert!(
        stray.is_empty(),
        "layout numbers belong in ryeos_client_base::layout_table:\n{}",
        stray.join("\n")
    );
}

#[test]
fn the_scan_sees_numbers_and_skips_names_comments_and_strings() {
    let source = "let a = x.gap(12.0); // 99\nlet b = \"7\"; let s9 = p.0; let c = '8'; f32 + 3";
    let seen: Vec<String> = literals(source).into_iter().map(|(_, l)| l).collect();
    assert_eq!(seen, ["12.0", "3"]);
}
