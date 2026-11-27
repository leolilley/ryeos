// Every route file that serves records or answers to a caller applies the
// node's secret floor, or is named here with the reason it serves nothing the
// floor could match. Every function that frames a persisted event for a
// stream does it through the floored envelope with the caller's floor, never
// with a literal "no floor". A new response mode, invoker or stream site
// cannot be added open: it either calls the floor or is listed with a reason.
//
// The production text of a file is the part before its top-level test module,
// the same cut crates/daemon/ryeos-ui/tests/live_path_sites_guard.rs uses (the
// shared source-text helper is not on this branch).

use std::path::{Path, PathBuf};

/// What counts as applying the floor at a serving site.
const FLOOR_CALLS: &[&str] = &[
    "caller_floor(",
    "SecretFloor",
    "floor_outcome(",
    "envelope_for_persisted_floored(",
    "mask_envelope(",
    "withhold_answer(",
    "withhold_error(",
    ".withhold(",
    ".mask(",
    "shape_events(",
];

/// Directories whose files are swept: the response modes and the route
/// invokers of the API crate, and the seat stream sources of the UI crate.
const SWEPT_DIRS: &[&str] = &[
    "crates/daemon/ryeos-api/src/routes/response_modes",
    "crates/daemon/ryeos-api/src/routes/invokers",
    "crates/daemon/ryeos-ui/src/invokers",
];

/// Single files swept with them: the seat stream policy every UI event stream
/// passes, and the floor's own gate.
const SWEPT_FILES: &[&str] = &[
    "crates/daemon/ryeos-ui/src/seat_stream_policy.rs",
    "crates/daemon/ryeos-api/src/routes/secret_floor_gate.rs",
];

/// Files that serve no record or answer a node secret could ride in, with why.
const SERVES_NOTHING_THE_FLOOR_COULD_MATCH: &[(&str, &str)] = &[
    (
        "crates/daemon/ryeos-api/src/routes/response_modes/mod.rs",
        "declares the response-mode modules and their registry; serves nothing",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/response_modes/browser_launch_mode.rs",
        "turns the answer of service:ui/launch into a cookie and a redirect; that answer already passed the executor's service floor and carries a session token, not run output",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/response_modes/channel_stream_mode.rs",
        "WebSocket exchange of one external occurrence's guest: every frame is a canonical signed supervisor frame answered by its canonical exchange response, authenticated by the attached binding rather than by a reader's key; it is not a reader of stored run output",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/response_modes/event_stream_mode.rs",
        "frames the envelopes its stream invoker produced as SSE and nothing else; every record source it hosts is an invoker in this sweep that applies the caller's floor",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/response_modes/json_mode.rs",
        "frames the Json answer of its invoker; the answers come from service_invocation.rs (floored by the service executor) and dispatch_invocation.rs (floored there, in this sweep)",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/response_modes/launch_mode.rs",
        "answers 202 with the minted thread id only; no run output reaches the response",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/response_modes/static_mode.rs",
        "serves embedded static assets; no run data",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/invokers/mod.rs",
        "declares the invoker modules and the auth-verifier registry; serves nothing",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/invokers/service_invocation.rs",
        "returns the answer of execute_service_verified, which applies the secret floor to every non-operator answer, recorded or not (disclose_service_result in the executor)",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/invokers/launch_invocation.rs",
        "returns RouteInvocationResult::Accepted: the thread id and launch status only",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/invokers/none_invocation.rs",
        "auth verifier: produces a principal, serves no record",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/invokers/hmac_invocation.rs",
        "auth verifier: produces a principal, serves no record",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/invokers/ryeos_signed_invocation.rs",
        "auth verifier: produces a principal, serves no record",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/invokers/external_occurrence_invocation.rs",
        "auth verifier for an occurrence's guest: produces a principal, serves no record",
    ),
    (
        "crates/daemon/ryeos-ui/src/invokers/mod.rs",
        "declares the UI invoker modules; serves nothing",
    ),
    (
        "crates/daemon/ryeos-ui/src/invokers/browser_session_invocation.rs",
        "auth verifier: turns a session cookie into a principal, serves no record",
    ),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace root")
        .to_owned()
}

/// Where a source file's test module starts: a `#[cfg(test)]` line directly
/// followed by `mod`, `pub mod` or `pub(crate) mod`.
fn test_module_start(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = text[from..].find("\n#[cfg(test)]\n") {
        let start = from + at;
        let next = text[start + "\n#[cfg(test)]\n".len()..].trim_start();
        if next.starts_with("mod ")
            || next.starts_with("pub mod ")
            || next.starts_with("pub(crate) mod ")
            || next.starts_with("pub(super) mod ")
        {
            return Some(start);
        }
        from = start + 1;
    }
    None
}

