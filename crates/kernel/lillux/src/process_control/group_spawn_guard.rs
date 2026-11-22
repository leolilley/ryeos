//! Guard for the premise behind group presence checks: no spawn joins an
//! existing process group. Every spawn starts a new group, so a group's leader
//! is its spawner's own direct child. This test fails when any workspace
//! source calls `setpgid` with a non-zero group argument or `process_group`
//! with a non-zero argument. `setpgid(x, x)` is not a join: it makes `x` its
//! own group's leader, the same new group the child's own `setpgid(0, 0)`
//! makes, so a spawner may repeat the call on its child to close the window
//! before the child runs.

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

/// Whether an argument is a literal zero, however it is written: `0`,
/// `0i32`, `0_u32`, `0 as _`, `0 as libc::pid_t`.
fn is_literal_zero(arg: &str) -> bool {
    let value = arg.split(" as ").next().unwrap_or("").trim();
    let Some(suffix) = value.strip_prefix('0') else {
        return false;
    };
    matches!(
        suffix.trim_start_matches('_'),
        "" | "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize"
    )
}

fn is_identifier_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Offending calls in one source text. `setpgid(_, x)` is checked in any
/// spelling (path-qualified or brought in by `use`). `process_group(x)` is
/// checked as a method call, and `process_group(&mut command, x)` in
/// associated-function form. Both need `x` to be zero. Declarations and
/// unrelated identifiers that merely end in these names are ignored.
fn offending_calls(source: &str) -> Vec<String> {
    let text = without_line_comments(source);
    let mut found = Vec::new();
    for (name, is_setpgid) in [("setpgid(", true), ("process_group(", false)] {
        for (at, _) in text.match_indices(name) {
            let before = &text[..at];
            if before.ends_with(is_identifier_char) || before.trim_end().ends_with(" fn") {
                continue;
            }
            if before.trim_end().ends_with("fn") && before.ends_with(char::is_whitespace) {
                continue;
            }
            let open = at + name.len() - 1;
            let args = call_arguments(&text, open);
            let count = args.as_ref().map_or(0, Vec::len);
            let index = if is_setpgid {
                1
            } else if before.ends_with('.') {
                0
            } else if before.ends_with("::") || count >= 2 {
                1
            } else {
                // A one-argument free function of the same name is unrelated.
                continue;
            };
            let zero = args
                .as_ref()
                .and_then(|args| args.get(index))
                .is_some_and(|arg| is_literal_zero(arg));
            let own_group = is_setpgid
                && args
                    .as_ref()
                    .is_some_and(|args| args.len() == 2 && args[0] == args[1]);
            if !zero && !own_group {
                found.push(format!("{name}{args:?}"));
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
    for zero in ["0", "0i32", "0_u32", "0 as _", "0 as libc::pid_t"] {
        assert!(offending_calls(&format!("libc::setpgid(0, {zero})")).is_empty());
        assert!(offending_calls(&format!("setpgid(pid, {zero})")).is_empty());
        assert!(offending_calls(&format!("cmd.process_group({zero});")).is_empty());
        assert!(offending_calls(&format!("CommandExt::process_group(&mut c, {zero})")).is_empty());
    }
    // A leader making itself its own group is a new group, not a join.
    assert!(offending_calls("libc::setpgid(pid, pid)").is_empty());
    assert!(offending_calls("unsafe { libc::setpgid(child, child) };").is_empty());
    assert!(offending_calls("// libc::setpgid(0, leader)").is_empty());
    assert!(offending_calls("fn setpgid(a: i32, b: i32) {}").is_empty());
    assert!(offending_calls("let g = process_group(pid);").is_empty());
    assert!(offending_calls("quiesce_exact_process_group(&identity, t)").is_empty());
    for join in [
        "libc::setpgid(0, leader)",
        "libc::setpgid(pid, other)",
        "setpgid(0, leader)",
        "libc::setpgid(0, 1)",
        "cmd.process_group(pgid)",
        "cmd.process_group(1)",
        "CommandExt::process_group(&mut c, pgid)",
        "std::os::unix::process::CommandExt::process_group(&mut c, 7)",
    ] {
        assert_eq!(offending_calls(join).len(), 1, "{join}");
    }
}
