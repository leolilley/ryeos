//! Guard for the premise behind group presence checks: no spawn joins an
//! existing process group. Every spawn starts a new group, so a group's leader
//! is its spawner's own direct child. This test fails when any workspace
//! source calls `setpgid` with a non-zero group argument or `process_group`
//! with a non-zero argument.

use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Source text with `//` comments blanked, so call-like text in comments is
/// ignored. A `//` inside a string literal blanks the rest of that line,
/// which can only hide text, never invent a call.
fn without_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| line.find("//").map_or(line, |at| &line[..at]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Arguments of the call whose opening parenthesis ends `text[..open]`.
fn call_arguments(text: &str, open: usize) -> Option<Vec<String>> {
    let mut depth = 0usize;
    let mut args = Vec::new();
    let mut current = String::new();
    for ch in text[open..].chars() {
        match ch {
            '(' | '[' | '{' => {
                depth += 1;
                if depth > 1 {
                    current.push(ch);
                }
            }
            ')' | ']' | '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    args.push(current.trim().to_owned());
                    return Some(args);
                }
                current.push(ch);
            }
            ',' if depth == 1 => args.push(std::mem::take(&mut current).trim().to_owned()),
            _ => current.push(ch),
        }
    }
    None
}

/// Offending calls in one source text: `.process_group(x)` with `x != 0`, and
/// `::setpgid(a, x)` with `x != 0`.
fn offending_calls(source: &str) -> Vec<String> {
    let text = without_line_comments(source);
    let mut found = Vec::new();
    for (needle, arg_index) in [(".process_group(", 0usize), ("::setpgid(", 1usize)] {
        for (at, _) in text.match_indices(needle) {
            let open = at + needle.len() - 1;
            let args = call_arguments(&text, open);
            let literal_zero = args
                .as_ref()
                .and_then(|args| args.get(arg_index))
                .is_some_and(|arg| arg == "0");
            if !literal_zero {
                found.push(format!("{needle}{:?}", args));
            }
        }
    }
    found
}

#[test]
fn no_spawn_joins_an_existing_process_group() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let this_file = Path::new(file!()).file_name().map(ToOwned::to_owned);
    let mut sources = Vec::new();
    rust_sources(&crates, &mut sources);
    assert!(sources.len() > 100, "workspace sources were not found");
    let mut violations = Vec::new();
    for path in sources {
        if path.file_name().map(ToOwned::to_owned) == this_file {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        for call in offending_calls(&source) {
            violations.push(format!("{}: {call}", path.display()));
        }
    }
    assert!(
        violations.is_empty(),
        "a non-zero process-group join breaks group presence reaping: {violations:#?}"
    );
}

#[test]
fn guard_flags_joins_and_accepts_new_groups() {
    assert!(offending_calls("libc::setpgid(0, 0)").is_empty());
    assert!(offending_calls("cmd.process_group(0);").is_empty());
    assert!(offending_calls("// libc::setpgid(0, leader)").is_empty());
    assert!(offending_calls("fn setpgid(a: i32, b: i32) {}").is_empty());
    assert_eq!(offending_calls("libc::setpgid(0, leader)").len(), 1);
    assert_eq!(offending_calls("libc::setpgid(pid, pid)").len(), 1);
    assert_eq!(offending_calls("cmd.process_group(pgid)").len(), 1);
}
