//! Regression: every shipped directive that declares a `context:` block
//! must use canonical refs (knowledge:...) not bare ids. Directives
//! without context blocks pass trivially.
//!
//! This test walks both the `core` and `standard` bundle directive
//! directories. If a future contributor adds a directive with a
//! context block using bare refs (e.g. `system: [arc/foundation]`
//! instead of `system: [knowledge:arc/foundation]`), this test fails.

use std::fs;
use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    manifest_dir()
        .parent()
        .expect("ryeosd has a parent dir")
        .to_path_buf()
}

/// Parse YAML frontmatter from a signed directive .md file.
/// Returns the YAML content between the two `---` delimiters, or None
/// if no frontmatter is found. Handles the `<!-- rye:signed:... -->`
/// envelope line.
fn extract_frontmatter(content: &str) -> Option<String> {
    // Strip signed envelope line if present.
    let content = content
        .lines()
        .skip_while(|line| line.starts_with("<!-- rye:signed:") || line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    let mut parts = content.splitn(3, "---");
    let before = parts.next()?;
    if !before.trim().is_empty() {
        // No leading delimiter — no frontmatter.
        return None;
    }
    let yaml = parts.next()?;
    let _body = parts.next()?;
    Some(yaml.to_string())
}

/// Extract the `context:` mapping from parsed YAML frontmatter.
/// Returns a map of position → list of ref strings, or None if no
/// context block exists.
fn extract_context_refs(yaml: &str) -> Option<Vec<(String, Vec<String>)>> {
    // Lightweight parse: find the `context:` key and extract its
    // values. This is intentionally not a full YAML parser — the
    // context block shape is well-known (flat string-seq mapping).
    let mut in_context = false;
    let mut result: Vec<(String, Vec<String>)> = Vec::new();
    let mut current_key = String::new();

    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }

        // Top-level key.
        if !line.starts_with(' ') && !line.starts_with('\t') {
            if trimmed == "context:" {
                in_context = true;
                continue;
            }
            if in_context {
                break; // context block ended
            }
            continue;
        }

        if !in_context {
            continue;
        }

        // Indented line within context block.
        if line.starts_with("  ") && trimmed.ends_with(':') {
            // Nested key like "  system:"
            current_key = trimmed.trim_end_matches(':').trim().to_string();
        } else if line.starts_with("  ") && trimmed.starts_with("- ") {
            // List item like "  - knowledge:arc/foundation"
            let value = trimmed.trim_start_matches("- ").trim().to_string();
            if let Some(last) = result.last_mut() {
                if last.0 == current_key {
                    last.1.push(value);
                    continue;
                }
            }
            result.push((current_key.clone(), vec![value]));
        } else if line.starts_with("    ") && trimmed.starts_with("- ") {
            // Deeper-indented list item
            let value = trimmed.trim_start_matches("- ").trim().to_string();
            if let Some(last) = result.last_mut() {
                last.1.push(value);
            }
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

fn check_directive_file(path: &std::path::Path) -> Vec<String> {
    let mut violations = Vec::new();
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return violations,
    };

    let frontmatter = match extract_frontmatter(&content) {
        Some(fm) => fm,
        None => return violations, // no frontmatter, passes trivially
    };

    let context_refs = match extract_context_refs(&frontmatter) {
        Some(refs) => refs,
        None => return violations, // no context block, passes trivially
    };

    for (position, refs) in &context_refs {
        for r in refs {
            if !r.starts_with("knowledge:") {
                violations.push(format!(
                    "{}: context.{} = {:?} (expected canonical ref starting with knowledge:)",
                    path.display(),
                    position,
                    r
                ));
            }
        }
    }

    violations
}

#[test]
fn bundle_directives_use_canonical_context_refs() {
    let workspace = workspace_root();
    let dirs = [
        workspace.join("ryeos-bundles/standard/.ai/directives"),
        workspace.join("ryeos-bundles/core/.ai/directives"),
    ];

    let mut all_violations = Vec::new();

    for dir in &dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut stack = vec![dir.clone()];
        while let Some(current) = stack.pop() {
            let entries = match fs::read_dir(&current) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().map_or(false, |ext| ext == "md") {
                    all_violations.extend(check_directive_file(&path));
                }
            }
        }
    }

    assert!(
        all_violations.is_empty(),
        "directives with non-canonical context refs found:\n{}",
        all_violations.join("\n")
    );
}
