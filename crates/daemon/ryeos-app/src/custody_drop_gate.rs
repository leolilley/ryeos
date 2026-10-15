//! Source gate: every production struct or enum in this crate that holds a
//! `ManuallyDrop` field has its own `Drop`, every shell move-out moves every
//! field it leaves behind, and no `forget` appears anywhere in the crate's
//! source, test modules included.
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
//! `ManuallyDrop` is recognised under any name that reaches it: a renamed
//! import (`use std::mem::ManuallyDrop as Kept`, braced or not) or a type
//! alias whose definition names it, transitively, from any file in the
//! crate.
//!
//! A move-out helper builds an unowned shell, `let shell =
//! ManuallyDrop::new(owner)`, and moves fields out of it one by one. The
//! shell is never dropped, so a field the helper does not move is lost
//! silently. Every such shell (any binding of a `ManuallyDrop::new(..)`
//! whose fields are then reached through it) must move every field of its
//! owner: by `ManuallyDrop::take`, `ptr::read`, `mem::take` or
//! `mem::replace` of that field, or by moving the whole owner out. Merely
//! mentioning a field is not a move; only a scalar field (an integer,
//! `bool`, `char` or float, copied whole by any read) may be read plainly.
//! The owner's type comes from `self` in an `impl` or from the binding's
//! declared type, and must name exactly one struct declared in the same
//! file; an undeterminable or ambiguous owner is refused.
//!
//! Items inside a test item (any item under a `#[cfg(..)]` whose predicate
//! names `test` other than only as `not(test)`) are fixtures and exempt from
//! the `Drop` and shell rules, but not from the `forget` ban.

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

/// The identifier starting at `start` in `code`, possibly empty.
fn ident_at(code: &str, start: usize) -> &str {
    let length = code[start..].bytes().take_while(|&byte| ident_byte(byte)).count();
    &code[start..start + length]
}

/// The identifier that follows `offset` after any whitespace.
fn next_ident(code: &str, offset: usize) -> &str {
    let rest = &code[offset..];
    ident_at(code, offset + rest.len() - rest.trim_start().len())
}

