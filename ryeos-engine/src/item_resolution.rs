//! Item resolution — system-first space search with clash diagnostics.
//!
//! Resolution order: system(node) → system(bundles) → user → project.
//! System is authoritative — the platform defines the baseline.
//! Clash warnings emitted when items exist in multiple spaces.
//!
//! All directory names and extension lists come from `KindSchema`.
//! This module never hardcodes kind strings, directories, or extensions.

use std::path::PathBuf;

use crate::canonical_ref::CanonicalRef;
use crate::contracts::{ItemSpace, ShadowedCandidate, SignatureEnvelope, SignatureHeader};
use crate::error::EngineError;
use crate::kind_registry::KindSchema;

/// A single labeled search root.
#[derive(Debug, Clone)]
pub struct ResolutionRoot {
    pub space: ItemSpace,
    /// Human-readable label, e.g. "system(node)", "system(bundle:standard)", "user", "project"
    pub label: String,
    /// Path to the `.ai/` directory
    pub ai_root: PathBuf,
}

/// Ordered list of search roots for item resolution.
///
/// Constructed in system-first order: node, bundles, user, project.
#[derive(Debug, Clone)]
pub struct ResolutionRoots {
    /// Search roots in resolution priority order (first match wins)
    pub ordered: Vec<ResolutionRoot>,
}

impl ResolutionRoots {
    /// Legacy convenience: build from flat fields.
    /// System roots are ordered first, then user, then project.
    pub fn from_flat(
        project: Option<PathBuf>,
        user: Option<PathBuf>,
        system: Vec<PathBuf>,
    ) -> Self {
        let mut ordered = Vec::new();

        for (i, sys_root) in system.iter().enumerate() {
            let label = if i == 0 {
                "system(node)".to_owned()
            } else {
                format!("system(bundle:{i})")
            };
            ordered.push(ResolutionRoot {
                space: ItemSpace::System,
                label,
                ai_root: sys_root.clone(),
            });
        }

        if let Some(user_root) = user {
            ordered.push(ResolutionRoot {
                space: ItemSpace::User,
                label: "user".to_owned(),
                ai_root: user_root,
            });
        }

        if let Some(project_root) = project {
            ordered.push(ResolutionRoot {
                space: ItemSpace::Project,
                label: "project".to_owned(),
                ai_root: project_root,
            });
        }

        Self { ordered }
    }
}

/// Full result of item resolution, including clash diagnostics.
#[derive(Debug, Clone)]
pub struct ResolutionResult {
    pub winner_path: PathBuf,
    pub winner_space: ItemSpace,
    pub winner_label: String,
    pub matched_ext: String,
    /// `.ai/` root directory under which the winner was found.
    /// Needed by the path-anchoring validator so it can compute the
    /// expected `<ai_root>/<kind.directory>` base for `match: path`
    /// rules without re-deriving it by walking parent components.
    pub winner_ai_root: PathBuf,
    pub shadowed: Vec<ShadowedCandidate>,
}

