//! Source gate: no `mem::forget` and no leak-on-drop `ManuallyDrop` in the
//! execution-history reset (`execution_history_reset.rs` and every module
//! under `execution_history_reset/`, offline policy included) or in the
//! original Runtime custody modules (`runtime_db/original_*.rs`).
//!
//! A live owner in these modules ends through a consuming release that
//! returns it on refusal, or through plain fields whose declaration order is
//! their disposal order. A `ManuallyDrop` binding is accepted only when the
//! same file takes it back by name with `ManuallyDrop::into_inner`,
//! `ManuallyDrop::take` or `ManuallyDrop::drop`: the ordered consuming
//! disposal, where a method destructures the owner and releases each field in
//! a fixed order. A wrapper that is never taken leaks its owner and is
//! refused, as is one that no binding names (a tuple field), because nothing
//! can show it is ever taken.
//!
//! Out of scope: the guard-disarm forgets in lillux-sqlite (scope and
//! deadline guards disarmed on success), and custody modules outside the
//! paths above.

use regex::Regex;
use std::path::{Path, PathBuf};

/// Every source file the gate covers, relative to this crate's `src`.
fn gated_sources(src: &Path) -> Vec<PathBuf> {
    let mut files = vec![src.join("execution_history_reset.rs")];
    let mut reset: Vec<PathBuf> = std::fs::read_dir(src.join("execution_history_reset"))
        .expect("execution_history_reset module directory")
        .map(|entry| entry.expect("reset module entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect();
    reset.sort();
    files.extend(reset);
    let mut original: Vec<PathBuf> = std::fs::read_dir(src.join("runtime_db"))
        .expect("runtime_db module directory")
        .map(|entry| entry.expect("runtime_db entry").path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "rs")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("original_"))
        })
        .collect();
    original.sort();
    files.extend(original);
    files
}