/// Every name in stripped `code` that reaches `ManuallyDrop`: the type
/// itself, the names in `inherited`, and, to a fixpoint, each `as` rename of
/// a reaching name and each `type` alias whose definition names one.
fn manual_drop_names(code: &str, inherited: &[String]) -> Vec<String> {
    let mut names = vec!["ManuallyDrop".to_string()];
    for name in inherited {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    loop {
        let mut added = Vec::new();
        for name in &names {
            for at in word_offsets(code, name) {
                let after = at + name.len();
                let rest = &code[after..];
                let gap = rest.len() - rest.trim_start().len();
                if ident_at(code, after + gap) == "as" {
                    let renamed = next_ident(code, after + gap + "as".len());
                    if !renamed.is_empty() && renamed != "_" {
                        added.push(renamed.to_string());
                    }
                }
            }
        }
        for at in word_offsets(code, "type") {
            let alias = next_ident(code, at + "type".len());
            let end = code[at..].find(';').map_or(code.len(), |close| at + close);
            let Some(equals) = code[at..end].find('=').map(|offset| at + offset) else {
                continue;
            };
            if !alias.is_empty()
                && names.iter().any(|name| !word_offsets(&code[equals..end], name).is_empty())
            {
                added.push(alias.to_string());
            }
        }
        let before = names.len();
        for name in added {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        if names.len() == before {
            return names;
        }
    }
}

/// Whether `span` names any of `names` as a whole identifier.
fn names_any(span: &str, names: &[String]) -> bool {
    names.iter().any(|name| !word_offsets(span, name).is_empty())
}

/// The fields of the struct whose declaration starts at `start`, in order,
/// as `(name, type)`: identifiers for a braced struct, positions for a tuple
/// struct.
fn struct_fields(code: &str, start: usize) -> Vec<(String, String)> {
    let end = struct_end(code, start);
    let Some(open) = code[start..end].find(['{', '(', ';']).map(|offset| start + offset) else {
        return Vec::new();
    };
    let tuple = code.as_bytes()[open] == b'(';
    if code.as_bytes()[open] == b';' {
        return Vec::new();
    }
    let body = &code[open + 1..end.saturating_sub(1)];
    let bytes = body.as_bytes();
    let mut segments = Vec::new();
    let mut depth = 0usize;
    let mut from = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'-' if bytes.get(index + 1) == Some(&b'>') => index += 1,
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' | b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                segments.push(&body[from..index]);
                from = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    segments.push(&body[from..]);
    segments
        .into_iter()
        .filter(|segment| !segment.trim().is_empty())
        .enumerate()
        .filter_map(|(position, segment)| {
            let mut rest = segment.trim_start();
            while rest.starts_with("#[") {
                rest = rest[rest.find(']').map_or(rest.len(), |close| close + 1)..].trim_start();
            }
            if let Some(after) = rest.strip_prefix("pub") {
                rest = after.trim_start();
                if rest.starts_with('(') {
                    rest = rest[rest.find(')').map_or(rest.len(), |close| close + 1)..].trim_start();
                }
            }
            if tuple {
                return Some((position.to_string(), rest.trim().to_string()));
            }
            let name = ident_at(rest, 0);
            if name.is_empty() {
                return None;
            }
            let field_type = rest[name.len()..].trim_start().strip_prefix(':').unwrap_or_default().trim();
            Some((name.to_string(), field_type.to_string()))
        })
        .collect()
}

/// Whether a field of type `field_type` is a scalar, copied whole by any
/// read and owning nothing.
fn is_scalar(field_type: &str) -> bool {
    matches!(
        field_type.trim(),
        "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64" | "i128"
            | "isize" | "bool" | "char" | "f32" | "f64"
    )
}

/// Index of the first non-whitespace byte at or after `offset`.
fn skip_space(code: &str, offset: usize) -> usize {
    let rest = &code[offset..];
    offset + rest.len() - rest.trim_start().len()
}

/// The last path segment of a written type, references, `mut` and generic
/// arguments removed.
fn type_name(written: &str) -> String {
    let mut written = written.trim().trim_start_matches('&').trim_start();
    if let Some(after) = written.strip_prefix("mut ") {
        written = after.trim_start();
    }
    let path = written.split(['<', ' ', '\n', '(', '[']).next().unwrap_or_default();
    path.rsplit("::").next().unwrap_or_default().to_string()
}

/// The declared type of binding `name` in `span`: a `name: Type` parameter or
/// typed `let`, read to its closing `,`, `)`, `=`, `;` or `{`.
fn binding_type(span: &str, name: &str) -> Option<String> {
    word_offsets(span, name).into_iter().find_map(|at| {
        let colon = skip_space(span, at + name.len());
        let rest = &span[colon..];
        if !rest.starts_with(':') || rest.starts_with("::") {
            return None;
        }
        let mut depth = 0usize;
        let written: String = rest[1..]
            .chars()
            .take_while(|&c| {
                match c {
                    '<' | '(' | '[' => depth += 1,
                    '>' | ')' | ']' if depth > 0 => depth -= 1,
                    ',' | ')' | '=' | ';' | '{' if depth == 0 => return false,
                    _ => {}
                }
                true
            })
            .collect();
        let named = type_name(&written);
        (!named.is_empty()).then_some(named)
    })
}

/// Index of the `)` closing the `(` at `open`.
fn closing_paren(code: &str, open: usize) -> usize {
    let mut depth = 0usize;
    for (index, byte) in code.bytes().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    code.len()
}

/// Whether `body` moves field `field` out of `shell`, or moves the whole
/// owner out, by one of the moves the shell rule accepts.
fn shell_moves(body: &str, shell: &str, field: &str, field_type: &str, manual: &[String]) -> bool {
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    let whole = manual.iter().any(|name| {
        compact.contains(&format!("{name}::into_inner({shell})"))
            || compact.contains(&format!("{name}::take(&mut{shell})"))
    }) || compact.contains(&format!("ptr::read(&*{shell})"));
    let moved = manual.iter().any(|name| compact.contains(&format!("{name}::take(&mut{shell}.{field})")))
        || [
            format!("ptr::read(&{shell}.{field})"),
            format!("ptr::read(&mut{shell}.{field})"),
            format!("mem::take(&mut{shell}.{field})"),
            format!("mem::replace(&mut{shell}.{field},"),
        ]
        .iter()
        .any(|pattern| compact.contains(pattern.as_str()));
    let scalar_read = is_scalar(field_type)
        && word_offsets(body, shell).into_iter().any(|at| {
            let after = at + shell.len();
            body[after..].starts_with('.') && ident_at(body, after + 1) == field
        });
    whole || moved || scalar_read
}

/// Index just past the first balanced `{..}` block at or after `start`.
fn block_end(code: &str, start: usize) -> usize {
    let bytes = code.as_bytes();
    let Some(open) = code[start..].find('{').map(|offset| start + offset) else {
        return bytes.len();
    };
    let mut braces = 0usize;
    for (index, &byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'{' => braces += 1,
            b'}' => {
                braces -= 1;
                if braces == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
    }
    bytes.len()
}

/// The start of the innermost item introduced by `keyword` whose body
/// contains `offset`.
fn enclosing(code: &str, keyword: &str, offset: usize) -> Option<usize> {
    word_offsets(code, keyword)
        .into_iter()
        .filter(|&start| start < offset && block_end(code, start) > offset)
        .last()
}

/// The self type named by the `impl` header starting at `start`: the last
/// path segment of the implementing type, generics and trait removed.
fn impl_self_type(code: &str, start: usize) -> String {
    let open = code[start..].find('{').map_or(code.len(), |offset| start + offset);
    let mut header = code[start + "impl".len()..open].trim_start();
    if header.starts_with('<') {
        let mut depth = 0usize;
        let mut cut = header.len();
        for (index, byte) in header.bytes().enumerate() {
            match byte {
                b'<' => depth += 1,
                b'>' => {
                    depth -= 1;
                    if depth == 0 {
                        cut = index + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        header = &header[cut..];
    }
    if let Some(trait_end) = word_offsets(header, "for").last() {
        header = &header[trait_end + "for".len()..];
    }
    let header = word_offsets(header, "where").first().map_or(header, |&clause| &header[..clause]);
    let path = header.trim().split(['<', ' ', '\n']).next().unwrap_or_default();
    path.rsplit("::").next().unwrap_or_default().to_string()
}

/// Every violation in one file's source, as `line: reason`.
fn violations(source: &str) -> Vec<String> {
    violations_with(source, &[])
}

/// Every violation in one file's source, with `inherited` names that reach
/// `ManuallyDrop` from elsewhere in the crate, as `line: reason`.
fn violations_with(source: &str, inherited: &[String]) -> Vec<String> {
    let code = strip_comments(source);
    let tests = test_item_ranges(&code);
    let in_test = |offset: usize| tests.iter().any(|range| range.contains(&offset));
    let manual = manual_drop_names(&code, inherited);
    let mut found = Vec::new();
    for offset in word_offsets(&code, "forget") {
        found.push(format!("{}: forget bypasses the custody holder", line_of(&code, offset)));
    }
    for keyword in ["struct", "enum"] {
        for offset in word_offsets(&code, keyword) {
            if in_test(offset) {
                continue;
            }
            let name = next_ident(&code, offset + keyword.len());
            if name.is_empty() {
                continue;
            }
            let end = struct_end(&code, offset);
            if !names_any(&code[offset..end], &manual) {
                continue;
            }
            let has_drop = word_offsets(&code, name).into_iter().any(|at| {
                !in_test(at) && code[..at].trim_end().ends_with("for")
                    && code[..at].trim_end().trim_end_matches("for").trim_end().ends_with("Drop")
            });
            if !has_drop {
                found.push(format!(
                    "{}: {keyword} `{name}` holds a ManuallyDrop field but has no Drop",
                    line_of(&code, offset)
                ));
            }
        }
    }
    for binding in word_offsets(&code, "let") {
        if in_test(binding) {
            continue;
        }
        let mut cursor = skip_space(&code, binding + "let".len());
        if ident_at(&code, cursor) == "mut" {
            cursor = skip_space(&code, cursor + "mut".len());
        }
        let shell = ident_at(&code, cursor);
        if shell.is_empty() {
            continue;
        }
        cursor = skip_space(&code, cursor + shell.len());
        if !code[cursor..].starts_with('=') || code[cursor..].starts_with("==") {
            continue;
        }
        cursor = skip_space(&code, cursor + 1);
        let path_length = code[cursor..].bytes().take_while(|&byte| ident_byte(byte) || byte == b':').count();
        let Some(wrapper) = code[cursor..cursor + path_length].strip_suffix("::new") else { continue };
        let wrapper = wrapper.rsplit("::").next().unwrap_or_default();
        if !manual.iter().any(|name| name == wrapper) {
            continue;
        }
        let open = skip_space(&code, cursor + path_length);
        if !code[open..].starts_with('(') {
            continue;
        }
        let wrapped = code[open + 1..closing_paren(&code, open)].trim();
        let Some(function) = enclosing(&code, "fn", binding) else { continue };
        let body_start = code[function..].find('{').map_or(code.len(), |offset| function + offset);
        let body = &code[function..block_end(&code, function)];
        // A binding whose fields are never reached is a wrapper moved whole,
        // not a shell.
        let reaches_fields = word_offsets(body, shell).into_iter().any(|at| {
            let after = at + shell.len();
            let field = if body[after..].starts_with('.') { ident_at(body, after + 1) } else { "" };
            !field.is_empty() && !body[skip_space(body, after + 1 + field.len())..].starts_with('(')
        });
        if !reaches_fields {
            continue;
        }
        let line = line_of(&code, binding);
        let owner = if wrapped == "self" {
            enclosing(&code, "impl", function).map(|start| impl_self_type(&code, start))
        } else if !wrapped.is_empty() && ident_at(wrapped, 0) == wrapped {
            binding_type(&code[function..body_start], wrapped).or_else(|| binding_type(body, wrapped))
        } else {
            None
        };
        let Some(owner) = owner.filter(|owner| !owner.is_empty()) else {
            found.push(format!("{line}: shell `{shell}` empties an owner whose type cannot be determined"));
            continue;
        };
        let declarations: Vec<usize> = word_offsets(&code, "struct")
            .into_iter()
            .filter(|&start| !in_test(start) && next_ident(&code, start + "struct".len()) == owner)
            .collect();
        let declaration = match declarations.as_slice() {
            [declaration] => *declaration,
            [] => {
                found.push(format!("{line}: shell `{shell}` empties `{owner}`, which this file does not declare"));
                continue;
            }
            _ => {
                found.push(format!("{line}: shell `{shell}` empties `{owner}`, which this file declares more than once"));
                continue;
            }
        };
        for (field, field_type) in struct_fields(&code, declaration) {
            if !shell_moves(body, shell, &field, &field_type, &manual) {
                found.push(format!("{line}: shell move-out of `{owner}` never moves field `{field}`"));
            }
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
    for required in [
        "accounting_db/native.rs",
        "temp_dir_guard.rs",
        "runtime_db/original_close.rs",
        "state_store/live_destination_join.rs",
        "state_store/live_outside.rs",
        "state_store/live_native.rs",
        "state_store/live_close.rs",
    ] {
        assert!(files.contains(&src.join(required)), "gate lost {required}");
    }
    let sources: Vec<String> =
        files.iter().map(|path| std::fs::read_to_string(path).expect("gated source")).collect();
    // A rename or alias declared in one file reaches every other file.
    let mut inherited = Vec::new();
    for source in &sources {
        for name in manual_drop_names(&strip_comments(source), &[]) {
            if !inherited.contains(&name) {
                inherited.push(name);
            }
        }
    }
    let mut refused = Vec::new();
    for (path, source) in files.iter().zip(&sources) {
        for violation in violations_with(source, &inherited) {
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
fn gate_requires_a_drop_for_each_manual_enum() {
    let source = "enum Slot {\n\
            Empty,\n\
            Held { inputs: Option<ManuallyDrop<Inputs>> },\n\
        }\n\
        enum Covered { Held(std::mem::ManuallyDrop<Inputs>) }\n\
        impl Drop for Covered { fn drop(&mut self) {} }\n\
        enum Plain { Held(Inputs) }";
    assert_eq!(violations(source), vec!["1: enum `Slot` holds a ManuallyDrop field but has no Drop"]);
}

#[test]
fn gate_sees_manual_drop_through_renames_and_aliases() {
    assert_eq!(
        violations("use std::mem::ManuallyDrop as Kept;\nstruct Owner { lock: Kept<Lock> }"),
        vec!["2: struct `Owner` holds a ManuallyDrop field but has no Drop"]
    );
    assert_eq!(
        violations("use std::mem::{self, ManuallyDrop as Md};\nenum Slot { Held(Md<Lock>) }"),
        vec!["2: enum `Slot` holds a ManuallyDrop field but has no Drop"]
    );
    // A type alias, and an alias of that alias.
    assert_eq!(
        violations("type Held<T> = std::mem::ManuallyDrop<T>;\ntype Again<T> = Held<T>;\n\
            struct Owner { lock: Again<Lock> }"),
        vec!["3: struct `Owner` holds a ManuallyDrop field but has no Drop"]
    );
    // A name declared in another file reaches this one.
    assert_eq!(
        violations_with("struct Owner { lock: Kept<Lock> }", &["Kept".to_string()]),
        vec!["1: struct `Owner` holds a ManuallyDrop field but has no Drop"]
    );
    // An unrelated alias is not a manual owner.
    assert!(violations("type Held<T> = Option<T>;\nstruct Owner { lock: Held<Lock> }").is_empty());
}

#[test]
fn gate_requires_shell_move_outs_to_move_every_field() {
    let owner = "struct Owner { process: u32, error: anyhow::Error, lock: ManuallyDrop<Lock>, rest: Vec<(A, B)> }\n\
        impl Drop for Owner { fn drop(&mut self) {} }\n";
    let complete = format!("{owner}impl Owner {{\n\
            fn into_parts(self) -> Parts {{\n\
                let mut shell = std::mem::ManuallyDrop::new(self);\n\
                unsafe {{ Parts {{ process: shell.process, error: std::ptr::read(&shell.error),\n\
                    lock: ManuallyDrop::take(&mut shell.lock), rest: std::mem::take(&mut shell.rest) }} }}\n\
            }}\n\
        }}");
    assert!(violations(&complete).is_empty(), "{:?}", violations(&complete));
    let missing = format!("{owner}impl Owner {{\n\
            fn into_parts(self) -> (anyhow::Error, Lock) {{\n\
                let mut shell = std::mem::ManuallyDrop::new(self);\n\
                unsafe {{ (std::ptr::read(&shell.error), ManuallyDrop::take(&mut shell.lock)) }}\n\
            }}\n\
        }}");
    assert_eq!(violations(&missing), vec![
        "5: shell move-out of `Owner` never moves field `process`",
        "5: shell move-out of `Owner` never moves field `rest`",
    ]);
    // Mentioning a non-scalar field is not moving it, and a take alone is
    // checked just like a read.
    let mentioned = format!("{owner}impl Owner {{\n\
            fn into_lock(self) -> (usize, Lock) {{\n\
                let mut shell = ManuallyDrop::new(self);\n\
                let _ = (shell.process, &shell.error);\n\
                (shell.rest.len(), unsafe {{ ManuallyDrop::take(&mut shell.lock) }})\n\
            }}\n\
        }}");
    assert_eq!(violations(&mentioned), vec![
        "5: shell move-out of `Owner` never moves field `error`",
        "5: shell move-out of `Owner` never moves field `rest`",
    ]);
    // A wrapper never reached through its fields is moved whole, not a shell.
    let whole = format!("{owner}impl Owner {{\n\
            fn wrap(self) -> ManuallyDrop<Owner> {{ let held = ManuallyDrop::new(self); held }}\n\
            fn back(self) -> Owner {{ let held = ManuallyDrop::new(self); ManuallyDrop::into_inner(held) }}\n\
        }}");
    assert!(violations(&whole).is_empty(), "{:?}", violations(&whole));
    // Tuple owners name positions, and a renamed wrapper builds a shell too.
    let tuple = "use std::mem::ManuallyDrop as Md;\n\
        struct Pair(Md<Lock>, Vec<u8>);\n\
        impl Drop for Pair { fn drop(&mut self) {} }\n\
        impl Pair { fn split(self) -> (Lock, Vec<u8>) { let mut shell = Md::new(self);\n\
            unsafe { (Md::take(&mut shell.0), std::ptr::read(&shell.1)) } } }\n\
        impl Pair { fn bytes(self) -> Vec<u8> { let shell = Md::new(self);\n\
            unsafe { std::ptr::read(&shell.1) } } }";
    assert_eq!(violations(tuple), vec!["6: shell move-out of `Pair` never moves field `0`"]);
}

#[test]
fn gate_checks_shells_of_any_binding_and_refuses_unknown_owners() {
    let owner = "struct Owner { error: anyhow::Error, lock: ManuallyDrop<Lock> }\n\
        impl Drop for Owner { fn drop(&mut self) {} }\n";
    // A parameter's declared type names the owner.
    let parameter = format!("{owner}fn split(owner: Owner) -> Lock {{\n\
            let mut shell = std::mem::ManuallyDrop::new(owner);\n\
            unsafe {{ ManuallyDrop::take(&mut shell.lock) }}\n\
        }}");
    assert_eq!(violations(&parameter), vec!["4: shell move-out of `Owner` never moves field `error`"]);
    // So does a typed `let`, through its path.
    let typed = format!("{owner}fn split(raw: Raw) -> (anyhow::Error, Lock) {{\n\
            let owner: crate::custody::Owner = raw.into();\n\
            let mut shell = ManuallyDrop::new(owner);\n\
            unsafe {{ (std::ptr::read(&shell.error), ManuallyDrop::take(&mut shell.lock)) }}\n\
        }}");
    assert!(violations(&typed).is_empty(), "{:?}", violations(&typed));
    // An expression whose type the gate cannot see is refused.
    let unknown = format!("{owner}fn split(raw: Raw) -> Lock {{\n\
            let mut shell = ManuallyDrop::new(raw.into_owner());\n\
            unsafe {{ ManuallyDrop::take(&mut shell.lock) }}\n\
        }}");
    assert_eq!(violations(&unknown), vec!["4: shell `shell` empties an owner whose type cannot be determined"]);
    // An owner declared elsewhere, or twice, is refused rather than guessed.
    let foreign = "fn split(owner: Elsewhere) -> Lock {\n\
            let mut shell = ManuallyDrop::new(owner);\n\
            unsafe { ManuallyDrop::take(&mut shell.lock) }\n\
        }";
    assert_eq!(violations(foreign), vec!["2: shell `shell` empties `Elsewhere`, which this file does not declare"]);
    let twice = format!("{owner}mod other {{ struct Owner {{ lock: Lock }} }}\n\
        fn split(owner: Owner) -> Lock {{\n\
            let mut shell = ManuallyDrop::new(owner);\n\
            unsafe {{ ManuallyDrop::take(&mut shell.lock) }}\n\
        }}");
    assert_eq!(violations(&twice), vec!["5: shell `shell` empties `Owner`, which this file declares more than once"]);
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