/// Resolve a canonical ref to a concrete file path, space, and clash info.
///
/// Searches roots in order (system-first). Returns the first match plus
/// all lower-priority matches (shadowed candidates).
#[tracing::instrument(
    level = "debug",
    name = "engine:resolve_ref",
    skip(roots, kind_schema),
    fields(ref = %ref_)
)]
pub fn resolve_item_full(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    ref_: &CanonicalRef,
) -> Result<ResolutionResult, EngineError> {
    let mut winner: Option<(PathBuf, ItemSpace, String, String, PathBuf)> = None;
    let mut shadowed = Vec::new();
    let mut searched_spaces = Vec::new();

    for root in &roots.ordered {
        let space_label = root.space.as_str().to_owned();
        if !searched_spaces.contains(&space_label) {
            searched_spaces.push(space_label);
        }

        let kind_dir = root.ai_root.join(&kind_schema.directory);
        for ext_spec in &kind_schema.extensions {
            let path = kind_dir.join(format!("{}{}", ref_.bare_id, ext_spec.ext));
            tracing::trace!(candidate = %path.display(), label = %root.label, "checking candidate path");
            if path.is_file() {
                if winner.is_none() {
                    winner = Some((
                        path,
                        root.space,
                        root.label.clone(),
                        ext_spec.ext.clone(),
                        root.ai_root.clone(),
                    ));
                } else {
                    shadowed.push(ShadowedCandidate {
                        label: root.label.clone(),
                        space: root.space,
                        path,
                    });
                }
                break; // Only match one extension per root (first ext wins)
            }
        }
    }

    match winner {
        Some((path, space, label, ext, ai_root)) => {
            if !shadowed.is_empty() {
                tracing::debug!(
                    item_ref = %ref_,
                    resolved_from = %label,
                    shadowed_count = shadowed.len(),
                    "item exists in multiple spaces"
                );
            }
            Ok(ResolutionResult {
                winner_path: path,
                winner_space: space,
                winner_label: label,
                matched_ext: ext,
                winner_ai_root: ai_root,
                shadowed,
            })
        }
        None => Err(EngineError::ItemNotFound {
            canonical_ref: ref_.to_string(),
            searched_spaces,
        }),
    }
}

/// Backward-compatible resolve: returns just the winner without clash info.
pub fn resolve_item(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    ref_: &CanonicalRef,
) -> Result<(PathBuf, ItemSpace, String), EngineError> {
    let result = resolve_item_full(roots, kind_schema, ref_)?;
    Ok((result.winner_path, result.winner_space, result.matched_ext))
}

/// Enumerate every canonical ref of `kind_schema` reachable from `roots`,
/// honouring resolution priority and the kind schema's own
/// `directory` + `extensions` declaration.
///
/// Walks `<root.ai_root>/<kind_schema.directory>/` recursively for each
/// root in `roots.ordered`. Files whose extension matches one of
/// `kind_schema.extensions[].ext` produce a `CanonicalRef { kind,
/// bare_id }` where `bare_id` is the path relative to the kind
/// directory with the matched extension stripped (slashes preserved
/// for nested layouts: `<dir>/rye/core/read.py` → `rye/core/read`).
///
/// Precedence semantics mirror `resolve_item_full`: the first root in
/// `roots.ordered` to surface a given `bare_id` wins, later occurrences
/// in lower-priority roots are silently dropped. Symlink loops, hidden
/// directories (starting with `.`), and IO errors on individual entries
/// are skipped — the loud failure modes are reserved for **resolving**
/// (where the caller asked for a specific ref); enumeration is a
/// best-effort discovery primitive.
///
/// Returns refs in deterministic order — sorted by `bare_id` after
/// precedence-collapse — so callers can rely on stable output for
/// caching / fingerprinting.
///
/// NB: this is intentionally **schema-driven**. There is no hardcoded
/// extension list, no hardcoded subdirectory; adding a new format =
/// adding an entry to the kind schema's `formats` block. Runtime /
/// daemon code consuming this primitive must not duplicate either.
pub fn enumerate_kind_refs(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    kind: &str,
) -> Vec<CanonicalRef> {
    use std::collections::HashSet;

    let extensions: HashSet<&str> = kind_schema
        .extensions
        .iter()
        .map(|e| e.ext.trim_start_matches('.'))
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut refs: Vec<CanonicalRef> = Vec::new();

    for root in &roots.ordered {
        let kind_dir = root.ai_root.join(&kind_schema.directory);
        if !kind_dir.is_dir() {
            continue;
        }
        walk_kind_dir(&kind_dir, &kind_dir, &extensions, &mut |bare_id| {
            if seen.insert(bare_id.clone()) {
                refs.push(CanonicalRef {
                    kind: kind.to_owned(),
                    bare_id,
                    suffix: None,
                });
            }
        });
    }

    refs.sort_by(|a, b| a.bare_id.cmp(&b.bare_id));
    refs
}