fn non_test_text(text: &str) -> &str {
    test_module_start(text).map_or(text, |end| &text[..end])
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if matches!(name.as_str(), "tests" | "target" | "benches" | "examples") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !name.ends_with("tests.rs")
            && !name.contains("test_support")
        {
            out.push(path);
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

fn applies_floor(body: &str) -> bool {
    FLOOR_CALLS.iter().any(|call| body.contains(call))
}

#[test]
fn every_serving_file_applies_the_floor_or_is_named_with_a_reason() {
    let root = workspace_root();
    let mut files = Vec::new();
    for dir in SWEPT_DIRS {
        rust_files(&root.join(dir), &mut files);
    }
    for file in SWEPT_FILES {
        files.push(root.join(file));
    }
    assert!(files.len() >= 25, "the sweep found {} files", files.len());

    let mut problems = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut applying = 0_usize;
    for file in &files {
        let name = relative(&root, file);
        let text = std::fs::read_to_string(file).unwrap();
        let floored = applies_floor(non_test_text(&text));
        applying += usize::from(floored);
        let listed = SERVES_NOTHING_THE_FLOOR_COULD_MATCH
            .iter()
            .any(|(path, _)| *path == name);
        seen.insert(name.clone());
        match (floored, listed) {
            (true, false) | (false, true) => {}
            (false, false) => problems.push(format!(
                "{name}: serves records or answers but applies no secret floor and is not in SERVES_NOTHING_THE_FLOOR_COULD_MATCH with a reason"
            )),
            (true, true) => problems.push(format!(
                "{name}: applies the floor, so it must come off the list of files that serve nothing"
            )),
        }
    }
    // The floor is applied at the sites this change covers; a cut marker or a
    // token that stops matching fails here instead of passing silently.
    assert!(
        applying >= 9,
        "only {applying} swept files apply the floor: the sweep or the call list is broken"
    );
    for (path, reason) in SERVES_NOTHING_THE_FLOOR_COULD_MATCH {
        assert!(!reason.is_empty(), "{path}: a listed file needs its reason");
        if !seen.contains(*path) {
            problems.push(format!(
                "{path}: listed but not in the sweep; drop or fix it"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The production functions of a source text, as `(first line, text)`. A
/// function ends where the next one starts, so a nested helper rides with the
/// function around it, which only makes the check stricter.
fn functions(text: &str) -> Vec<(usize, String)> {
    const STARTS: &[&str] = &[
        "fn ",
        "pub fn ",
        "pub(crate) fn ",
        "pub(super) fn ",
        "async fn ",
        "pub async fn ",
        "pub(crate) async fn ",
    ];
    let mut out: Vec<(usize, String)> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if STARTS.iter().any(|start| trimmed.starts_with(start)) {
            out.push((index + 1, String::new()));
        }
        if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

#[test]
fn every_event_framing_function_uses_the_floored_envelope_with_the_callers_floor() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    assert!(files.len() > 500, "the sweep found {} files", files.len());

    let mut problems = Vec::new();
    let mut floored_callers = std::collections::BTreeSet::new();
    for file in &files {
        let name = relative(&root, file);
        let text = std::fs::read_to_string(file).unwrap();
        let body = non_test_text(&text);
        if !body.contains("envelope_for_persisted") {
            continue;
        }
        for (line, function) in functions(body) {
            // The unfloored builder is private to stream_helpers.rs and is
            // called only by the floored one there.
            let unfloored = function.match_indices("envelope_for_persisted(").count();
            let floored = function
                .match_indices("envelope_for_persisted_floored(")
                .count();
            if name.ends_with("routes/invokers/stream_helpers.rs") {
                continue;
            }
            if unfloored > 0 {
                problems.push(format!(
                    "{name}:{line}: calls the unfloored envelope_for_persisted"
                ));
            }
            if floored > 0 {
                floored_callers.insert(name.clone());
                // Every call passes the floor the stream built, never a
                // literal absence of one.
                if function.contains("envelope_for_persisted_floored(&ev, None)")
                    || function.contains("envelope_for_persisted_floored(ev, None)")
                {
                    problems.push(format!("{name}:{line}: frames a record with no floor"));
                }
                if !function.contains("floor") || !function.contains("floor.as_deref()") {
                    problems.push(format!(
                        "{name}:{line}: frames a persisted event without the stream's floor"
                    ));
                }
            }
        }
    }
    let callers: Vec<_> = floored_callers.iter().cloned().collect();
    assert_eq!(
        callers,
        vec![
            "crates/daemon/ryeos-api/src/routes/invokers/chain_tail_invocation.rs".to_owned(),
            "crates/daemon/ryeos-api/src/routes/invokers/gateway_stream_invocation.rs".to_owned(),
            "crates/daemon/ryeos-api/src/routes/invokers/subscription_stream_invocation.rs"
                .to_owned(),
        ],
        "the files that frame persisted events changed: add the new one to this list once it applies the floor"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
