//! Source gate: every production struct in this crate that holds a
//! `ManuallyDrop` field has its own `Drop`, and no `forget` appears anywhere
//! in the crate's source, test modules included.
//!
//! A `ManuallyDrop` field never releases what it holds. Without a `Drop` on
//! its owner, dropping that owner on a refusal, a `?` or an unwind loses the
//! held custody silently. With one, the owner either releases when settled
//! or hands its unsettled contents to `lillux::custody::abandon`, which keeps
//! them for the life of the process and counts them. `forget` would bypass
//! both, so it is refused outright. A copy running in another process, such
//! as a forked child, gives up its copied owners only through
//! `lillux::custody::relinquish_in_foreign_process`, which this gate allows.
//!
//! Structs inside a test item (any item under a `#[cfg(..)]` whose predicate
//! names `test` other than only as `not(test)`) are fixtures and exempt from
//! the `Drop` rule, but not from the `forget` ban.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `directory`, at any depth, sorted.
fn rust_files_under(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).expect("gated source directory") {
            let path = entry.expect("gated source entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Whether `byte` can continue an identifier.
fn ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Source text with comments, string contents and char-literal contents
/// blanked out (newlines kept for line numbers), so neither prose nor a
/// literal can satisfy or trip the gate, and a quoted brace never unbalances
/// an item.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        // `r"…"`, `r#"…"#`, `br"…"` and `br#"…"#`, but never the tail of an
        // identifier such as `bar"`.
        let raw_start = if byte == b'r' {
            let before = index.checked_sub(1).map(|at| bytes[at]);
            match before {
                Some(b'b') => {
                    let start_ok = index < 2 || !ident_byte(bytes[index - 2]);
                    start_ok.then_some(index - 1)
                }
                Some(previous) if ident_byte(previous) => None,
                _ => Some(index),
            }
        } else {
            None
        };
        let raw_hashes = raw_start
            .map(|_| bytes[index + 1..].iter().take_while(|&&b| b == b'#').count())
            .filter(|&hashes| bytes.get(index + 1 + hashes) == Some(&b'"'));
        if let Some(hashes) = raw_hashes {
            let close = format!("\"{}", "#".repeat(hashes));
            let body = index + 2 + hashes;
            let end = source[body..].find(&close).map_or(bytes.len(), |offset| body + offset + close.len());
            out.push_str("\"\"");
            out.extend(source[index..end].chars().filter(|&c| c == '\n'));
            index = end;
        } else if byte == b'"' {
            let start = index;
            index += 1;
            while index < bytes.len() && bytes[index] != b'"' {
                if bytes[index] == b'\\' {
                    index += 1;
                }
                index += 1;
            }
            index = (index + 1).min(bytes.len());
            out.push_str("\"\"");
            out.extend(source[start..index].chars().filter(|&c| c == '\n'));
        } else if byte == b'\'' {
            // A char literal is blanked whole; a lifetime is kept as is.
            let end = if bytes.get(index + 1) == Some(&b'\\') {
                source[index + 2..].find('\'').map(|offset| index + 3 + offset)
            } else {
                source[index + 1..]
                    .chars()
                    .next()
                    .map(|c| index + 1 + c.len_utf8())
                    .filter(|&close| bytes.get(close) == Some(&b'\''))
                    .map(|close| close + 1)
            };
            match end {
                Some(end) => {
                    out.push_str("' '");
                    index = end;
                }
                None => {
                    out.push('\'');
                    index += 1;
                }
            }
        } else if source[index..].starts_with("//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
        } else if source[index..].starts_with("/*") {
            let mut depth = 0usize;
            while index < bytes.len() {
                if source[index..].starts_with("/*") {
                    depth += 1;
                    index += 2;
                } else if source[index..].starts_with("*/") {
                    depth -= 1;
                    index += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if bytes[index] == b'\n' {
                        out.push('\n');
                    }
                    index += 1;
                }
            }
        } else {
            let c = source[index..].chars().next().expect("char boundary");
            out.push(c);
            index += c.len_utf8();
        }
    }
    out
}

fn line_of(code: &str, offset: usize) -> usize {
    code[..offset].bytes().filter(|&byte| byte == b'\n').count() + 1
}

