//! Item resolution — three-tier space search and signature header parsing.
//!
//! All directory names and extension lists come from `KindSchema`.
//! This module never hardcodes kind strings, directories, or extensions.

use std::path::PathBuf;

use crate::canonical_ref::CanonicalRef;
use crate::contracts::{ItemSpace, SignatureEnvelope, SignatureHeader};
use crate::error::EngineError;
use crate::kind_registry::KindSchema;

/// Search roots for the three-tier resolution order.
///
/// Constructed from `ProjectContext` + user space + system bundle roots.
#[derive(Debug, Clone)]
pub struct ResolutionRoots {
    /// Project `.ai/` root, if a project context is materialized
    pub project: Option<PathBuf>,
    /// User `~/.ai/` root
    pub user: Option<PathBuf>,
    /// System bundle `.ai/` roots (may be multiple bundles)
    pub system: Vec<PathBuf>,
}

/// Resolve a canonical ref to a concrete file path and space.
///
/// Searches project → user → system (in order). Within each space,
/// tries extensions in the priority order declared in the `KindSchema`.
/// Returns the first match.
pub fn resolve_item(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    ref_: &CanonicalRef,
) -> Result<(PathBuf, ItemSpace, String), EngineError> {
    let spaces: Vec<(&Option<PathBuf>, ItemSpace)> = vec![
        (&roots.project, ItemSpace::Project),
        (&roots.user, ItemSpace::User),
    ];

    let mut searched_spaces = Vec::new();

    // Project and user: single optional root each
    for (root_opt, space) in &spaces {
        if let Some(root) = root_opt {
            searched_spaces.push(space.as_str().to_owned());
            let kind_dir = root.join(&kind_schema.directory);
            for ext_spec in &kind_schema.extensions {
                let path = kind_dir.join(format!("{}{}", ref_.bare_id, ext_spec.ext));
                tracing::trace!(candidate = %path.display(), space = space.as_str(), "checking candidate path");
                if path.is_file() {
                    return Ok((path, *space, ext_spec.ext.clone()));
                }
            }
        }
    }

    // System: multiple bundle roots
    for system_root in &roots.system {
        let already_added = searched_spaces.iter().any(|s| s == "system");
        if !already_added {
            searched_spaces.push("system".to_owned());
        }
        let kind_dir = system_root.join(&kind_schema.directory);
        for ext_spec in &kind_schema.extensions {
            let path = kind_dir.join(format!("{}{}", ref_.bare_id, ext_spec.ext));
            tracing::trace!(candidate = %path.display(), space = "system", "checking candidate path");
            if path.is_file() {
                return Ok((path, ItemSpace::System, ext_spec.ext.clone()));
            }
        }
    }

    Err(EngineError::ItemNotFound {
        canonical_ref: ref_.to_string(),
        searched_spaces,
    })
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

    fn make_kind_schema(directory: &str, extensions: Vec<(&str, &str)>) -> KindSchema {
        KindSchema {
            directory: directory.to_owned(),
            default_executor_id: None,
            extraction_rules: std::collections::HashMap::new(),
            resolution: Vec::new(),
            extensions: extensions
                .into_iter()
                .map(|(ext, parser)| ExtensionSpec {
                    ext: ext.to_owned(),
                    parser_id: parser.to_owned(),
                    signature: SignatureEnvelope {
                        prefix: "#".to_owned(),
                        suffix: None,
                        after_shebang: false,
                    },
                })
                .collect(),
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

    fn write_item(root: &PathBuf, kind_dir: &str, bare_id: &str, ext: &str, content: &str) {
        let dir = root.join(kind_dir);
        // Handle nested bare_ids like "rye/bash/bash"
        let file_path = dir.join(format!("{bare_id}{ext}"));
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&file_path, content).unwrap();
    }

    #[test]
    fn resolve_finds_project_space_first() {
        let project_root = tempdir();
        let system_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/ast")]);

        write_item(&project_root, "tools", "my_tool", ".py", "# project");
        write_item(&system_root, "tools", "my_tool", ".py", "# system");

        let roots = ResolutionRoots {
            project: Some(project_root.clone()),
            user: None,
            system: vec![system_root],
        };
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let (path, space, ext) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(space, ItemSpace::Project);
        assert_eq!(ext, ".py");
        assert!(path.starts_with(&project_root));
    }

    #[test]
    fn resolve_finds_user_space_second() {
        let user_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/ast")]);

        write_item(&user_root, "tools", "my_tool", ".py", "# user");

        let roots = ResolutionRoots {
            project: None,
            user: Some(user_root.clone()),
            system: vec![],
        };
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

        let roots = ResolutionRoots {
            project: None,
            user: None,
            system: vec![system_root.clone()],
        };
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

        let roots = ResolutionRoots {
            project: Some(project_root),
            user: None,
            system: vec![],
        };
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let (path, _, ext) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(ext, ".py");
        assert!(path.to_string_lossy().ends_with(".py"));
    }

    #[test]
    fn resolve_not_found() {
        let project_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/ast")]);

        let roots = ResolutionRoots {
            project: Some(project_root),
            user: None,
            system: vec![],
        };
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
}
