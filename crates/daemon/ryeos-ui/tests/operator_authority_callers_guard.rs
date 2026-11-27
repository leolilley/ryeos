// Every non-test caller of the functions that decide whether a key is the
// node's operator. Operator class unlocks data, acts and durable operations
// at each of them, and a `remote_principal` is an operator only if its key
// speaks for the operator's owner, so a new caller is a new place where that
// rule matters. This guard counts the callers; a new one fails it until the
// count is changed here together with its row in
// `.tmp/admitted-operator-owner-impact-20261126.md` (what operator class
// unlocks there and the test with a non-operator remote principal).

use std::path::{Path, PathBuf};

/// (function, expected count of `name(` occurrences in non-test source outside
/// operator_authority.rs, up to the file's top-level test module: an item-level
/// `#[cfg(test)]` earlier in a file is not the end of its production code). The seventh is a substring of the third, as the
/// scan counts text.
const EXPECTED: &[(&str, usize)] = &[
    ("require_admitted_operator(", 82),
    ("disclosure_class_for(", 10),
    ("admitted_operator_authority_digest(", 11),
    ("admitted_operator_authority_for_principal(", 10),
    ("admitted_binding_owner_authority_for_principal(", 1),
    ("retained_admitted_operator_authority(", 5),
    ("retained_admitted_operator_authority_digest(", 1),
];

fn sources(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if matches!(name.as_str(), "tests" | "target" | "benches" | "examples") {
                continue;
            }
            sources(root, &path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !name.ends_with("tests.rs")
            && !name.contains("test_support")
        {
            out.push(path.strip_prefix(root).unwrap().to_owned());
        }
    }
}

#[test]
fn the_operator_decision_functions_have_exactly_the_callers_the_impact_table_lists() {
    let root = ryeos_engine::test_support::workspace_root();
    let mut files = Vec::new();
    sources(&root, &root.join("crates"), &mut files);
    assert!(files.len() > 500, "the sweep found {} files", files.len());
    let mut counts = vec![0_usize; EXPECTED.len()];
    // A file with no top-level test module is production in full. An
    // item-level `#[cfg(test)]` is legal anywhere, so only the module ends
    // production text; the floor fails the guard if the marker's spelling
    // stops matching and the cut silently stops applying.
    let mut with_test_module = 0_usize;
    for file in &files {
        if file.to_string_lossy() == "crates/daemon/ryeos-app/src/operator_authority.rs" {
            continue;
        }
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let end = text.find("\n#[cfg(test)]\nmod ");
        with_test_module += usize::from(end.is_some());
        let body = end.map_or(text.as_str(), |end| &text[..end]);
        for (index, (name, _)) in EXPECTED.iter().enumerate() {
            counts[index] += body.matches(name).count();
        }
    }
    assert!(
        with_test_module > 800,
        "only {with_test_module} files have a top-level test module: the cut marker no longer matches"
    );
    let mismatches: Vec<String> = EXPECTED
        .iter()
        .zip(&counts)
        .filter(|((_, expected), found)| expected != *found)
        .map(|((name, expected), found)| {
            format!("{name}: expected {expected} callers, found {found}")
        })
        .collect();
    assert!(
        mismatches.is_empty(),
        "the set of operator-decision callers changed; add the new caller to the impact table \
         (class and test) and update EXPECTED:\n{}",
        mismatches.join("\n")
    );
}
