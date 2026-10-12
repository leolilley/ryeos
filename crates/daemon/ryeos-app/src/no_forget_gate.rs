//! Source gate: no forgetting or leaking of owners in the execution-history
//! reset (`execution_history_reset.rs` and every module under
//! `execution_history_reset/`, at any depth, offline policy included) or in
//! the original Runtime custody modules (any file under `runtime_db/` whose
//! own name, or the name of a directory between it and `runtime_db/`, starts
//! with `original_`).
//!
//! A live owner in these modules ends through a consuming release that
//! returns it on refusal, or through plain fields whose declaration order is
//! their disposal order. Refused outright, wherever they appear (test modules
//! included): any `forget` token, `leak`, `into_raw`, `MaybeUninit` (an
//! uninitialised slot never drops what it holds), and any `as` rename of
//! `ManuallyDrop`, `forget` or `std::mem`, because a renamed path hides every
//! later use from this gate.
//!
//! `ManuallyDrop` is accepted in four shapes only:
//! - a plain `use` of the type;
//! - a named binding whose type is a `ManuallyDrop` (a field, parameter or
//!   typed `let`);
//! - `let name = ManuallyDrop::new(..)`, or a struct-literal field
//!   `name: ManuallyDrop::new(..)` where `name` is itself such a binding;
//! - the consuming disposals `ManuallyDrop::into_inner`, `take` and `drop`.
//!
//! Every binding must be taken back by name through one of those disposals:
//! the ordered consuming disposal, where a method destructures the owner and
//! releases each field in a fixed order. A disposal inside a test module (one
//! under a `#[cfg(..)]` whose predicate names `test` other than as
//! `not(test)`) covers only a binding inside a test module, and a production
//! disposal only a production binding, so a test can never vouch for
//! production code. Any other `ManuallyDrop` token (a tuple field, a wrapper
//! passed into an expression, a method path) is refused, because nothing can
//! show its owner is ever taken.
//!
//! Out of scope: the guard-disarm forgets in lillux-sqlite (scope and
//! deadline guards disarmed on success), and custody modules outside the
//! paths above.

use regex::Regex;
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

/// Every source file the gate covers, relative to this crate's `src`.
fn gated_sources(src: &Path) -> Vec<PathBuf> {
    let mut files = vec![src.join("execution_history_reset.rs")];
    files.extend(rust_files_under(&src.join("execution_history_reset")));
    let runtime_db = src.join("runtime_db");
    files.extend(rust_files_under(&runtime_db).into_iter().filter(|path| {
        path.strip_prefix(&runtime_db).expect("runtime_db child").components().any(|component| {
            component.as_os_str().to_str().is_some_and(|name| name.starts_with("original_"))
        })
    }));
    files
}

/// Whether `byte` can continue an identifier.
fn ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Source text with comments, string contents and char-literal contents
/// blanked out (newlines kept for line numbers), so neither prose nor a
/// literal can satisfy or trip the gate, a `//` inside a string never hides
/// the code after it, and a quoted brace never unbalances a module.
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

/// Byte ranges of every test module body in stripped `code`: a module under
/// a `#[cfg(..)]` whose predicate names `test`, such as `cfg(test)` or
/// `cfg(all(test, target_os = "linux"))`, other than only as `not(test)`.
fn test_module_ranges(code: &str) -> Vec<std::ops::Range<usize>> {
    let opener = Regex::new(r"#\s*\[\s*cfg\s*\(([^\]]*)\)\s*\](?:\s*#\s*\[[^\]]*\])*\s*(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+\w+\s*\{")
        .unwrap();
    let negated = Regex::new(r"\bnot\s*\(\s*test\s*\)").unwrap();
    let names_test = Regex::new(r"\btest\b").unwrap();
    let bytes = code.as_bytes();
    let mut ranges = Vec::new();
    for capture in opener.captures_iter(code) {
        let hit = capture.get(0).unwrap();
        if !names_test.is_match(&negated.replace_all(&capture[1], "")) {
            continue;
        }
        let mut depth = 1usize;
        let mut index = hit.end();
        while index < bytes.len() && depth > 0 {
            match bytes[index] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            index += 1;
        }
        ranges.push(hit.start()..index);
    }
    ranges
}

