//! Source gate: every production struct in this crate that holds a
//! `ManuallyDrop` field has its own `Drop`, every production enum with such
//! a payload is guarded by a `Drop`, and no `forget` appears anywhere in the
//! crate's source, test modules included.
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
//! An enum whose variants hold a `ManuallyDrop` payload is guarded the same
//! way: it needs its own `Drop`, or every struct that holds it needs a `Drop`
//! that names the holding field on every path through its body, directly or
//! in a method it calls on `self` (an enum holding it must in turn be
//! guarded). A field named in one branch of an `if` or `match` but not in
//! another, or only after an early `return` that skips it, leaves the enum
//! dropped unguarded on the other path. An enum nothing holds, one moved out
//! of its owner, and one whose name another declaration shares needs its own
//! `Drop`; a same-named type's `Drop` counts only in the declaring file. An
//! enum is moved out when a function returns it, takes it by value (as a
//! parameter, or as `self` in its own `impl`), or a local binds it: by
//! annotation, by construction, or from a `mem::replace`, `mem::take`,
//! `Option::take` or `Option::replace` of a field that holds it, outside the
//! holder's own `Drop`. A type alias whose definition names `ManuallyDrop`,
//! or an import renamed from it, counts as `ManuallyDrop` wherever it is used.
//!
//! Structs inside a test item (any item under a `#[cfg(..)]` whose predicate
//! names `test` other than only as `not(test)`) are fixtures and exempt from
//! the `Drop` rule, but not from the `forget` ban.

use std::collections::BTreeSet;
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

/// One gated source: its stripped code and the byte ranges of its test items.
struct Gated {
    code: String,
    tests: Vec<std::ops::Range<usize>>,
}

impl Gated {
    fn new(source: &str) -> Self {
        let code = strip_comments(source);
        let tests = test_item_ranges(&code);
        Self { code, tests }
    }

    fn in_test(&self, offset: usize) -> bool {
        self.tests.iter().any(|range| range.contains(&offset))
    }
}

/// The identifier that starts at or after `from`, past any whitespace.
fn ident_at(code: &str, from: usize) -> String {
    code[from..]
        .trim_start()
        .bytes()
        .take_while(|&byte| ident_byte(byte))
        .map(char::from)
        .collect()
}

/// The identifier that ends at or before `to`, past any whitespace.
fn ident_before(code: &str, to: usize) -> String {
    let head = code[..to].trim_end();
    let start = head.len() - head.bytes().rev().take_while(|&byte| ident_byte(byte)).count();
    head[start..].to_string()
}

fn names_any(code: &str, names: &BTreeSet<String>) -> bool {
    names.iter().any(|name| !word_offsets(code, name).is_empty())
}

/// Every name that denotes `ManuallyDrop` in production code: the type
/// itself, each import renamed from such a name, and each type alias whose
/// definition names one, closed under repetition so an alias of an alias
/// counts too.
fn manual_names(files: &[Gated]) -> BTreeSet<String> {
    let mut names = BTreeSet::from(["ManuallyDrop".to_string()]);
    loop {
        let known = names.len();
        for file in files {
            let code = &file.code;
            for offset in word_offsets(code, "type") {
                if file.in_test(offset) {
                    continue;
                }
                let name = ident_at(code, offset + "type".len());
                let end = code[offset..].find(';').map_or(code.len(), |at| offset + at);
                let definition = code[offset..end].find('=').map(|at| &code[offset + at + 1..end]);
                if !name.is_empty() && definition.is_some_and(|definition| names_any(definition, &names)) {
                    names.insert(name);
                }
            }
            for offset in word_offsets(code, "as") {
                if file.in_test(offset) || !names.contains(&ident_before(code, offset)) {
                    continue;
                }
                let renamed = ident_at(code, offset + "as".len());
                if !renamed.is_empty() && renamed != "_" {
                    names.insert(renamed);
                }
            }
        }
        if names.len() == known {
            return names;
        }
    }
}

/// A production `struct` or `enum` declaration.
struct Declared {
    file: usize,
    offset: usize,
    is_enum: bool,
    name: String,
    body: std::ops::Range<usize>,
}

fn declarations(files: &[Gated]) -> Vec<Declared> {
    let mut found = Vec::new();
    for (index, file) in files.iter().enumerate() {
        for (keyword, is_enum) in [("struct", false), ("enum", true)] {
            for offset in word_offsets(&file.code, keyword) {
                if file.in_test(offset) {
                    continue;
                }
                let name = ident_at(&file.code, offset + keyword.len());
                if name.is_empty() {
                    continue;
                }
                let body = offset..struct_end(&file.code, offset);
                found.push(Declared { file: index, offset, is_enum, name, body });
            }
        }
    }
    found
}

