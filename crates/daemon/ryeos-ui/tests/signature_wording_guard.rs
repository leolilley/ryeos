// A signature or endorsement shows only that a key signed these bytes at a
// recorded time. No shipped wording, view or Surface text may say more
// (rulings 1.73). The list is phrases, so true sentences are not blocked:
// "signature verifies" and "verification failed: signer_revoked" are fine.
//
// "reviewed by" and "approved by" are allowed only in a view field the view
// contract binds to a named signed review or approval record. No shipped
// view binds one yet, so they are forbidden everywhere until one does.

use std::path::{Path, PathBuf};

const FORBIDDEN_PHRASES: &[&str] = &[
    "verified person",
    "verified author",
    "verified identity",
    "verified user",
    "certified",
    "guaranteed",
    "trusted author",
    "trustworthy",
    "safe to run",
    "safe to install",
    "is safe",
    "real name",
    "legal name",
    "reviewed by",
    "approved by",
    "endorsed as",
];

const SCANNED_DIRS: &[&str] = &[
    "bundles/ryeos-ui/.ai/config",
    "bundles/ryeos-ui/.ai/views",
    "bundles/ryeos-ui/.ai/view_sets",
    "bundles/ryeos-ui/.ai/surfaces",
];

fn yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            yaml_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "yaml") {
            out.push(path);
        }
    }
}

fn offending_phrases(text: &str) -> Vec<&'static str> {
    let lower = text.to_lowercase();
    FORBIDDEN_PHRASES
        .iter()
        .copied()
        .filter(|phrase| lower.contains(phrase))
        .collect()
}

#[test]
fn the_phrase_list_catches_an_overclaim_and_passes_a_mechanical_sentence() {
    assert_eq!(
        offending_phrases("Verified author. Safe to install."),
        ["verified author", "safe to install"]
    );
    assert!(
        offending_phrases("The signature verifies. Verification failed: signer_revoked.")
            .is_empty()
    );
}

#[test]
fn no_shipped_ui_wording_view_or_surface_overclaims_a_signature() {
    let root = ryeos_engine::test_support::workspace_root();
    let mut files = Vec::new();
    for dir in SCANNED_DIRS {
        yaml_files(&root.join(dir), &mut files);
    }
    assert!(!files.is_empty(), "the scan found no files");
    for file in files {
        let hits = offending_phrases(&std::fs::read_to_string(&file).unwrap());
        assert!(hits.is_empty(), "{} says {hits:?}", file.display());
    }
}