/// Offsets of every whole-identifier occurrence of `word` in `code`.
fn word_offsets(code: &str, word: &str) -> Vec<usize> {
    let bytes = code.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = code[from..].find(word) {
        let start = from + offset;
        let end = start + word.len();
        let before = start.checked_sub(1).map(|at| bytes[at]);
        if !before.is_some_and(ident_byte) && !bytes.get(end).copied().is_some_and(ident_byte) {
            found.push(start);
        }
        from = end;
    }
    found
}

/// Index just past the item that starts at `start`: its first balanced
/// `{..}` block, or its first `;` or `,` outside any bracket, whichever
/// closes it. A field or statement item also ends at its enclosing `}`.
fn item_end(code: &str, start: usize) -> usize {
    let bytes = code.as_bytes();
    let mut depth = 0usize;
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b';' | b',' if depth == 0 => return index + 1,
            b'}' if depth == 0 => return index,
            b'{' if depth == 0 => {
                let mut braces = 1usize;
                index += 1;
                while index < bytes.len() && braces > 0 {
                    match bytes[index] {
                        b'{' => braces += 1,
                        b'}' => braces -= 1,
                        _ => {}
                    }
                    index += 1;
                }
                return index;
            }
            _ => {}
        }
        index += 1;
    }
    bytes.len()
}

/// Index just past a struct declared at `start`: its balanced `{..}` or
/// `(..)` field list, or its `;` when it has none.
fn struct_end(code: &str, start: usize) -> usize {
    let bytes = code.as_bytes();
    let Some(open) = code[start..].find(['{', '(', ';']).map(|offset| start + offset) else {
        return bytes.len();
    };
    let (opener, closer) = match bytes[open] {
        b'{' => (b'{', b'}'),
        b'(' => (b'(', b')'),
        _ => return open + 1,
    };
    let mut depth = 1usize;
    let mut index = open + 1;
    while index < bytes.len() && depth > 0 {
        if bytes[index] == opener {
            depth += 1;
        } else if bytes[index] == closer {
            depth -= 1;
        }
        index += 1;
    }
    index
}

/// Byte ranges of every item under a `#[cfg(..)]` whose predicate names
/// `test` other than only as `not(test)`, in stripped `code`.
fn test_item_ranges(code: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = code.as_bytes();
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(offset) = code[from..].find("#[cfg(") {
        let start = from + offset;
        let predicate_start = start + "#[cfg(".len();
        let mut depth = 1usize;
        let mut index = predicate_start;
        while index < bytes.len() && depth > 0 {
            match bytes[index] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            index += 1;
        }
        let predicate: String = code[predicate_start..index.saturating_sub(1)]
            .split_whitespace()
            .collect();
        let names_test = !word_offsets(&predicate.replace("not(test)", ""), "test").is_empty();
        // Skip the closing `]` of this attribute; the item, with any further
        // attributes, follows.
        let item = code[index..].find(']').map_or(bytes.len(), |close| index + close + 1);
        if names_test {
            let mut item_start = item;
            loop {
                let rest = &code[item_start..];
                let trimmed = rest.trim_start();
                item_start += rest.len() - trimmed.len();
                if !trimmed.starts_with("#[") {
                    break;
                }
                item_start = code[item_start..].find(']').map_or(bytes.len(), |close| item_start + close + 1);
            }
            ranges.push(start..item_end(code, item_start));
        }
        from = item;
    }
    ranges
}