/// Recursively walk `dir`, invoking `emit(bare_id)` for each file whose
/// extension is in `extensions`. `bare_id` is computed relative to
/// `kind_root` with the matched extension stripped and platform path
/// separators normalised to `/`.
///
/// Hidden entries (basename starting with `.`) are skipped — they're
/// not legitimate item paths and tend to be VCS / editor noise.
fn walk_kind_dir(
    kind_root: &std::path::Path,
    dir: &std::path::Path,
    extensions: &std::collections::HashSet<&str>,
    emit: &mut dyn FnMut(String),
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let basename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if basename.starts_with('.') {
            continue;
        }
        let ftype = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ftype.is_dir() {
            walk_kind_dir(kind_root, &path, extensions, emit);
            continue;
        }
        if !ftype.is_file() {
            continue;
        }
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        if !extensions.contains(ext) {
            continue;
        }
        let rel = match path.strip_prefix(kind_root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut bare = String::with_capacity(rel.as_os_str().len());
        for (i, comp) in rel.with_extension("").components().enumerate() {
            if i > 0 {
                bare.push('/');
            }
            bare.push_str(&comp.as_os_str().to_string_lossy());
        }
        if bare.is_empty() {
            continue;
        }
        emit(bare);
    }
}

/// Parse a `rye:signed:<timestamp>:<content_hash>:<sig_b64>:<signer_fp>` header
/// from file content, using the envelope to locate the signature line.
pub fn parse_signature_header(
    content: &str,
    envelope: &SignatureEnvelope,
) -> Option<SignatureHeader> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // Determine which lines to inspect
    let candidates: Vec<usize> = if envelope.after_shebang {
        // Check line 2 first (after shebang), then line 1
        let mut c = Vec::new();
        if lines.len() > 1 {
            c.push(1);
        }
        c.push(0);
        c
    } else {
        vec![0]
    };

    for idx in candidates {
        let line = lines[idx];
        if let Some(header) = try_parse_signature_line(line, envelope) {
            return Some(header);
        }
    }

    None
}

fn try_parse_signature_line(line: &str, envelope: &SignatureEnvelope) -> Option<SignatureHeader> {
    let header = lillux::signature::parse_signature_line(
        line,
        &envelope.prefix,
        envelope.suffix.as_deref(),
    )?;
    Some(SignatureHeader {
        timestamp: header.timestamp,
        content_hash: header.content_hash,
        signature_b64: header.signature_b64,
        signer_fingerprint: header.signer_fingerprint,
    })
}