/// Whether more than one production `struct` or `enum` carries `name`.
fn ambiguous(declared: &[Declared], name: &str) -> bool {
    declared.iter().filter(|item| item.name == name).count() > 1
}

/// The bodies of every production `impl Drop for` the type `item` declares.
/// When another declaration shares its name, only an impl in the same file
/// can be shown to name this type, so impls elsewhere do not count.
fn drop_bodies(files: &[Gated], declared: &[Declared], item: &Declared) -> Vec<(usize, std::ops::Range<usize>)> {
    let same_file_only = ambiguous(declared, &item.name);
    let mut bodies = Vec::new();
    for (index, file) in files.iter().enumerate() {
        if same_file_only && index != item.file {
            continue;
        }
        for at in word_offsets(&file.code, &item.name) {
            let head = file.code[..at].trim_end();
            if !file.in_test(at) && head.ends_with("for")
                && head.trim_end_matches("for").trim_end().ends_with("Drop")
            {
                bodies.push((index, at..item_end(&file.code, at)));
            }
        }
    }
    bodies
}

fn has_drop(files: &[Gated], declared: &[Declared], item: &Declared) -> bool {
    !drop_bodies(files, declared, item).is_empty()
}

/// Names of the fields in a struct body whose type names `held`: the field
/// identifier for a named field, its position for a tuple field.
fn fields_holding(body: &str, held: &str) -> Vec<String> {
    let Some(open) = body.find(['{', '(']) else {
        return Vec::new();
    };
    let tuple = body.as_bytes()[open] == b'(';
    let inner = &body[open + 1..body.len().saturating_sub(1).max(open + 1)];
    let mut fields = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    let mut position = 0;
    let bytes = inner.as_bytes();
    for index in 0..=bytes.len() {
        let at_end = index == bytes.len();
        if !at_end {
            match bytes[index] {
                b'(' | b'[' | b'<' | b'{' => depth += 1,
                b')' | b']' | b'>' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if at_end || (bytes[index] == b',' && depth == 0) {
            let segment = &inner[start..index];
            if !segment.trim().is_empty() {
                if !word_offsets(segment, held).is_empty() {
                    if tuple {
                        fields.push(position.to_string());
                    } else if let Some(colon) = segment.find(':') {
                        fields.push(ident_before(segment, colon));
                    }
                }
                position += 1;
            }
            start = index + 1;
        }
    }
    fields
}

/// Whether `code` reads `.field` as a whole field access.
fn names_field(code: &str, field: &str) -> bool {
    word_offsets(code, field).into_iter().any(|at| code[..at].ends_with('.'))
}

/// Whether `text`, inside `file`, names `field`: directly, or in the body
/// of a function it calls on `self`.
fn text_names_field(file: &Gated, text: &str, field: &str) -> bool {
    let code = &file.code;
    if names_field(text, field) {
        return true;
    }
    word_offsets(text, "self").into_iter().any(|at| {
        let rest = &text[at + "self".len()..];
        let Some(called) = rest.strip_prefix('.') else {
            return false;
        };
        let name = ident_at(called, 0);
        !name.is_empty() && called[name.len()..].trim_start().starts_with('(')
            && word_offsets(code, &name).into_iter().any(|def| {
                !file.in_test(def) && code[..def].trim_end().ends_with("fn")
                    && names_field(&code[def..item_end(code, def)], field)
            })
    })
}

/// Index of the `}` matching the `{` at `open`.
fn matching_brace(code: &str, open: usize) -> usize {
    let bytes = code.as_bytes();
    let mut depth = 0usize;
    for (index, &byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    bytes.len()
}

/// Whether the identifier `word` starts at `at` in `code`.
fn word_at(code: &str, at: usize, word: &str) -> bool {
    let bytes = code.as_bytes();
    code[at..].starts_with(word)
        && !at.checked_sub(1).is_some_and(|before| ident_byte(bytes[before]))
        && !bytes.get(at + word.len()).copied().is_some_and(ident_byte)
}

/// The index of the first non-whitespace byte at or after `at`.
fn skip_space(code: &str, at: usize) -> usize {
    at + code[at..].len() - code[at..].trim_start().len()
}

/// One piece of a function body, in execution order, for path analysis.
enum Piece {
    /// Code that runs whenever control reaches it.
    Text(std::ops::Range<usize>),
    /// A branching construct: each arm may run; a non-exhaustive one may
    /// also run none of them.
    Branch { arms: Vec<Vec<Piece>>, exhaustive: bool },
    /// Control leaves the body here.
    Return,
}

/// The pieces of the block content `start..end` (inside its braces). An
/// `if`/`else` chain, a `match`, and a `while` or `for` body are branches;
/// any other block (`unsafe`, a closure, a struct literal) runs in line.
fn pieces(code: &str, start: usize, end: usize) -> Vec<Piece> {
    let bytes = code.as_bytes();
    let mut out = Vec::new();
    let mut text_start = start;
    let mut head_start = start;
    let mut index = start;
    while index < end {
        let byte = bytes[index];
        if byte == b';' {
            head_start = index + 1;
            index += 1;
        } else if byte == b'{' {
            let head = &code[head_start..index];
            let close = matching_brace(code, index).min(end);
            out.push(Piece::Text(text_start..index));
            if !word_offsets(head, "match").is_empty() {
                out.push(Piece::Branch { arms: match_arms(code, index + 1, close), exhaustive: true });
                index = close + 1;
            } else if !word_offsets(head, "if").is_empty() {
                let mut arms = vec![pieces(code, index + 1, close)];
                let mut exhaustive = false;
                let mut next = close + 1;
                loop {
                    let at = skip_space(code, next);
                    if at >= end || !word_at(code, at, "else") {
                        break;
                    }
                    let after = skip_space(code, at + "else".len());
                    let Some(open) = code[after..end].find('{').map(|offset| after + offset) else {
                        break;
                    };
                    let close = matching_brace(code, open).min(end);
                    let mut arm = vec![Piece::Text(after..open)];
                    arm.extend(pieces(code, open + 1, close));
                    arms.push(arm);
                    next = close + 1;
                    if !word_at(code, after, "if") {
                        exhaustive = true;
                        break;
                    }
                }
                out.push(Piece::Branch { arms, exhaustive });
                index = next;
            } else if !word_offsets(head, "while").is_empty() || !word_offsets(head, "for").is_empty() {
                out.push(Piece::Branch { arms: vec![pieces(code, index + 1, close)], exhaustive: false });
                index = close + 1;
            } else {
                out.extend(pieces(code, index + 1, close));
                index = close + 1;
            }
            text_start = index;
            head_start = index;
        } else if word_at(code, index, "return") {
            out.push(Piece::Text(text_start..index));
            let mut depth = 0usize;
            let mut stop = index;
            while stop < end {
                match bytes[stop] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                    b';' if depth == 0 => break,
                    _ => {}
                }
                stop += 1;
            }
            out.push(Piece::Text(index..stop));
            out.push(Piece::Return);
            index = stop;
            text_start = index;
            head_start = index;
        } else {
            index += 1;
        }
    }
    out.push(Piece::Text(text_start..end));
    out
}

/// The arms of the `match` block content `start..end`, each as pieces.
fn match_arms(code: &str, start: usize, end: usize) -> Vec<Vec<Piece>> {
    let bytes = code.as_bytes();
    let mut arms = Vec::new();
    let mut index = start;
    while index < end {
        let mut depth = 0usize;
        let mut arrow = None;
        let mut at = index;
        while at + 1 < end {
            match bytes[at] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                b'=' if depth == 0 && bytes[at + 1] == b'>' => {
                    arrow = Some(at);
                    break;
                }
                _ => {}
            }
            at += 1;
        }
        let Some(arrow) = arrow else {
            break;
        };
        let body = skip_space(code, arrow + 2);
        if body < end && bytes[body] == b'{' {
            let close = matching_brace(code, body).min(end);
            arms.push(pieces(code, body + 1, close));
            index = skip_space(code, close + 1);
            if index < end && bytes[index] == b',' {
                index += 1;
            }
        } else {
            let mut depth = 0usize;
            let mut stop = body;
            while stop < end {
                match bytes[stop] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                    b',' if depth == 0 => break,
                    _ => {}
                }
                stop += 1;
            }
            arms.push(pieces(code, body, stop));
            index = stop + 1;
        }
    }
    arms
}

