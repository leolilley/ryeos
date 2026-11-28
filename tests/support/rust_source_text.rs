//! The production text of a Rust source file, for the source guards.
//!
//! Included by path (`#[path = ".../tests/support/rust_source_text.rs"] mod
//! rust_source_text;`) from every guard that scans source text, so they all
//! cut test code the same way instead of each guessing at it.
//!
//! A guard helper may show too much, never too little: a line of production
//! code that the helper hides is a line no guard will ever check. So the scan
//! reads strings, raw strings, characters, lifetimes and comments before it
//! counts a brace, blanks only what `#[cfg(test)]` really attributes, and
//! panics, naming the file and line, when it cannot find where an item ends.

/// Whether each byte of `source` is code (true) or part of a comment, string
/// literal or character literal (false). Lifetimes and labels are code.
fn code_mask(source: &str) -> Vec<bool> {
    let bytes = source.as_bytes();
    let length = bytes.len();
    let mut mask = vec![true; length];
    let identifier = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut index = 0;
    while index < length {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if byte == b'/' && next == Some(b'/') {
            let start = index;
            while index < length && bytes[index] != b'\n' {
                index += 1;
            }
            mask[start..index].fill(false);
        } else if byte == b'/' && next == Some(b'*') {
            let start = index;
            let mut depth = 0_usize;
            while index < length {
                if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                    depth += 1;
                    index += 2;
                } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    depth -= 1;
                    index += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    index += 1;
                }
            }
            mask[start..index.min(length)].fill(false);
        } else if byte == b'"' {
            let start = index;
            index += 1;
            while index < length {
                if bytes[index] == b'\\' {
                    index += 2;
                } else if bytes[index] == b'"' {
                    index += 1;
                    break;
                } else {
                    index += 1;
                }
            }
            mask[start..index.min(length)].fill(false);
        } else if byte == b'r'
            && (index == 0
                || !identifier(bytes[index - 1])
                || (bytes[index - 1] == b'b' && (index < 2 || !identifier(bytes[index - 2]))))
            && raw_string_hashes(bytes, index).is_some()
        {
            let hashes = raw_string_hashes(bytes, index).unwrap_or(0);
            let start = index;
            let mut cursor = index + 1 + hashes + 1;
            while cursor < length {
                if bytes[cursor] == b'"'
                    && bytes[cursor + 1..]
                        .iter()
                        .take(hashes)
                        .filter(|&&candidate| candidate == b'#')
                        .count()
                        == hashes
                {
                    cursor += 1 + hashes;
                    break;
                }
                cursor += 1;
            }
            let end = cursor.min(length);
            mask[start..end].fill(false);
            index = end;
        } else if byte == b'\'' {
            if next == Some(b'\\') {
                // An escaped character: '\n', '\'', '\u{41}'. The escape's own
                // character is consumed before looking for the closing quote.
                let mut close = index + 3;
                while close < length && bytes[close] != b'\'' && close - index < 14 {
                    close += 1;
                }
                if close < length && bytes[close] == b'\'' {
                    mask[index..=close].fill(false);
                    index = close + 1;
                } else {
                    index += 1;
                }
            } else if let Some(character) = source[index + 1..].chars().next() {
                let width = character.len_utf8();
                if bytes.get(index + 1 + width) == Some(&b'\'') {
                    mask[index..index + 2 + width].fill(false);
                    index += 2 + width;
                } else {
                    index += 1; // a lifetime or a label
                }
            } else {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    mask
}

/// The number of `#` in a raw string opener (`r"`, `r#"`, `r##"`) starting at
/// the `r` at `index`, or None when it is not one.
fn raw_string_hashes(bytes: &[u8], index: usize) -> Option<usize> {
    let mut cursor = index + 1;
    let mut hashes = 0;
    while bytes.get(cursor) == Some(&b'#') {
        hashes += 1;
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'"')).then_some(hashes)
}

/// The words that start an item. A `#[cfg(test)]` before anything else is a
/// field, an arm or a statement, and only its own next line is blanked.
const ITEM_WORDS: &[&str] = &[
    "mod",
    "fn",
    "use",
    "const",
    "static",
    "struct",
    "enum",
    "impl",
    "trait",
    "type",
    "extern",
    "unsafe",
    "async",
    "macro_rules!",
    "union",
];

fn first_word(line: &str) -> &str {
    let mut text = line.trim_start();
    if let Some(rest) = text.strip_prefix("pub")
        && rest.starts_with(|c: char| c.is_whitespace() || c == '(')
    {
        text = rest;
        if text.starts_with('(')
            && let Some(close) = text.find(')')
        {
            text = &text[close + 1..];
        }
        text = text.trim_start();
    }
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '!'))
        .unwrap_or(text.len());
    &text[..end]
}