/// Every violation in one file's source, as `line: reason`.
fn violations(source: &str) -> Vec<String> {
    let code = strip_comments(source);
    let tests = test_module_ranges(&code);
    let in_test = |offset: usize| tests.iter().any(|range| range.contains(&offset));
    let mut found = Vec::new();
    let refused = Regex::new(r"\b(forget|leak|into_raw|MaybeUninit)\b").unwrap();
    for hit in refused.find_iter(&code) {
        let reason = match hit.as_str() {
            "forget" => "mem::forget",
            "leak" => "leak",
            "MaybeUninit" => "MaybeUninit",
            _ => "into_raw",
        };
        found.push(format!("{}: {reason}", line_of(&code, hit.start())));
    }
    let renamed = Regex::new(r"\b(?:ManuallyDrop|mem|forget)\s+as\b").unwrap();
    for hit in renamed.find_iter(&code) {
        found.push(format!("{}: renamed import hides later uses", line_of(&code, hit.start())));
    }

    // Accepted ManuallyDrop shapes; each records the span of its token.
    let path = r"(?:(?:::)?(?:std|core)::mem::)?";
    let import = Regex::new(&format!(r"\buse\s+{path}ManuallyDrop\s*;")).unwrap();
    let named = Regex::new(&format!(r"\b(\w+)\s*:\s*(?:[\w:]+\s*<\s*)*?{path}ManuallyDrop\s*<")).unwrap();
    let local = Regex::new(&format!(r"\blet\s+(?:mut\s+)?(\w+)\s*=\s*{path}ManuallyDrop::new\s*\(")).unwrap();
    let field_init = Regex::new(&format!(r"\b(\w+)\s*:\s*{path}ManuallyDrop::new\s*\(")).unwrap();
    let disposal = Regex::new(&format!(
        r"\b{path}ManuallyDrop::(?:into_inner|take|drop)\s*\(\s*(?:&\s*mut\s+)?(?:[\w.]*\.)?(\w+)\s*\)"
    ))
    .unwrap();
    let token = Regex::new(r"\bManuallyDrop\b").unwrap();
    let token_in = |start: usize, end: usize| {
        token.find_at(&code, start).filter(|found| found.end() <= end).map(|found| found.start())
    };

    let mut accepted = Vec::new();
    for hit in import.find_iter(&code) {
        accepted.extend(token_in(hit.start(), hit.end()));
    }
    // (name, offset, in a test module)
    let mut bindings = Vec::new();
    for capture in named.captures_iter(&code) {
        let whole = capture.get(0).unwrap();
        accepted.extend(token_in(whole.start(), whole.end()));
        bindings.push((capture[1].to_owned(), whole.start(), in_test(whole.start())));
    }
    for capture in local.captures_iter(&code) {
        let whole = capture.get(0).unwrap();
        accepted.extend(token_in(whole.start(), whole.end()));
        bindings.push((capture[1].to_owned(), whole.start(), in_test(whole.start())));
    }
    let declared: Vec<String> = bindings.iter().map(|(name, _, _)| name.clone()).collect();
    for capture in field_init.captures_iter(&code) {
        let whole = capture.get(0).unwrap();
        if declared.contains(&capture[1].to_owned()) {
            accepted.extend(token_in(whole.start(), whole.end()));
        }
    }
    // (taken name, in a test module)
    let mut taken = Vec::new();
    for capture in disposal.captures_iter(&code) {
        let whole = capture.get(0).unwrap();
        accepted.extend(token_in(whole.start(), whole.end()));
        taken.push((capture[1].to_owned(), in_test(whole.start())));
    }
    for hit in token.find_iter(&code) {
        if !accepted.contains(&hit.start()) {
            found.push(format!(
                "{}: ManuallyDrop outside a named binding or consuming disposal cannot show its owner is taken",
                line_of(&code, hit.start())
            ));
        }
    }
    for (name, offset, test) in bindings {
        if !taken.iter().any(|(taken, taken_test)| *taken == name && *taken_test == test) {
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
    assert!(!files.contains(&src.join("runtime_db/inspection_cleanup.rs")), "gate widened past original_ modules");
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
fn live_state_and_operational_opening_sources_hold_no_forget_or_leaking_wrapper() {
    let state_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../state/ryeos-state/src");
    let mut refused = Vec::new();
    for relative in ["state_db/live_native.rs", "operational/live_native.rs"] {
        let source = std::fs::read_to_string(state_src.join(relative)).expect("gated State source");
        for violation in violations(&source) {
            refused.push(format!("{relative}:{violation}"));
        }
    }
    assert!(refused.is_empty(), "no-forget gate refused:\n{}", refused.join("\n"));
}

#[test]
fn gate_walks_nested_reset_and_original_directories() {
    let root = tempfile::tempdir().unwrap();
    let src = root.path();
    for file in [
        "execution_history_reset.rs",
        "execution_history_reset/a.rs",
        "execution_history_reset/nested/deeper/b.rs",
        "runtime_db/original_close.rs",
        "runtime_db/original_group/inner.rs",
        "runtime_db/inspection_cleanup.rs",
        "runtime_db/other/original_c.rs",
        "runtime_db/other/plain.rs",
    ] {
        let path = src.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
    }
    let mut files: Vec<String> = gated_sources(src)
        .iter()
        .map(|path| path.strip_prefix(src).unwrap().display().to_string())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "execution_history_reset.rs",
            "execution_history_reset/a.rs",
            "execution_history_reset/nested/deeper/b.rs",
            "runtime_db/original_close.rs",
            "runtime_db/original_group/inner.rs",
            "runtime_db/other/original_c.rs",
        ]
    );
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
            fn new(context: Context) -> Self {
                Self { context: std::mem::ManuallyDrop::new(Some(context)),
                    host: std::mem::ManuallyDrop::new(None), refusal: std::mem::ManuallyDrop::new(None) }
            }
            fn into_exhausted(self) -> Result<(), (Self, Error)> {
                let Self { context, host, refusal } = self;
                drop(std::mem::ManuallyDrop::into_inner(context));
                drop(std::mem::ManuallyDrop::into_inner(host));
                drop(std::mem::ManuallyDrop::into_inner(refusal));
                Ok(())
            }
        }
        use std::mem::ManuallyDrop;
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
        const BYTES: &[u8] = br#"a "quoted" Box::leak(x) and into_raw"#;
        const BRACE: char = '{';
        #[cfg(test)]
        mod tests {
            fn local(v: Vec<File>) {
                let held = std::mem::ManuallyDrop::new(v);
                drop(std::mem::ManuallyDrop::into_inner(held));
            }
        }
    "##;
    assert!(violations(source).is_empty(), "{:?}", violations(source));
    // Removing one step of the ordered disposal leaves that field leaking.
    let partial = source.replace("drop(std::mem::ManuallyDrop::into_inner(host));", "drop(host);");
    assert_eq!(violations(&partial), vec!["4: ManuallyDrop `host` is never taken by a consuming disposal"]);
}