/// Every violation in one file's source, as `line: reason`.
fn violations(source: &str) -> Vec<String> {
    let code = strip_comments(source);
    let tests = test_item_ranges(&code);
    let in_test = |offset: usize| tests.iter().any(|range| range.contains(&offset));
    let mut found = Vec::new();
    for offset in word_offsets(&code, "forget") {
        found.push(format!("{}: forget bypasses the custody holder", line_of(&code, offset)));
    }
    for offset in word_offsets(&code, "struct") {
        if in_test(offset) {
            continue;
        }
        let rest = &code[offset + "struct".len()..];
        let name: String = rest
            .trim_start()
            .bytes()
            .take_while(|&byte| ident_byte(byte))
            .map(char::from)
            .collect();
        if name.is_empty() {
            continue;
        }
        let end = struct_end(&code, offset);
        if word_offsets(&code[offset..end], "ManuallyDrop").is_empty() {
            continue;
        }
        let has_drop = word_offsets(&code, &name).into_iter().any(|at| {
            !in_test(at) && code[..at].trim_end().ends_with("for")
                && code[..at].trim_end().trim_end_matches("for").trim_end().ends_with("Drop")
        });
        if !has_drop {
            found.push(format!(
                "{}: struct `{name}` holds a ManuallyDrop field but has no Drop",
                line_of(&code, offset)
            ));
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn crate_sources_hold_no_forget_and_no_undropped_manual_owner() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_files_under(&src);
    for required in ["lib.rs", "native.rs", "original_physical_custody.rs", "original_host_owner.rs"] {
        assert!(files.contains(&src.join(required)), "gate lost {required}");
    }
    let mut refused = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path).expect("gated source");
        for violation in violations(&source) {
            refused.push(format!("{}:{violation}", path.strip_prefix(&src).unwrap().display()));
        }
    }
    assert!(refused.is_empty(), "custody drop gate refused:\n{}", refused.join("\n"));
}

#[test]
fn gate_requires_a_drop_for_each_manual_owner() {
    let source = r#"
        struct Owner {
            lock: std::mem::ManuallyDrop<Lock>,
        }
        struct Plain { lock: Lock }
        pub struct Generic<T: Send, U: 'static> { slots: ManuallyDrop<Vec<(T, U)>> }
        impl<T: Send, U: 'static> Drop for Generic<T, U> { fn drop(&mut self) {} }
        struct Nested { body: Option<ManuallyDrop<Body>> }
        impl Drop for Nested { fn drop(&mut self) {} }
        // A comment naming struct Fake { x: ManuallyDrop<T> } is not code.
        const TEXT: &str = "struct Quoted { x: ManuallyDrop<T> }";
        #[cfg(test)]
        mod tests {
            struct Fixture { held: ManuallyDrop<File> }
        }
        #[cfg(all(test, target_os = "linux"))]
        struct LinuxFixture { held: ManuallyDrop<File> }
    "#;
    assert_eq!(violations(source), vec!["2: struct `Owner` holds a ManuallyDrop field but has no Drop"]);
    // A Drop inside a test item does not cover a production owner.
    let covered_by_test = "struct Owner { lock: ManuallyDrop<Lock> }\n\
        #[cfg(test)]\n\
        impl Drop for Owner { fn drop(&mut self) {} }";
    assert_eq!(
        violations(covered_by_test),
        vec!["1: struct `Owner` holds a ManuallyDrop field but has no Drop"]
    );
    // `not(test)` is production code.
    let negated = "#[cfg(not(test))]\nstruct Live { lock: ManuallyDrop<Lock> }";
    assert_eq!(violations(negated), vec!["2: struct `Live` holds a ManuallyDrop field but has no Drop"]);
    // A tuple struct is an owner too.
    assert_eq!(
        violations("struct Wrapped(ManuallyDrop<Lock>);"),
        vec!["1: struct `Wrapped` holds a ManuallyDrop field but has no Drop"]
    );
}

#[test]
fn gate_refuses_forget_everywhere() {
    assert_eq!(
        violations("fn f(owner: Owner) { std::mem::forget(owner); }"),
        vec!["1: forget bypasses the custody holder"]
    );
    assert_eq!(violations("use std::mem::forget;\nfn f(o: O) { forget(o) }").len(), 2);
    assert_eq!(
        violations("#[cfg(test)]\nmod tests {\n    fn t(o: O) { std::mem::forget(o) }\n}"),
        vec!["3: forget bypasses the custody holder"]
    );
    // The audited relinquish primitive for copies in another process is the
    // one sanctioned way to skip a destructor; it is not a forget.
    assert!(violations("fn f(o: O) { lillux::custody::relinquish_in_foreign_process(o) }").is_empty());
    // Prose and literals are not code; a longer identifier is not the token.
    assert!(violations("// forget(owner)\nconst T: &str = \"forget\";\nfn forget_not() {}").is_empty());
}