/// Whether an attribute line is a cfg that can only be true under test:
/// `#[cfg(test)]`, or `#[cfg(all(..))]` with `test` as one of the conjuncts.
fn is_test_only_cfg(attribute: &str) -> bool {
    let Some(inner) = attribute
        .strip_prefix("#[cfg(")
        .and_then(|rest| rest.strip_suffix(")]"))
    else {
        return false;
    };
    let inner = inner.trim();
    if inner == "test" {
        return true;
    }
    let Some(conjuncts) = inner
        .strip_prefix("all(")
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    let mut depth = 0_i32;
    let mut start = 0;
    let mut parts = Vec::new();
    for (position, character) in conjuncts.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&conjuncts[start..position]);
                start = position + 1;
            }
            _ => {}
        }
    }
    parts.push(&conjuncts[start..]);
    depth == 0 && parts.iter().any(|part| part.trim() == "test")
}

/// `source` with every `#[cfg(test)]` item blanked, line for line: a
/// `;`-terminated item (`mod x;`, `use ..;`) ends at its `;`, a braced item
/// (`mod tests { .. }`, `fn f() { .. }`) ends where its braces balance, and the
/// attributes and comments between the `#[cfg(test)]` line and the item belong
/// to it. A `#[cfg(test)]` on a field, an arm or a statement blanks that one
/// line (a multi-line one leaves its tail visible: too much, never too little).
/// Every blanked line stays as an empty line, so line numbers in the result are
/// the line numbers in `source`.
///
/// Only a cfg alone on its line and in code that can be true only under test is
/// recognised: `#[cfg(test)]`, and `#[cfg(all(.., test, ..))]` with `test` a
/// direct conjunct. `#[cfg(any(test, ..))]`, `#[cfg(not(test))]` and every
/// other form stay visible: too much, never too little. `label` names the file in the panic raised when an item does
/// not end cleanly (end of file, a closing brace below depth zero, a multi-line
/// attribute): a guard must fail loudly, not read a half-blanked file.
#[allow(dead_code)]
pub fn production_text(label: &str, source: &str) -> String {
    let mask = code_mask(source);
    let bytes = source.as_bytes();
    let lines: Vec<&str> = source.lines().collect();
    let mut starts = vec![0_usize];
    for (offset, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            starts.push(offset + 1);
        }
    }
    let item_end = |first: usize| -> usize {
        let mut braces = 0_i64;
        let mut nesting = 0_i64;
        let mut opened = false;
        let mut line = first;
        for (position, &byte) in bytes.iter().enumerate().skip(starts[first]) {
            if mask[position] {
                match byte {
                    b'(' | b'[' => nesting += 1,
                    b')' | b']' => nesting -= 1,
                    b'{' => {
                        braces += 1;
                        opened = true;
                    }
                    b'}' => {
                        braces -= 1;
                        assert!(
                            braces >= 0,
                            "{label}:{}: a closing brace ends the enclosing block before the \
                             #[cfg(test)] item does",
                            line + 1
                        );
                        if opened && braces == 0 && nesting == 0 {
                            return line;
                        }
                    }
                    b';' if braces == 0 && nesting == 0 => return line,
                    _ => {}
                }
            }
            if byte == b'\n' {
                line += 1;
            }
        }
        panic!(
            "{label}:{}: the item under #[cfg(test)] does not end before the end of the file",
            first + 1
        );
    };
    let mut blanked = vec![false; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        let indent = lines[index].len() - lines[index].trim_start().len();
        if !is_test_only_cfg(lines[index].trim()) || !mask[starts[index] + indent] {
            index += 1;
            continue;
        }
        let mut item = index + 1;
        while item < lines.len() {
            let text = lines[item].trim_start();
            if text.starts_with("#[") {
                assert!(
                    text.trim_end().ends_with(']'),
                    "{label}:{}: a multi-line attribute under #[cfg(test)]",
                    item + 1
                );
                item += 1;
            } else if text.starts_with("//") || text.is_empty() {
                item += 1;
            } else {
                break;
            }
        }
        assert!(
            item < lines.len(),
            "{label}:{}: #[cfg(test)] at the end of the file",
            index + 1
        );
        let braced = lines[item].trim_start().starts_with('{');
        let end = if braced || ITEM_WORDS.contains(&first_word(lines[item])) {
            item_end(item)
        } else {
            item
        };
        for flag in &mut blanked[index..=end] {
            *flag = true;
        }
        index = end + 1;
    }
    let mut out = String::with_capacity(source.len());
    for (line, blank) in lines.iter().zip(&blanked) {
        if !blank {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::production_text;

    fn shown(source: &str) -> Vec<String> {
        production_text("fixture.rs", source)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn a_braced_test_module_a_declaration_and_a_fn_are_blanked_with_their_lines_kept() {
        let source = "fn a() {}\n#[cfg(test)]\nmod x;\nfn b() {}\n#[cfg(test)]\n#[allow(dead_code)]\nfn t() {\n    let s = 1;\n}\nfn c() {}\n#[cfg(test)]\nmod tests {\n    fn d() {}\n}\nfn e() {}\n";
        let lines = shown(source);
        assert_eq!(lines.len(), source.lines().count());
        assert_eq!(lines[0], "fn a() {}");
        assert_eq!(lines[3], "fn b() {}");
        assert_eq!(lines[9], "fn c() {}");
        assert_eq!(lines[14], "fn e() {}");
        assert!(lines[1..3].iter().all(String::is_empty));
        assert!(lines[4..9].iter().all(String::is_empty));
        assert!(lines[10..14].iter().all(String::is_empty));
    }

    #[test]
    fn brace_and_quote_characters_in_literals_and_comments_do_not_move_the_depth() {
        let source = "#[cfg(test)]\nmod tests {\n    fn t() {\n        let open = '{';\n        let close = \"}\";\n        let quote = '\\'';\n        let raw = r#\"} \"{\"#;\n        // } not a brace\n        /* { nested /* } */ } */\n        let life: &'static str = \"x\";\n    }\n}\nfn production() {}\n";
        let lines = shown(source);
        assert_eq!(lines.last().map(String::as_str), Some("fn production() {}"));
        assert!(lines[..lines.len() - 1].iter().all(String::is_empty));
    }

    #[test]
    fn a_cfg_that_can_only_be_true_under_test_is_blanked_in_every_all_shape() {
        for attribute in [
            "#[cfg(all(test, unix))]",
            "#[cfg(all(unix, test))]",
            "#[cfg(all(test))]",
            "#[cfg(all(unix, test, target_pointer_width = \"64\"))]",
            "#[cfg(all(test, any(unix, windows)))]",
        ] {
            let source = format!(
                "fn a() {{}}\n{attribute}\nmod tests {{\n    fn t() {{}}\n}}\nfn b() {{}}\n"
            );
            let lines = shown(&source);
            assert_eq!(lines[0], "fn a() {}", "{attribute}");
            assert!(lines[1..5].iter().all(String::is_empty), "{attribute}");
            assert_eq!(lines[5], "fn b() {}", "{attribute}");
        }
    }

    #[test]
    fn a_cfg_that_can_be_true_outside_test_stays_visible() {
        for attribute in [
            "#[cfg(any(test, unix))]",
            "#[cfg(not(test))]",
            "#[cfg(all(not(test), unix))]",
            "#[cfg(all(unix, any(test, windows)))]",
            "#[cfg(all(feature = \"test\", unix))]",
            "#[cfg(feature = \"test-support\")]",
            "#[cfg(all(unix, windows))]",
        ] {
            let source =
                format!("fn a() {{}}\n{attribute}\nmod m {{\n    fn t() {{}}\n}}\nfn b() {{}}\n");
            let lines = shown(&source);
            assert_eq!(lines.len(), source.lines().count(), "{attribute}");
            assert_eq!(lines[1], attribute, "{attribute}");
            assert_eq!(lines[3], "    fn t() {}", "{attribute}");
        }
    }

    #[test]
    fn a_cfg_test_field_blanks_that_line_only_and_the_impl_after_the_struct_stays() {
        let source = "struct S {\n    #[cfg(test)]\n    hook: u8,\n    other: u8,\n}\nimpl Drop for S {\n    fn drop(&mut self) {}\n}\n";
        let lines = shown(source);
        assert!(lines[1].is_empty() && lines[2].is_empty());
        assert_eq!(lines[3], "    other: u8,");
        assert_eq!(lines[4], "}");
        assert_eq!(lines[5], "impl Drop for S {");
        assert_eq!(lines[7], "}");
    }

    #[test]
    fn a_test_module_at_the_end_of_the_file_with_no_trailing_newline() {
        let lines = shown("fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}");
        assert_eq!(lines[0], "fn a() {}");
        assert!(lines[1..].iter().all(String::is_empty));
    }

    #[test]
    fn production_code_after_a_test_module_and_a_non_test_cfg_stay() {
        let lines =
            shown("#[cfg(test)]\nmod tests {\n}\nfn after() {}\n#[cfg(not(test))]\nfn kept() {}\n");
        assert_eq!(lines[3], "fn after() {}");
        assert_eq!(lines[5], "fn kept() {}");
    }

    #[test]
    fn a_cfg_test_text_inside_a_string_or_a_comment_is_not_an_attribute() {
        let lines =
            shown("const A: &str = \"\n#[cfg(test)]\n\";\n/*\n#[cfg(test)]\n*/\nfn a() {}\n");
        assert_eq!(lines[6], "fn a() {}");
        assert_eq!(lines[1], "#[cfg(test)]");
    }

    #[test]
    #[should_panic(expected = "fixture.rs:3: the item under #[cfg(test)] does not end")]
    fn an_item_that_never_ends_panics_with_its_file_and_line() {
        production_text(
            "fixture.rs",
            "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n",
        );
    }

    #[test]
    #[should_panic(expected = "fixture.rs:4: a closing brace ends the enclosing block")]
    fn a_closing_brace_below_depth_zero_panics() {
        production_text(
            "fixture.rs",
            "struct S {\n    #[cfg(test)]\n    const X: u8 = 1\n}\n",
        );
    }

    #[test]
    #[should_panic(expected = "a multi-line attribute under #[cfg(test)]")]
    fn a_multi_line_attribute_panics() {
        production_text(
            "fixture.rs",
            "#[cfg(test)]\n#[allow(\n    dead_code\n)]\nfn a() {}\n",
        );
    }
}