#[test]
fn gate_refuses_forget_leak_and_untaken_wrappers() {
    assert_eq!(violations("fn f(owner: Owner) { std::mem::forget(owner); }"), vec!["1: mem::forget"]);
    assert_eq!(violations("use std::mem::forget;\nfn f(o: O) { forget(o) }").len(), 2);
    // A forget reached without a call, or through a function pointer.
    assert_eq!(violations("fn f() { let g = std::mem::forget::<Owner>; }"), vec!["1: mem::forget"]);
    assert_eq!(violations("fn f(o: Box<O>) -> &'static O { Box::leak(o) }"), vec!["1: leak"]);
    assert_eq!(violations("fn f(o: Arc<O>) -> *const O { Arc::into_raw(o) }"), vec!["1: into_raw"]);
    assert_eq!(
        violations("fn f(o: O) { let slot = std::mem::MaybeUninit::new(o); }"),
        vec!["1: MaybeUninit"]
    );
    assert_eq!(violations("use std::mem::MaybeUninit;"), vec!["1: MaybeUninit"]);
    assert_eq!(
        violations("struct Owner {\n    lock: std::mem::ManuallyDrop<Lock>,\n}"),
        vec!["2: ManuallyDrop `lock` is never taken by a consuming disposal"]
    );
    assert_eq!(
        violations("enum Proof { Closed(std::mem::ManuallyDrop<Arc<Closed>>) }"),
        vec!["1: ManuallyDrop outside a named binding or consuming disposal cannot show its owner is taken"]
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

#[test]
fn gate_refuses_wrappers_hidden_in_expressions_or_renames() {
    let unbound = "1: ManuallyDrop outside a named binding or consuming disposal cannot show its owner is taken";
    // A wrapper built inside an expression has no binding to take back.
    assert_eq!(violations("fn f(v: V) { keep(ManuallyDrop::new(v)); }"), vec![unbound]);
    assert_eq!(violations("fn f(v: V) { let held = Some(std::mem::ManuallyDrop::new(v)); }"), vec![unbound]);
    // A struct-literal field must name a declared, taken binding.
    assert_eq!(violations("fn f(v: V) -> S { S { stray: ManuallyDrop::new(v) } }"), vec![unbound]);
    // Renames hide every later use.
    assert_eq!(
        violations("use std::mem::ManuallyDrop as Md;"),
        vec!["1: ManuallyDrop outside a named binding or consuming disposal cannot show its owner is taken",
            "1: renamed import hides later uses"]
    );
    assert_eq!(violations("use std::mem as m;\nfn f(o: O) { m::drop(o) }"), vec!["1: renamed import hides later uses"]);
    assert!(violations("use std::mem::forget as release;").contains(&"1: renamed import hides later uses".to_owned()));
}

#[test]
fn gate_keeps_test_and_production_disposals_apart() {
    // A test-module disposal does not vouch for a production binding.
    let production = "struct Owner { lock: ManuallyDrop<Lock> }\n\
        #[cfg(test)]\n\
        mod tests {\n\
            fn t(o: Owner) { let Owner { lock } = o; drop(ManuallyDrop::into_inner(lock)); }\n\
        }";
    assert_eq!(violations(production), vec!["1: ManuallyDrop `lock` is never taken by a consuming disposal"]);
    // Test modules are gated too: an untaken test wrapper is refused.
    let test = "#[cfg(test)]\nmod tests {\n    fn t(v: V) { let held = ManuallyDrop::new(v); }\n}";
    assert_eq!(violations(test), vec!["3: ManuallyDrop `held` is never taken by a consuming disposal"]);
    assert_eq!(violations("#[cfg(test)]\nmod tests {\n    fn t(o: O) { std::mem::forget(o) }\n}"), vec!["3: mem::forget"]);
    // A compound predicate naming `test` is a test module too: its disposal
    // does not vouch for a production binding.
    let compound = "struct Owner { lock: ManuallyDrop<Lock> }\n\
        #[cfg(all(test, target_os = \"linux\"))]\n\
        mod linux_tests {\n\
            fn t(o: Owner) { let Owner { lock } = o; drop(ManuallyDrop::into_inner(lock)); }\n\
        }";
    assert_eq!(violations(compound), vec!["1: ManuallyDrop `lock` is never taken by a consuming disposal"]);
    // A test binding in such a module is taken by its own test disposal.
    let compound_test = "#[cfg(all(test, target_os = \"linux\"))]\nmod linux_tests {\n\
        fn t(v: V) { let held = ManuallyDrop::new(v); drop(ManuallyDrop::into_inner(held)); }\n}";
    assert!(violations(compound_test).is_empty(), "{:?}", violations(compound_test));
    // `not(test)` is production code, not a test module.
    let negated = "#[cfg(not(test))]\nmod live {\n\
        fn t(v: V) { let held = ManuallyDrop::new(v); }\n}\n\
        #[cfg(test)]\nmod tests {\n    fn u(v: V) { drop(ManuallyDrop::into_inner(held)); }\n}";
    assert_eq!(violations(negated), vec!["3: ManuallyDrop `held` is never taken by a consuming disposal"]);
}
