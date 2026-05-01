//! Runtime catalog built by SCANNING signed `kind: runtime` YAMLs from
//! bundle roots. No Rust descriptor table — runtimes are external
//! binaries with zero Rust function pointers.
//!
//! A `kind: runtime` YAML declares which item kind the runtime
//! interprets (`serves`), the binary reference, the ABI version it
//! implements, and optionally a `default: true` marker used to
//! disambiguate when more than one runtime serves the same kind.
//!
//! At engine init we walk `<bundle_root>/.ai/runtimes/*.yaml` for each
//! root, verify each file via the trust store (same envelope as kind
//! schemas: hash-prefix, no shebang), then group by `serves`.
//!
//! The registry deliberately stops at "verified + grouped". Binary
//! resolution against the CAS lives in dispatch and is wired in a
//! later task.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::canonical_ref::CanonicalRef;
use crate::error::EngineError;
use crate::kind_registry::{KindRegistry, TerminatorDecl};
use crate::resolution::TrustClass;
use crate::trust::TrustStore;

/// ABI version this daemon understands for runtime binaries.
/// Bundles shipping a different `abi_version` in their runtime YAML
/// are rejected at registry load — we fail closed at load, not at
/// dispatch.
///
/// Bump when the LaunchEnvelope, callback ABI, or any other
/// daemon↔runtime contract surface changes incompatibly.
pub const SUPPORTED_RUNTIME_ABI_VERSION: &str = "v1";

// ── Public types ─────────────────────────────────────────────────────

/// Typed view over a parsed `kind: runtime` YAML.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeYaml {
    /// Always the literal string `"runtime"`. Mismatch is a hard error.
    pub kind: String,
    /// Item kind this runtime interprets, e.g. `"directive"`.
    pub serves: String,
    /// `Some(true)` marks the default among multiple runtimes serving
    /// the same kind. `None` is implicit (= not the default).
    #[serde(default)]
    pub default: Option<bool>,
    /// Binary reference. May contain `{host_triple}` placeholder.
    pub binary_ref: String,
    /// ABI contract version, e.g. `"v1"`.
    pub abi_version: String,
    #[serde(default)]
    pub required_caps: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub schema: Option<RuntimeSchema>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSchema {
    pub envelope: String,
    pub result: String,
}

/// A runtime YAML that has been parsed AND trust-verified.
#[derive(Debug, Clone)]
pub struct VerifiedRuntime {
    pub canonical_ref: CanonicalRef,
    pub yaml: RuntimeYaml,
    pub trust_class: TrustClass,
    pub bundle_root: PathBuf,
}

/// Catalog of all `kind: runtime` items discovered at engine init.
#[derive(Debug, Clone, Default)]
pub struct RuntimeRegistry {
    by_kind: HashMap<String, Vec<VerifiedRuntime>>,
    by_ref: HashMap<CanonicalRef, VerifiedRuntime>,
}

impl RuntimeRegistry {
    /// Walk every `<bundle_root>/.ai/runtimes/*.yaml` for each given
    /// root. Parse + verify each via the trust store, group by `serves`.
    /// Multi-default conflict per kind = fail-closed Err.
    pub fn build_from_bundles(
        bundle_roots: &[(PathBuf, TrustClass)],
        trust: &TrustStore,
        kinds: &KindRegistry,
    ) -> Result<Self, EngineError> {
        let mut by_kind: HashMap<String, Vec<VerifiedRuntime>> = HashMap::new();
        let mut by_ref: HashMap<CanonicalRef, VerifiedRuntime> = HashMap::new();

        for (bundle_root, root_trust) in bundle_roots {
            let runtimes_dir = bundle_root.join(crate::AI_DIR).join("runtimes");
            if !runtimes_dir.is_dir() {
                continue;
            }

            let entries = std::fs::read_dir(&runtimes_dir).map_err(|e| {
                EngineError::Internal(format!(
                    "cannot read runtimes dir {}: {e}",
                    runtimes_dir.display()
                ))
            })?;

            let mut paths: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| {
                    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
                    matches!(ext, "yaml" | "yml")
                })
                .collect();
            paths.sort();

            for path in &paths {
                let verified = load_and_verify_runtime_yaml(path, bundle_root, *root_trust, trust)?;
                if by_ref.contains_key(&verified.canonical_ref) {
                    return Err(EngineError::DuplicateRuntimeRef {
                        canonical_ref: verified.canonical_ref.to_string(),
                    });
                }
                by_kind
                    .entry(verified.yaml.serves.clone())
                    .or_default()
                    .push(verified.clone());
                by_ref.insert(verified.canonical_ref.clone(), verified);
            }
        }