/// Source text with comments and string contents blanked out (newlines kept
/// for line numbers), so neither prose nor a literal can satisfy or trip the
/// gate, and a `//` inside a string never hides the code after it.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let raw_hashes = (byte == b'r'
            && (index == 0 || !(bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_')))
            .then(|| bytes[index + 1..].iter().take_while(|&&b| b == b'#').count())
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
            // A char literal is skipped whole; a lifetime is kept as is.
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
            let end = end.unwrap_or(index + 1);
            out.push_str(&source[index..end]);
            index = end;
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

/// Every violation in one file's source, as `line: reason`.
fn violations(source: &str) -> Vec<String> {
    let code = strip_comments(source);
    let mut found = Vec::new();
    let forget = Regex::new(r"\bforget\s*\(|\bmem::forget\b").unwrap();
    for hit in forget.find_iter(&code) {
        found.push(format!("{}: mem::forget", line_of(&code, hit.start())));
    }
    let wrapper = Regex::new(r"\bManuallyDrop\s*<").unwrap();
    let named = Regex::new(
        r"\b(\w+)\s*:\s*(?:[\w:]+\s*<\s*)*?(?:(?:std|core)::mem::)?ManuallyDrop\s*<",
    )
    .unwrap();
    let local = Regex::new(
        r"\blet\s+(?:mut\s+)?(\w+)\s*=\s*(?:(?:std|core)::mem::)?ManuallyDrop::new\s*\(",
    )
    .unwrap();
    let mut covered = Vec::new();
    let mut bindings = Vec::new();
    for capture in named.captures_iter(&code) {
        let whole = capture.get(0).unwrap();
        covered.push(whole.end());
        bindings.push((capture[1].to_owned(), whole.start()));
    }
    for capture in local.captures_iter(&code) {
        bindings.push((capture[1].to_owned(), capture.get(0).unwrap().start()));
    }
    for hit in wrapper.find_iter(&code) {
        if !covered.contains(&hit.end()) {
            found.push(format!(
                "{}: ManuallyDrop without a named binding cannot show a consuming disposal",
                line_of(&code, hit.start())
            ));
        }
    }
    for (name, offset) in bindings {
        let disposal = Regex::new(&format!(
            r"\bManuallyDrop::(?:into_inner|take|drop)\s*\(\s*(?:&\s*mut\s+)?(?:[\w.]*\.)?{}\s*\)",
            regex::escape(&name)
        ))
        .unwrap();
        if !disposal.is_match(&code) {
            found.push(format!(
                "{}: ManuallyDrop `{name}` is never taken by a consuming disposal",
                line_of(&code, offset)
            ));
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn reset_and_original_runtime_sources_hold_no_forget_or_leaking_wrapper() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = gated_sources(&src);
    for required in [
        "execution_history_reset/original_quota_claim.rs",
        "execution_history_reset/creation_evidence.rs",
        "execution_history_reset/offline_policy.rs",
        "runtime_db/original_close.rs",
        "runtime_db/original_inspection.rs",
        "runtime_db/original_native.rs",
    ] {
        assert!(files.contains(&src.join(required)), "gate lost {required}");
    }
    let mut refused = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path).expect("gated source");
        for violation in violations(&source) {
            refused.push(format!("{}:{violation}", path.strip_prefix(&src).unwrap().display()));
        }
    }
    assert!(refused.is_empty(), "no-forget gate refused:\n{}", refused.join("\n"));
}

#[test]
fn gate_accepts_ordered_into_inner_disposal() {
    let source = r##"
        struct Frame {
            context: std::mem::ManuallyDrop<Option<Context>>,
            host: std::mem::ManuallyDrop<Option<Host>>,
            pub(super) refusal: std::mem::ManuallyDrop<Option<Error>>,
        }
        impl Frame {
            fn into_exhausted(self) -> Result<(), (Self, Error)> {
                let Self { context, host, refusal } = self;
                drop(std::mem::ManuallyDrop::into_inner(context));
                drop(std::mem::ManuallyDrop::into_inner(host));
                drop(std::mem::ManuallyDrop::into_inner(refusal));
                Ok(())
            }
        }
        fn pins(files: Vec<File>) {
            let mut files = std::mem::ManuallyDrop::new(files);
            drop(std::mem::ManuallyDrop::into_inner(files));
        }
        struct Slot { owner: ManuallyDrop<Owner> }
        impl Drop for Slot {
            fn drop(&mut self) { unsafe { std::mem::ManuallyDrop::drop(&mut self.owner) } }
        }
        // A comment naming std::mem::forget(x) or ManuallyDrop<T> is not code.
        /* Nor is a block comment: forget(owner); leak: ManuallyDrop<T> */
        const TEXT: &str = "mem::forget( is only text // ManuallyDrop<T>";
        const RAW: &str = r#"a "quoted" forget(owner) and ManuallyDrop<T>"#;
    "##;
    assert!(violations(source).is_empty(), "{:?}", violations(source));
    // Removing one step of the ordered disposal leaves that field leaking.
    let partial = source.replace("drop(std::mem::ManuallyDrop::into_inner(host));", "drop(host);");
    assert_eq!(violations(&partial), vec!["4: ManuallyDrop `host` is never taken by a consuming disposal"]);
}

#[test]
fn gate_refuses_forget_and_untaken_wrappers() {
    assert_eq!(violations("fn f(owner: Owner) { std::mem::forget(owner); }"), vec!["1: mem::forget"]);
    assert_eq!(violations("use std::mem::forget;\nfn f(o: O) { forget(o) }").len(), 2);
    assert_eq!(
        violations("struct Owner {\n    lock: std::mem::ManuallyDrop<Lock>,\n}"),
        vec!["2: ManuallyDrop `lock` is never taken by a consuming disposal"]
    );
    assert_eq!(
        violations("enum Proof { Closed(std::mem::ManuallyDrop<Arc<Closed>>) }"),
        vec!["1: ManuallyDrop without a named binding cannot show a consuming disposal"]
    );
    assert_eq!(
        violations("fn f(v: Vec<File>) { let pins = ManuallyDrop::new(v); }"),
        vec!["1: ManuallyDrop `pins` is never taken by a consuming disposal"]
    );
    // Taking a different binding of the same type does not cover this one.
    assert_eq!(
        violations(
            "struct A { root: ManuallyDrop<Dir>, lock: ManuallyDrop<Lock> }\n\
             fn f(a: A) { let A { root, lock } = a; drop(ManuallyDrop::into_inner(root)); }"
        ),
        vec!["1: ManuallyDrop `lock` is never taken by a consuming disposal"]
    );
}