/// Reachability after `pieces`, from `reached` (bit 1: a path that has not
/// named the field; bit 2: one that has), and whether any path returns
/// without naming it.
fn walk(file: &Gated, pieces: &[Piece], field: &str, mut reached: u8) -> (u8, bool) {
    let mut unnamed_exit = false;
    for piece in pieces {
        match piece {
            Piece::Text(range) => {
                if reached != 0 && text_names_field(file, &file.code[range.clone()], field) {
                    reached = 2;
                }
            }
            Piece::Return => {
                unnamed_exit |= reached & 1 != 0;
                reached = 0;
            }
            Piece::Branch { arms, exhaustive } => {
                let mut after = if *exhaustive { 0 } else { reached };
                for arm in arms {
                    let (arm_after, arm_exit) = walk(file, arm, field, reached);
                    after |= arm_after;
                    unnamed_exit |= arm_exit;
                }
                reached = after;
            }
        }
    }
    (reached, unnamed_exit)
}

/// Whether the `fn drop` of the `Drop` impl at `body` in `file` names
/// `field` on every path through it, directly or in a method it calls on
/// `self`.
fn drop_names_field(file: &Gated, body: std::ops::Range<usize>, field: &str) -> bool {
    let code = &file.code;
    let Some(function) = word_offsets(&code[body.clone()], "fn")
        .into_iter()
        .map(|at| body.start + at)
        .find(|&at| ident_at(code, at + "fn".len()) == "drop")
    else {
        return false;
    };
    let Some(open) = code[function..body.end].find('{').map(|offset| function + offset) else {
        return false;
    };
    let close = matching_brace(code, open);
    let (reached, unnamed_exit) = walk(file, &pieces(code, open + 1, close), field, 1);
    reached & 1 == 0 && !unnamed_exit
}