        // Fail-closed: any kind with >1 runtimes marked `default: true`
        // is ambiguous and must be rejected at build time.
        for (kind, list) in &by_kind {
            let defaults: Vec<String> = list
                .iter()
                .filter(|r| r.yaml.default == Some(true))
                .map(|r| r.canonical_ref.to_string())
                .collect();
            if defaults.len() > 1 {
                return Err(EngineError::MultipleRuntimeDefaults {
                    kind: kind.clone(),
                    defaults,
                });
            }
        }

        // Typed-view validation: when a runtime's `serves` kind declares
        // an explicit terminator (not a delegate), that terminator MUST be
        // Subprocess with protocol `protocol:rye/core/runtime_v1`.
        // Kinds that use `delegate: via: runtime_registry` (no terminator)
        // are inherently compatible — they explicitly delegate to the
        // runtime registry. Kinds with an incompatible terminator would
        // produce confusing dispatch errors downstream.
        const EXPECTED_PROTOCOL: &str = "protocol:rye/core/runtime_v1";
        for (kind, list) in &by_kind {
            let schema = match kinds.get(kind) {
                Some(s) => s,
                None => {
                    return Err(EngineError::RuntimeServesUnknownKind {
                        kind: kind.clone(),
                        runtime: list[0].canonical_ref.to_string(),
                    });
                }
            };
            let exec = match schema.execution() {
                Some(e) => e,
                None => {
                    return Err(EngineError::RuntimeServesKindNoExecution {
                        kind: kind.clone(),
                        runtime: list[0].canonical_ref.to_string(),
                    });
                }
            };
            // If the kind delegates to the runtime registry (no terminator),
            // it's inherently compatible with any runtime.
            let terminator = match exec.terminator.as_ref() {
                Some(t) => t,
                None => continue, // delegate-based or alias-based — valid
            };
            let found_protocol = match terminator {
                TerminatorDecl::Subprocess { protocol_ref } => protocol_ref.clone(),
                TerminatorDecl::InProcess { .. } => String::new(),
            };
            if found_protocol != EXPECTED_PROTOCOL {
                for _rt in list {
                    return Err(EngineError::RuntimeProtocolMismatch {
                        runtime: list[0].canonical_ref.to_string(),
                        kind: kind.clone(),
                        expected: EXPECTED_PROTOCOL.to_string(),
                        found: found_protocol.clone(),
                    });
                }
            }
        }

        Ok(Self { by_kind, by_ref })
    }

    /// Resolve runtime serving the given kind:
    /// - 1 runtime → return it (default field ignored).
    /// - >1 runtimes, exactly one with `default: true` → return the default.
    /// - >1 runtimes, none default → Err RuntimeDefaultRequired.
    /// - 0 runtimes → Err NoRuntimeFor.
    pub fn lookup_for(&self, kind: &str) -> Result<&VerifiedRuntime, EngineError> {
        let list = self
            .by_kind
            .get(kind)
            .ok_or_else(|| EngineError::NoRuntimeFor {
                kind: kind.to_owned(),
            })?;

        match list.len() {
            0 => Err(EngineError::NoRuntimeFor {
                kind: kind.to_owned(),
            }),
            1 => Ok(&list[0]),
            _ => {
                let defaults: Vec<&VerifiedRuntime> = list
                    .iter()
                    .filter(|r| r.yaml.default == Some(true))
                    .collect();
                match defaults.len() {
                    1 => Ok(defaults[0]),
                    0 => Err(EngineError::RuntimeDefaultRequired {
                        kind: kind.to_owned(),
                        candidates: list
                            .iter()
                            .map(|r| r.canonical_ref.to_string())
                            .collect(),
                    }),
                    _ => Err(EngineError::MultipleRuntimeDefaults {
                        kind: kind.to_owned(),
                        defaults: defaults
                            .iter()
                            .map(|r| r.canonical_ref.to_string())
                            .collect(),
                    }),
                }
            }
        }
    }

    pub fn lookup_by_ref(&self, canonical: &CanonicalRef) -> Option<&VerifiedRuntime> {
        self.by_ref.get(canonical)
    }

    pub fn all(&self) -> impl Iterator<Item = &VerifiedRuntime> {
        self.by_ref.values()
    }
}

// ── Internals ────────────────────────────────────────────────────────