/// Compute a SHA-256 hex digest of the given content.
pub fn content_hash(content: &str) -> String {
    lillux::signature::content_hash(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind_registry::ExtensionSpec;
    use std::fs;
    use std::path::Path;
    use ryeos_tracing::test as trace_test;

    fn make_kind_schema(directory: &str, extensions: Vec<(&str, &str)>) -> KindSchema {
        KindSchema {
            directory: directory.to_owned(),
            extraction_rules: std::collections::HashMap::new(),
            execution: Some(crate::kind_registry::ExecutionSchema {
                aliases: std::collections::HashMap::new(),
                alias_max_depth: 8,
                resolution: Vec::new(),
                terminator: None,
                delegate: None,
                thread_profile: None,
                default_operation: None,
                operations: Vec::new(),
                launch_augmentations: Vec::new(),
            }),
            extensions: extensions
                .into_iter()
                .map(|(ext, parser)| ExtensionSpec {
                    ext: ext.to_owned(),
                    parser: parser.to_owned(),
                    signature: SignatureEnvelope {
                        prefix: "#".to_owned(),
                        suffix: None,
                        after_shebang: false,
                    },
                })
                .collect(),
            composed_value_contract: crate::contracts::ValueShape::any_mapping(),
            composer: "handler:rye/core/identity".to_owned(),
            composer_config: serde_json::Value::Null,
            runtime: None,
            inventory_kinds: Vec::new(),
            inventory_schema_keys: Vec::new(),
        }
    }

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir = std::env::temp_dir().join(format!(
            "rye_resolution_test_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_item(root: &Path, kind_dir: &str, bare_id: &str, ext: &str, content: &str) {
        let dir = root.join(kind_dir);
        // Handle nested bare_ids like "rye/bash/bash"
        let file_path = dir.join(format!("{bare_id}{ext}"));
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&file_path, content).unwrap();
    }

    #[test]
    fn resolve_finds_project_space_when_only_source() {
        let project_root = tempdir();
        let system_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/ast")]);

        write_item(&project_root, "tools", "my_tool", ".py", "# project");
        write_item(&system_root, "tools", "my_tool", ".py", "# system");

        // When only project has it (system root empty), project wins
        let roots = ResolutionRoots::from_flat(
            Some(project_root.clone()),
            None,
            vec![system_root],
        );
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let (_path, space, ext) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(space, ItemSpace::System); // system root is searched first
        assert_eq!(ext, ".py");
    }

    #[test]
    fn resolve_system_wins_over_project() {
        let project_root = tempdir();
        let system_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/ast")]);

        write_item(&system_root, "tools", "my_tool", ".py", "# system");
        write_item(&project_root, "tools", "my_tool", ".py", "# project");

        let roots = ResolutionRoots::from_flat(
            Some(project_root),
            None,
            vec![system_root.clone()],
        );
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let (path, space, _) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(space, ItemSpace::System);
        assert!(path.starts_with(&system_root));
    }

    #[test]
    fn resolve_finds_user_space() {
        let user_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/ast")]);

        write_item(&user_root, "tools", "my_tool", ".py", "# user");

        let roots = ResolutionRoots::from_flat(
            None,
            Some(user_root.clone()),
            vec![],
        );
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let (path, space, _) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(space, ItemSpace::User);
        assert!(path.starts_with(&user_root));
    }

    #[test]
    fn resolve_finds_system_space() {
        let system_root = tempdir();
        let schema = make_kind_schema("directives", vec![(".md", "markdown/xml")]);

        write_item(&system_root, "directives", "init", ".md", "# system");

        let roots = ResolutionRoots::from_flat(
            None,
            None,
            vec![system_root.clone()],
        );
        let ref_ = CanonicalRef::parse("directive:init").unwrap();

        let (path, space, _) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(space, ItemSpace::System);
        assert!(path.starts_with(&system_root));
    }

    #[test]
    fn resolve_extension_priority() {
        let project_root = tempdir();
        // .py is listed first, so it should win even though .yaml also exists
        let schema = make_kind_schema("tools", vec![(".py", "python/ast"), (".yaml", "yaml/yaml")]);

        write_item(&project_root, "tools", "my_tool", ".py", "# python");
        write_item(&project_root, "tools", "my_tool", ".yaml", "name: yaml");

        let roots = ResolutionRoots::from_flat(
            Some(project_root),
            None,
            vec![],
        );
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let (path, _, ext) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(ext, ".py");
        assert!(path.to_string_lossy().ends_with(".py"));
    }

    #[test]
    fn resolve_not_found() {
        let project_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/ast")]);

        let roots = ResolutionRoots::from_flat(
            Some(project_root),
            None,
            vec![],
        );
        let ref_ = CanonicalRef::parse("tool:nonexistent").unwrap();

        let err = resolve_item(&roots, &schema, &ref_).unwrap_err();
        match err {
            EngineError::ItemNotFound {
                canonical_ref,
                searched_spaces,
            } => {
                assert_eq!(canonical_ref, "tool:nonexistent");
                assert!(searched_spaces.contains(&"project".to_owned()));
            }
            other => panic!("expected ItemNotFound, got: {other:?}"),
        }
    }

    #[test]
    fn resolve_clash_diagnostics() {
        let project_root = tempdir();
        let user_root = tempdir();
        let system_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/ast")]);

        write_item(&system_root, "tools", "my_tool", ".py", "# system");
        write_item(&user_root, "tools", "my_tool", ".py", "# user");
        write_item(&project_root, "tools", "my_tool", ".py", "# project");

        let roots = ResolutionRoots::from_flat(
            Some(project_root),
            Some(user_root),
            vec![system_root],
        );
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let result = resolve_item_full(&roots, &schema, &ref_).unwrap();
        assert_eq!(result.winner_space, ItemSpace::System);
        assert_eq!(result.winner_label, "system(node)");
        assert_eq!(result.shadowed.len(), 2);
        assert_eq!(result.shadowed[0].space, ItemSpace::User);
        assert_eq!(result.shadowed[1].space, ItemSpace::Project);
    }

    #[test]
    fn parse_signature_header_hash_prefix() {
        let content =
            "# rye:signed:2026-04-10T00:00:00Z:abc123:sigB64data:fp_signer\nprint('hello')";
        let envelope = SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: false,
        };

        let header = parse_signature_header(content, &envelope).unwrap();
        assert_eq!(header.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(header.content_hash, "abc123");
        assert_eq!(header.signature_b64, "sigB64data");
        assert_eq!(header.signer_fingerprint, "fp_signer");
    }

    #[test]
    fn parse_signature_header_slash_prefix() {
        let content =
            "// rye:signed:2026-04-10T00:00:00Z:abc123:sigB64data:fp_signer\nconsole.log('hi')";
        let envelope = SignatureEnvelope {
            prefix: "//".to_owned(),
            suffix: None,
            after_shebang: false,
        };

        let header = parse_signature_header(content, &envelope).unwrap();
        assert_eq!(header.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(header.content_hash, "abc123");
        assert_eq!(header.signature_b64, "sigB64data");
        assert_eq!(header.signer_fingerprint, "fp_signer");
    }

    #[test]
    fn parse_signature_header_html_prefix() {
        let content =
            "<!-- rye:signed:2026-04-10T00:00:00Z:abc123:sigB64data:fp_signer -->\n# Hello";
        let envelope = SignatureEnvelope {
            prefix: "<!--".to_owned(),
            suffix: Some("-->".to_owned()),
            after_shebang: false,
        };

        let header = parse_signature_header(content, &envelope).unwrap();
        assert_eq!(header.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(header.content_hash, "abc123");
        assert_eq!(header.signature_b64, "sigB64data");
        assert_eq!(header.signer_fingerprint, "fp_signer");
    }

    #[test]
    fn parse_signature_header_after_shebang() {
        let content =
            "#!/usr/bin/env python3\n# rye:signed:2026-04-10T00:00:00Z:abc123:sigB64data:fp_signer\nprint('hello')";
        let envelope = SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: true,
        };

        let header = parse_signature_header(content, &envelope).unwrap();
        assert_eq!(header.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(header.content_hash, "abc123");
        assert_eq!(header.signature_b64, "sigB64data");
        assert_eq!(header.signer_fingerprint, "fp_signer");
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn resolve_item_full_emits_span() {
        let project_root = tempdir();
        let system_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/ast")]);
        write_item(&project_root, "tools", "trace_tool", ".py", "# content");

        let roots = ResolutionRoots::from_flat(
            Some(project_root.clone()),
            None,
            vec![system_root],
        );
        let ref_ = CanonicalRef::parse("tool:trace_tool").unwrap();

        let (_, spans) = trace_test::capture_traces(|| {
            let _ = resolve_item_full(&roots, &schema, &ref_);
        });

        let span = trace_test::find_span(&spans, "engine:resolve_ref");
        assert!(span.is_some(), "expected engine:resolve_ref span, got: {:?}", spans.iter().map(|s: &ryeos_tracing::test::RecordedSpan| &s.name).collect::<Vec<_>>());

        let span = span.unwrap();
        let field_val = |name: &str| -> Option<&str> {
            span.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        };
        assert_eq!(field_val("ref"), Some("tool:trace_tool"));
    }
}