/// The parameter list of the function whose `fn` keyword is at `at`, split
/// at its top-level commas.
fn parameters(code: &str, at: usize) -> Vec<&str> {
    let bytes = code.as_bytes();
    let name_end = skip_space(code, at + "fn".len());
    let mut index = name_end + ident_at(code, name_end).len();
    index = skip_space(code, index);
    if index < bytes.len() && bytes[index] == b'<' {
        let mut depth = 0usize;
        while index < bytes.len() {
            match bytes[index] {
                b'<' => depth += 1,
                b'>' if bytes[index - 1] != b'-' => {
                    depth -= 1;
                    if depth == 0 {
                        index += 1;
                        break;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        index = skip_space(code, index);
    }
    if index >= bytes.len() || bytes[index] != b'(' {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut depth = 0usize;
    let mut start = index + 1;
    for at in index..bytes.len() {
        match bytes[at] {
            b'(' | b'[' | b'{' => depth += 1,
            b'<' => depth += 1,
            b'>' if bytes[at - 1] != b'-' => depth = depth.saturating_sub(1),
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    found.push(&code[start..at]);
                    break;
                }
            }
            b',' if depth == 1 => {
                found.push(&code[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    found.into_iter().filter(|parameter| !parameter.trim().is_empty()).collect()
}

/// The type a parameter declares, after its top-level `:`.
fn parameter_type(parameter: &str) -> Option<&str> {
    let bytes = parameter.as_bytes();
    let mut depth = 0usize;
    for (index, &byte) in bytes.iter().enumerate() {
        match byte {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' | b'>' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                if bytes.get(index + 1) == Some(&b':') || index.checked_sub(1).is_some_and(|at| bytes[at] == b':') {
                    continue;
                }
                return Some(parameter[index + 1..].trim());
            }
            _ => {}
        }
    }
    None
}

/// The byte ranges of every production `impl` block in `file`, each with
/// its target type's name and whether it implements `Drop`.
fn impl_blocks(file: &Gated) -> Vec<(std::ops::Range<usize>, String, bool)> {
    let code = &file.code;
    word_offsets(code, "impl")
        .into_iter()
        .filter(|&at| !file.in_test(at))
        .filter_map(|at| {
            let open = code[at..].find(['{', ';']).map(|offset| at + offset)?;
            let header = &code[at + "impl".len()..open];
            let is_drop = !word_offsets(header, "Drop").is_empty();
            let target = match word_offsets(header, "for").last() {
                Some(&at_for) => &header[at_for + "for".len()..],
                None => header.trim_start().strip_prefix('<').map_or(header, |rest| {
                    let mut depth = 1usize;
                    let end = rest.bytes().position(|byte| {
                        match byte {
                            b'<' => depth += 1,
                            b'>' => depth -= 1,
                            _ => {}
                        }
                        depth == 0
                    });
                    end.map_or(rest, |end| &rest[end + 1..])
                }),
            };
            let target = ident_at(target, 0);
            let target = target.rsplit("::").next().unwrap_or_default().to_string();
            Some((at..item_end(code, at), target, is_drop))
        })
        .collect()
}

/// Whether an enum named `name` is moved out of any owner: returned from a
/// function (an `into_custody`, an `into_parts` or inside a `Result`), taken
/// by value by a function (a parameter typed by it outside a reference, or
/// `self` in its own `impl`), or bound to a local by annotation, by
/// construction, or from a `mem::replace`, `mem::take`, `Option::take` or
/// `Option::replace` of a field in `holding`, outside a `Drop` impl.
fn moved_out(files: &[Gated], name: &str, holding: &BTreeSet<String>) -> bool {
    files.iter().any(|file| {
        let code = &file.code;
        let impls = impl_blocks(file);
        let in_drop = |at: usize| impls.iter().any(|(range, _, is_drop)| *is_drop && range.contains(&at));
        let functions: Vec<usize> = word_offsets(code, "fn").into_iter().filter(|&at| !file.in_test(at)).collect();
        let returned = functions.iter().any(|&at| {
            let signature_end = code[at..].find(['{', ';']).map_or(code.len(), |end| at + end);
            let signature = &code[at..signature_end];
            signature.find("->").is_some_and(|arrow| !word_offsets(&signature[arrow..], name).is_empty())
        });
        let taken = functions.iter().any(|&at| {
            let own_impl = impls.iter().any(|(range, target, _)| target == name && range.contains(&at));
            parameters(code, at).into_iter().any(|parameter| {
                let parameter = parameter.trim();
                let receiver = parameter.strip_prefix("mut").map_or(parameter, str::trim_start);
                if receiver == "self" {
                    return own_impl;
                }
                parameter_type(parameter).is_some_and(|declared| {
                    !declared.starts_with('&')
                        && (!word_offsets(declared, name).is_empty()
                            || (own_impl && word_at(receiver, 0, "self") && declared == "Self"))
                })
            })
        });
        let bound = word_offsets(code, "let").into_iter().any(|at| {
            if file.in_test(at) {
                return false;
            }
            let end = code[at..].find(';').map_or(code.len(), |end| at + end);
            let binding = &code[at..end];
            let (pattern, value) = binding.split_once('=').unwrap_or((binding, ""));
            let annotated = pattern.find(':').is_some_and(|colon| !word_offsets(&pattern[colon..], name).is_empty());
            let constructed = value.trim_start().starts_with(&format!("{name}::"));
            let from_field = !in_drop(at) && holding.iter().any(|field| field_moved_out(value, field));
            annotated || constructed || from_field
        });
        returned || taken || bound
    })
}

/// Whether `value` moves `field` out of its holder: `replace` or `take` of
/// `&mut ….field`, or `….field.take()` / `….field.replace(..)`.
fn field_moved_out(value: &str, field: &str) -> bool {
    let swapped = ["replace", "take"].into_iter().any(|call| {
        word_offsets(value, call).into_iter().any(|at| {
            let rest = value[at + call.len()..].trim_start();
            let Some(argument) = rest.strip_prefix('(') else {
                return false;
            };
            let Some(place) = argument.trim_start().strip_prefix("&mut") else {
                return false;
            };
            let place = place.split([',', ')']).next().unwrap_or_default().trim();
            place.ends_with(&format!(".{field}"))
        })
    });
    let method = word_offsets(value, field).into_iter().any(|at| {
        let after: String = value[at + field.len()..].split_whitespace().collect();
        value[..at].ends_with('.') && (after.starts_with(".take()") || after.starts_with(".replace("))
    });
    swapped || method
}

/// Whether dropping a value of the enum `item` is guarded: by its own
/// `Drop`, or, when it never leaves its owners, by every owner. A struct
/// owner is guarded when its own `Drop` names each field holding the enum;
/// an enum owner when it is guarded in turn. An enum nothing holds, one
/// moved out of its owner, and one whose name is ambiguous need their own
/// `Drop`.
fn guarded(files: &[Gated], declared: &[Declared], item: &Declared, visiting: &mut BTreeSet<String>) -> bool {
    if has_drop(files, declared, item) {
        return true;
    }
    let name = &item.name;
    let owners: Vec<&Declared> = declared
        .iter()
        .filter(|owner| owner.name != *name)
        .filter(|owner| !word_offsets(&files[owner.file].code[owner.body.clone()], name).is_empty())
        .collect();
    let holding: BTreeSet<String> = owners
        .iter()
        .filter(|owner| !owner.is_enum)
        .flat_map(|owner| fields_holding(&files[owner.file].code[owner.body.clone()], name))
        .filter(|field| !field.bytes().all(|byte| byte.is_ascii_digit()))
        .collect();
    if !item.is_enum || ambiguous(declared, name) || moved_out(files, name, &holding)
        || !visiting.insert(name.clone())
    {
        return false;
    }
    let covered = !owners.is_empty()
        && owners.iter().all(|owner| {
            if owner.is_enum {
                return guarded(files, declared, owner, visiting);
            }
            let fields = fields_holding(&files[owner.file].code[owner.body.clone()], name);
            let bodies = drop_bodies(files, declared, owner);
            !fields.is_empty() && !bodies.is_empty() && fields.iter().all(|field| {
                bodies.iter().any(|(file, body)| drop_names_field(&files[*file], body.clone(), field))
            })
        });
    visiting.remove(name);
    covered
}

/// Every violation across `sources`, as the index of its source and
/// `line: reason`.
fn violations_across(sources: &[&str]) -> Vec<(usize, String)> {
    let files: Vec<Gated> = sources.iter().map(|source| Gated::new(source)).collect();
    let manual = manual_names(&files);
    let declared = declarations(&files);
    let mut found = Vec::new();
    for (index, file) in files.iter().enumerate() {
        for offset in word_offsets(&file.code, "forget") {
            found.push((index, format!("{}: forget bypasses the custody holder", line_of(&file.code, offset))));
        }
    }
    for item in &declared {
        let code = &files[item.file].code;
        if !names_any(&code[item.body.clone()], &manual) {
            continue;
        }
        let line = line_of(code, item.offset);
        let name = &item.name;
        if !item.is_enum {
            if !has_drop(&files, &declared, item) {
                found.push((item.file, format!("{line}: struct `{name}` holds a ManuallyDrop field but has no Drop")));
            }
        } else if !guarded(&files, &declared, item, &mut BTreeSet::new()) {
            found.push((item.file, format!(
                "{line}: enum `{name}` holds a ManuallyDrop payload but neither it nor every owner has a Drop"
            )));
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Every violation in one self-contained source, as `line: reason`.
fn violations(source: &str) -> Vec<String> {
    violations_across(&[source]).into_iter().map(|(_, violation)| violation).collect()
}

#[test]
fn crate_sources_hold_no_forget_and_no_undropped_manual_owner() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_files_under(&src);
    for required in [
        "sqlite_schema.rs",
        "projection/generation_lease.rs",
        "state_db/live_native.rs",
        "state_db/live_close.rs",
        "state_db/destination_join.rs",
        "operational/live_native.rs",
        "operational/original.rs",
    ] {
        assert!(files.contains(&src.join(required)), "gate lost {required}");
    }
    let sources: Vec<String> = files
        .iter()
        .map(|path| std::fs::read_to_string(path).expect("gated source"))
        .collect();
    let borrowed: Vec<&str> = sources.iter().map(String::as_str).collect();
    let refused: Vec<String> = violations_across(&borrowed)
        .into_iter()
        .map(|(index, violation)| {
            format!("{}:{violation}", files[index].strip_prefix(&src).unwrap().display())
        })
        .collect();
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

#[test]
fn gate_guards_enums_with_manual_payloads() {
    let unguarded = "1: enum `Custody` holds a ManuallyDrop payload but neither it nor every owner has a Drop";
    // Unheld and undropped.
    assert_eq!(
        violations("enum Custody { Open(Lock), Closing { progress: ManuallyDrop<Progress> } }"),
        vec![unguarded]
    );
    // Its own Drop guards it.
    assert!(violations(
        "enum Custody { Closing(ManuallyDrop<P>) }\n\
         impl Drop for Custody { fn drop(&mut self) {} }"
    ).is_empty());
    // Every holding struct's Drop takes the field that holds it.
    let held = "enum Custody { Closing(ManuallyDrop<P>) }\n\
                struct Failure { custody: Custody }\n\
                impl Drop for Failure { fn drop(&mut self) { abandon(K, take(&mut self.custody)); } }\n\
                struct Other { held: Option<Custody> }\n";
    assert!(violations(&format!(
        "{held}impl Drop for Other {{ fn drop(&mut self) {{ abandon(K, self.held.take()); }} }}"
    )).is_empty());
    // One holder without a Drop leaves it unguarded.
    assert_eq!(violations(held), vec![unguarded]);
    // A holder whose Drop never names the field does not guard it.
    assert_eq!(
        violations(&format!("{held}impl Drop for Other {{ fn drop(&mut self) {{}} }}")),
        vec![unguarded]
    );
    // A Drop that names the field through a method it calls on self does.
    assert!(violations(
        "enum Custody { Closing(ManuallyDrop<P>) }\n\
         struct Failure(Lock, Custody);\n\
         impl Failure { fn take_whole(&mut self) -> Taken { Taken(take(&mut self.1)) } }\n\
         impl Drop for Failure { fn drop(&mut self) { abandon(K, self.take_whole()); } }"
    ).is_empty());
    // An enum holder is guarded through its own holders.
    let nested = "enum Custody { Held(ManuallyDrop<P>) }\n\
                  enum Outer { Inner(Custody), Empty }\n";
    assert!(violations(&format!(
        "{nested}struct Owner {{ outer: Outer }}\nimpl Drop for Owner {{ fn drop(&mut self) {{ take(&mut self.outer); }} }}"
    )).is_empty());
    assert_eq!(violations(nested), vec![unguarded]);
    // A test enum is a fixture.
    assert!(violations("#[cfg(test)]\nenum Fixture { Held(ManuallyDrop<P>) }").is_empty());
}

#[test]
fn gate_requires_its_own_drop_for_an_enum_moved_out_of_its_owner() {
    let unguarded = "1: enum `Custody` holds a ManuallyDrop payload but neither it nor every owner has a Drop";
    let owned = "enum Custody { Closing(ManuallyDrop<P>) }\n\
                 struct Failure { custody: Custody }\n\
                 impl Drop for Failure { fn drop(&mut self) { abandon(K, take(&mut self.custody)); } }\n";
    assert!(violations(owned).is_empty());
    // Returned whole from its owner.
    assert_eq!(
        violations(&format!("{owned}impl Failure {{ fn into_custody(self) -> Custody {{ todo() }} }}")),
        vec![unguarded]
    );
    // Returned inside a Result.
    assert_eq!(
        violations(&format!("{owned}fn retry() -> Result<Failure, (Custody, Error)> {{ todo() }}")),
        vec![unguarded]
    );
    // Bound to a local by annotation or by construction.
    assert_eq!(
        violations(&format!("{owned}fn f(x: X) {{ let held: Option<Custody> = x.get(); }}")),
        vec![unguarded]
    );
    assert_eq!(
        violations(&format!("{owned}fn f(p: P) {{ let held = Custody::Closing(wrap(p)); }}")),
        vec![unguarded]
    );
    // Its own Drop covers every move.
    assert!(violations(&format!(
        "{owned}impl Failure {{ fn into_custody(self) -> Custody {{ todo() }} }}\n\
         impl Drop for Custody {{ fn drop(&mut self) {{}} }}"
    )).is_empty());
}

#[test]
fn gate_matches_same_named_types_only_within_their_file() {
    // Two enums named alike: holders elsewhere cannot be shown to hold this
    // one, so it needs its own Drop.
    let across = violations_across(&[
        "enum Custody { Held(ManuallyDrop<P>) }\n\
         struct Failure { custody: Custody }\n\
         impl Drop for Failure { fn drop(&mut self) { take(&mut self.custody); } }",
        "pub enum Custody { Plain(Lock) }",
    ]);
    assert_eq!(across, vec![(0, "1: enum `Custody` holds a ManuallyDrop payload but neither it nor every owner has a Drop".to_string())]);
    // A Drop for a same-named struct in another file does not cover this one.
    let across = violations_across(&[
        "struct Owner { lock: ManuallyDrop<Lock> }",
        "struct Owner { lock: Lock }\nimpl Drop for Owner { fn drop(&mut self) {} }",
    ]);
    assert_eq!(across, vec![(0, "1: struct `Owner` holds a ManuallyDrop field but has no Drop".to_string())]);
    // The same Drop in its own file does.
    assert!(violations_across(&[
        "struct Owner { lock: ManuallyDrop<Lock> }\nimpl Drop for Owner { fn drop(&mut self) {} }",
        "struct Owner { lock: Lock }",
    ]).is_empty());
}

#[test]
fn gate_sees_through_aliases_and_renamed_imports() {
    let unguarded = "2: struct `Owner` holds a ManuallyDrop field but has no Drop";
    assert_eq!(
        violations("type Held<T> = std::mem::ManuallyDrop<T>;\nstruct Owner { lock: Held<Lock> }"),
        vec![unguarded]
    );
    assert_eq!(
        violations("use std::mem::ManuallyDrop as Kept;\nstruct Owner { lock: Kept<Lock> }"),
        vec![unguarded]
    );
    assert_eq!(
        violations(
            "use std::mem::{ManuallyDrop as Kept};\n\
             type Again = Kept<Lock>;\n\
             enum Custody { Held(Again) }"
        ),
        vec!["3: enum `Custody` holds a ManuallyDrop payload but neither it nor every owner has a Drop"]
    );
    // An alias defined in one file is seen in another.
    let across = violations_across(&[
        "pub(crate) type Held<T> = std::mem::ManuallyDrop<T>;",
        "struct Owner { lock: crate::Held<Lock> }",
    ]);
    assert_eq!(across, vec![(1, "1: struct `Owner` holds a ManuallyDrop field but has no Drop".to_string())]);
    // A plain cast or an unrelated alias is not a manual name.
    assert!(violations("type Count = u32;\nfn f(x: u8) -> u32 { x as u32 }\nstruct Plain { n: Count }").is_empty());
}

#[test]
fn gate_requires_its_own_drop_for_an_enum_taken_by_value_or_swapped_out() {
    let unguarded = "1: enum `Custody` holds a ManuallyDrop payload but neither it nor every owner has a Drop";
    let owned = "enum Custody { Closing(ManuallyDrop<P>), Empty }\n\
                 struct Failure { custody: Custody }\n\
                 impl Drop for Failure { fn drop(&mut self) { abandon(K, take(&mut self.custody)); } }\n";
    assert!(violations(owned).is_empty());
    // Passed by value into a function, bare or inside another type.
    for moved in [
        "fn settle(custody: Custody) {}",
        "fn settle(n: u8, mut held: Option<Custody>) {}",
        "fn settle<T: Into<u8>>(n: T, held: Vec<Custody>) -> u8 { 0 }",
        "impl Custody { fn finish(self) {} }",
        "impl Custody { fn finish(mut self, n: u8) {} }",
    ] {
        assert_eq!(violations(&format!("{owned}{moved}")), vec![unguarded], "{moved}");
    }
    // A reference does not move it, nor does a method borrowing it.
    for borrowed in [
        "fn look(custody: &Custody, other: &mut Option<Custody>) {}",
        "impl Custody { fn look(&self) {} fn change(&mut self) {} }",
        "impl Failure { fn finish(self) {} }",
    ] {
        assert!(violations(&format!("{owned}{borrowed}")).is_empty(), "{borrowed}");
    }
    // Bound to an unannotated local from a field that holds it.
    for swapped in [
        "impl Failure { fn f(&mut self) { let held = std::mem::replace(&mut self.custody, Custody::Empty); } }",
        "impl Failure { fn f(&mut self) { let held = mem::take(&mut self.custody); } }",
        "fn f(failure: &mut Failure) { let held = unsafe { ManuallyDrop::take(&mut failure.custody) }; }",
    ] {
        assert_eq!(violations(&format!("{owned}{swapped}")), vec![unguarded], "{swapped}");
    }
    let optional = "enum Custody { Closing(ManuallyDrop<P>) }\n\
                    struct Other { held: Option<Custody> }\n\
                    impl Drop for Other { fn drop(&mut self) { let held = self.held.take(); abandon(K, held); } }\n";
    assert!(violations(optional).is_empty());
    assert_eq!(
        violations(&format!("{optional}impl Other {{ fn f(&mut self) {{ let held = self.held.take(); }} }}")),
        vec![unguarded]
    );
    assert_eq!(
        violations(&format!("{optional}impl Other {{ fn f(&mut self, p: P) {{ let held = self.held.replace(p); }} }}")),
        vec![unguarded]
    );
    // An unrelated field of the same shape is not a move of it.
    assert!(violations(&format!(
        "{optional}struct Count {{ seen: Option<u8> }}\n\
         impl Count {{ fn f(&mut self) {{ let seen = self.seen.take(); }} }}"
    )).is_empty());
}

#[test]
fn gate_requires_the_holding_field_on_every_drop_path() {
    let unguarded = "1: enum `Custody` holds a ManuallyDrop payload but neither it nor every owner has a Drop";
    let held = "enum Custody { Closing(ManuallyDrop<P>) }\n\
                struct Failure { custody: Custody, forked: bool, mode: Mode }\n";
    let with_drop = |body: &str| format!("{held}impl Drop for Failure {{ fn drop(&mut self) {{ {body} }} }}");
    for covered in [
        "if self.forked { relinquish(take(&mut self.custody)); } else { abandon(K, take(&mut self.custody)); }",
        "let custody = take(&mut self.custody); if self.forked { relinquish(custody); return; } abandon(K, custody);",
        "if self.forked { relinquish(self.take_whole()); return; } abandon(K, take(&mut self.custody));",
        "match self.mode { Mode::A => abandon(K, take(&mut self.custody)), Mode::B => { relinquish(take(&mut self.custody)); } }",
        "if self.forked { return relinquish(take(&mut self.custody)); } abandon(K, take(&mut self.custody));",
        "let custody = unsafe { take(&mut self.custody) }; if self.forked { relinquish(custody) } else { abandon(K, custody) }",
        "if a { take(&mut self.custody); } else if b { take(&mut self.custody); } else { abandon(K, take(&mut self.custody)); }",
    ] {
        let source = format!(
            "{}\nimpl Failure {{ fn take_whole(&mut self) -> T {{ take(&mut self.custody) }} }}",
            with_drop(covered)
        );
        assert!(violations(&source).is_empty(), "{covered}");
    }
    for partial in [
        "if self.forked { relinquish(other); } else { abandon(K, take(&mut self.custody)); }",
        "if settled { drop(take(&mut self.custody)); }",
        "if self.forked { return; } abandon(K, take(&mut self.custody));",
        "match self.mode { Mode::A => abandon(K, take(&mut self.custody)), Mode::B => {} }",
        "match self.mode { Mode::A => { abandon(K, take(&mut self.custody)); } Mode::B => drop(other), }",
        "if a { take(&mut self.custody); } else if b { take(&mut self.custody); }",
        "for slot in slots { abandon(K, take(&mut self.custody)); }",
    ] {
        assert_eq!(violations(&with_drop(partial)), vec![unguarded], "{partial}");
    }
}