/// Verify the signature on a runtime YAML, then parse it. Mirrors the
/// kind-schema bootstrap loader: hash-prefix envelope, fails closed on
/// missing or invalid signature, and rejects content tampering.
fn load_and_verify_runtime_yaml(
    yaml_path: &Path,
    bundle_root: &Path,
    root_trust: TrustClass,
    trust: &TrustStore,
) -> Result<VerifiedRuntime, EngineError> {
    let content = std::fs::read_to_string(yaml_path).map_err(|e| {
        EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: format!("cannot read file: {e}"),
        }
    })?;

    let sig_header = lillux::signature::parse_signature_line(
        content.lines().next().unwrap_or(""),
        "#",
        None,
    )
    .ok_or_else(|| EngineError::RuntimeYamlInvalid {
        path: yaml_path.to_owned(),
        reason: "missing or malformed signature line".to_owned(),
    })?;

    let body = lillux::signature::strip_signature_lines(&content);
    let actual_hash = lillux::signature::content_hash(&body);
    if actual_hash != sig_header.content_hash {
        return Err(EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: format!(
                "content hash mismatch: signed {} but file hashes to {}",
                sig_header.content_hash, actual_hash
            ),
        });
    }

    let signer = trust.get(&sig_header.signer_fingerprint).ok_or_else(|| {
        EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: format!(
                "untrusted signer fingerprint {}",
                sig_header.signer_fingerprint
            ),
        }
    })?;

    if !lillux::signature::verify_signature(
        &sig_header.content_hash,
        &sig_header.signature_b64,
        &signer.verifying_key,
    ) {
        return Err(EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: "Ed25519 signature verification failed".to_owned(),
        });
    }

    let yaml: RuntimeYaml = parse_runtime_yaml(yaml_path, &body)?;
    validate_runtime_yaml(yaml_path, &yaml)?;

    let bare_id = yaml_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: "cannot derive bare_id from filename".to_owned(),
        })?;

    let canonical = CanonicalRef::parse(&format!("runtime:{bare_id}")).map_err(|e| {
        EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: format!("cannot form canonical ref: {e}"),
        }
    })?;

    Ok(VerifiedRuntime {
        canonical_ref: canonical,
        yaml,
        trust_class: root_trust,
        bundle_root: bundle_root.to_owned(),
    })
}

/// Parse a runtime YAML body into the typed view.
///
/// Pub(crate) so the integration tests can exercise the parser
/// directly without standing up a trust store / bundle directory.
pub(crate) fn parse_runtime_yaml(
    yaml_path: &Path,
    body: &str,
) -> Result<RuntimeYaml, EngineError> {
    serde_yaml::from_str::<RuntimeYaml>(body).map_err(|e| EngineError::RuntimeYamlInvalid {
        path: yaml_path.to_owned(),
        reason: format!("YAML parse error: {e}"),
    })
}

pub(crate) fn validate_runtime_yaml(
    yaml_path: &Path,
    yaml: &RuntimeYaml,
) -> Result<(), EngineError> {
    if yaml.kind != "runtime" {
        return Err(EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: format!("expected `kind: runtime`, got `kind: {}`", yaml.kind),
        });
    }
    if yaml.serves.is_empty() {
        return Err(EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: "`serves` must be non-empty".to_owned(),
        });
    }
    if yaml.binary_ref.is_empty() {
        return Err(EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: "`binary_ref` must be non-empty".to_owned(),
        });
    }
    if yaml.abi_version.is_empty() {
        return Err(EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: "`abi_version` must be non-empty".to_owned(),
        });
    }
    if yaml.abi_version != SUPPORTED_RUNTIME_ABI_VERSION {
        return Err(EngineError::AbiVersionMismatch {
            runtime: yaml_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("(unknown)")
                .to_owned(),
            expected: SUPPORTED_RUNTIME_ABI_VERSION.to_owned(),
            found: yaml.abi_version.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn minimal_yaml() -> RuntimeYaml {
        RuntimeYaml {
            kind: "runtime".to_owned(),
            serves: "test_kind".to_owned(),
            default: None,
            binary_ref: "bin/x86_64-unknown-linux-gnu/test-runtime".to_owned(),
            abi_version: SUPPORTED_RUNTIME_ABI_VERSION.to_owned(),
            required_caps: vec![],
            description: None,
            schema: None,
        }
    }

    fn test_path() -> PathBuf {
        PathBuf::from("/tmp/test-runtime.yaml")
    }

    #[test]
    fn accepts_runtime_with_supported_abi_version() {
        let yaml = minimal_yaml();
        assert!(
            validate_runtime_yaml(&test_path(), &yaml).is_ok(),
            "v1 abi_version should be accepted"
        );
    }

    #[test]
    fn refuses_runtime_with_unsupported_abi_version() {
        let mut yaml = minimal_yaml();
        yaml.abi_version = "v999".to_owned();
        let result = validate_runtime_yaml(&test_path(), &yaml);
        let err = result.expect_err("expected AbiVersionMismatch");
        match err {
            EngineError::AbiVersionMismatch {
                expected, found, ..
            } => {
                assert_eq!(expected, "v1");
                assert_eq!(found, "v999");
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }
}
