use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::contracts::{ItemSpace, SignatureEnvelope, TrustClass};
use crate::error::EngineError;
use crate::item_resolution::{ResolutionRoots, parse_signature_header};
use crate::kind_registry::KindRegistry;
use crate::parsers::dispatcher::ParserDispatcher;
use crate::project_content::AuthoritativeProjectContent;
use crate::trust::{
    TrustStore, content_hash_after_signature, verify_item_signature,
    verify_item_signature_with_hash,
};

/// Maximum bytes accepted for one config source, independent of whether it is
/// observed live or read from an admitted content authority.
const MAX_CONFIG_SOURCE_BYTES: u64 = 1024 * 1024;
const MAX_BUNDLE_MANIFEST_BYTES: u64 = 256 * 1024;

/// One exact, node-trusted Config from an admitted project generation. It is
/// deliberately not a merged Config: source-bundle, node, and project overlay
/// precedence must not change the ownership authority used by a build Tool.
#[derive(Debug, Clone)]
pub struct StrictSignedProjectBundleConfig {
    pub value: Value,
    pub signer_fingerprint: String,
    pub manifest_body_digest: String,
}

/// Resolve one source-bundle Config through the retained project-content
/// authority, anchored to the installed bundle publisher. Both the source
/// manifest and Config must be signed by that same node-trusted publisher.
/// No path-backed read or trust-store overlay is permitted here.
pub(crate) fn load_strict_signed_project_bundle_config(
    project_root: &Path,
    project_content: &dyn AuthoritativeProjectContent,
    node_trust_store: &TrustStore,
    registered_bundle_root: &Path,
    bundle_name: &str,
    config_path: &str,
) -> Result<StrictSignedProjectBundleConfig, EngineError> {
    let invalid = |reason: String| EngineError::InvalidRuntimeConfig {
        path: config_path.to_owned(),
        reason,
    };
    if !project_root.is_absolute() {
        return Err(invalid(
            "strict project Config requires an absolute admitted project root".into(),
        ));
    }
    if bundle_name.is_empty()
        || bundle_name.len() > 128
        || !bundle_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || bundle_name.starts_with('-')
        || bundle_name.ends_with('-')
    {
        return Err(invalid(
            "strict project Config names an invalid source bundle".into(),
        ));
    }
    let prefix = format!("bundles/{bundle_name}/.ai/config/");
    let Some(config_suffix) = config_path.strip_prefix(&prefix) else {
        return Err(invalid(
            "strict project Config must belong to its exact source bundle".into(),
        ));
    };
    if config_suffix.is_empty()
        || !config_suffix.ends_with(".yaml")
        || config_path.contains('\\')
        || config_path
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        || config_path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(invalid(
            "strict project Config path is not canonical".into(),
        ));
    }

    let installed = crate::plan_builder::verify_bundle_source_manifest_identity(
        registered_bundle_root,
        bundle_name,
        node_trust_store,
    )?;
    let manifest_path = format!("bundles/{bundle_name}/.ai/manifest.yaml");
    let (manifest, manifest_signer, manifest_body_digest) = read_signed_project_yaml(
        project_content,
        Path::new(&manifest_path),
        MAX_BUNDLE_MANIFEST_BYTES,
        node_trust_store,
    )?;
    if manifest.get("name").and_then(Value::as_str) != Some(bundle_name) {
        return Err(invalid(
            "signed source-bundle manifest names another bundle".into(),
        ));
    }
    if manifest_signer != installed.signer_fingerprint {
        return Err(invalid(
            "source-bundle manifest signer differs from installed publisher".into(),
        ));
    }
    let (value, config_signer, _) = read_signed_project_yaml(
        project_content,
        Path::new(config_path),
        MAX_CONFIG_SOURCE_BYTES,
        node_trust_store,
    )?;
    if config_signer != manifest_signer {
        return Err(invalid(
            "project Config signer differs from source-bundle publisher".into(),
        ));
    }
    Ok(StrictSignedProjectBundleConfig {
        value,
        signer_fingerprint: config_signer,
        manifest_body_digest,
    })
}

fn read_signed_project_yaml(
    content: &dyn AuthoritativeProjectContent,
    relative_path: &Path,
    max_bytes: u64,
    trust: &TrustStore,
) -> Result<(Value, String, String), EngineError> {
    let label = relative_path.display().to_string();
    let invalid = |reason: String| EngineError::InvalidRuntimeConfig {
        path: label.clone(),
        reason,
    };
    let bytes = content
        .read_file(relative_path, max_bytes)?
        .ok_or_else(|| invalid("required signed project source is absent".into()))?;
    let raw = String::from_utf8(bytes)
        .map_err(|error| invalid(format!("signed project source is not UTF-8: {error}")))?;
    let (body, canonical_header) =
        lillux::signature::strip_canonical_signature_with_envelope(&raw, "#", None, false)
            .map_err(|error| invalid(format!("noncanonical signature envelope: {error}")))?;
    if canonical_header.is_none() {
        return Err(invalid(
            "project source requires a trusted signature".into(),
        ));
    }
    let envelope = SignatureEnvelope {
        prefix: "#".to_owned(),
        suffix: None,
        after_shebang: false,
    };
    let header = parse_signature_header(&raw, &envelope)
        .ok_or_else(|| invalid("project source has no valid signature envelope".into()))?;
    let (trust_class, _) = verify_item_signature(&raw, &header, &envelope, trust)?;
    if trust_class != TrustClass::Trusted {
        return Err(invalid("project source signer is not node-trusted".into()));
    }
    let value: Value = serde_yaml::from_str(&body)
        .map_err(|error| invalid(format!("decode signed project YAML: {error}")))?;
    if !value.is_object() {
        return Err(invalid("signed project YAML must be an object".into()));
    }
    Ok((
        value,
        header.signer_fingerprint,
        lillux::sha256_hex(body.as_bytes()),
    ))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSpec {
    pub path: String,
    #[serde(default)]
    pub mode: ResolveMode,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolveMode {
    #[default]
    DeepMerge,
    FirstMatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLayerSource {
    pub path: PathBuf,
    pub space: ItemSpace,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub value: Value,
    pub layers: Vec<ConfigLayerSource>,
}

pub struct ConfigLoadContext<'a> {
    pub roots: &'a ResolutionRoots,
    pub parsers: &'a ParserDispatcher,
    pub kinds: &'a KindRegistry,
    pub trust_store: &'a TrustStore,
    pub project_authority: Option<(
        &'a Path,
        &'a dyn crate::project_content::AuthoritativeProjectContent,
    )>,
}

pub fn resolve_config_spec(
    spec: &ConfigSpec,
    ctx: &ConfigLoadContext<'_>,
) -> Result<ResolvedConfig, EngineError> {
    match spec.mode {
        ResolveMode::DeepMerge => {
            let mut merged = Value::Object(Map::new());
            let mut layers = Vec::new();
            for root in ctx.roots.ordered.iter().rev() {
                let candidate = root.ai_root.join("config").join(&spec.path);
                if config_candidate_exists(&candidate, root.space, ctx)? {
                    tracing::info!(
                        config_path = %candidate.display(),
                        space = ?root.space,
                        mode = "deep_merge",
                        "config_resolve loaded config layer"
                    );
                    let layer = load_and_verify_config_file(&candidate, ctx)?;
                    merged = deep_merge(merged, layer);
                    layers.push(ConfigLayerSource {
                        path: candidate,
                        space: root.space,
                    });
                }
            }
            Ok(ResolvedConfig {
                value: merged,
                layers,
            })
        }
        ResolveMode::FirstMatch => {
            for target in &[ItemSpace::Project, ItemSpace::Bundle] {
                for root in ctx.roots.ordered.iter().filter(|r| r.space == *target) {
                    let candidate = root.ai_root.join("config").join(&spec.path);
                    if config_candidate_exists(&candidate, root.space, ctx)? {
                        tracing::info!(
                            config_path = %candidate.display(),
                            space = ?root.space,
                            mode = "first_match",
                            "config_resolve selected config file"
                        );
                        let value = load_and_verify_config_file(&candidate, ctx)?;
                        return Ok(ResolvedConfig {
                            value,
                            layers: vec![ConfigLayerSource {
                                path: candidate,
                                space: root.space,
                            }],
                        });
                    }
                }
            }
            Ok(ResolvedConfig {
                value: Value::Object(Map::new()),
                layers: Vec::new(),
            })
        }
    }
}

pub fn load_and_verify_config_file(
    path: &Path,
    ctx: &ConfigLoadContext<'_>,
) -> Result<Value, EngineError> {
    load_and_verify_config_file_with_hash(path, ctx).map(|(value, _)| value)
}

/// Load one config contributor only when its signature is valid under the
/// node trust store.
///
/// Ordinary authored config resolution deliberately supports unsigned layers
/// and classifies their trust later. Node-wide admission limits cannot use
/// that permissive contract: an existing untrusted layer must stop startup
/// rather than silently weakening or replacing the valve.
pub fn load_and_verify_trusted_config_file(
    path: &Path,
    ctx: &ConfigLoadContext<'_>,
) -> Result<Value, EngineError> {
    load_and_verify_config_file_with_policy(path, ctx, true).map(|(value, _)| value)
}

/// Load, verify, and parse one config from a single securely-opened source
/// observation, returning the whole-file digest of the exact parsed bytes.
pub fn load_and_verify_config_file_with_hash(
    path: &Path,
    ctx: &ConfigLoadContext<'_>,
) -> Result<(Value, String), EngineError> {
    load_and_verify_config_file_with_policy(path, ctx, false)
}

fn load_and_verify_config_file_with_policy(
    path: &Path,
    ctx: &ConfigLoadContext<'_>,
    require_trusted_signature: bool,
) -> Result<(Value, String), EngineError> {
    let content = match ctx.project_authority {
        Some((project_root, project_content)) if path.starts_with(project_root) => {
            let relative =
                path.strip_prefix(project_root)
                    .map_err(|_| EngineError::InvalidRuntimeConfig {
                        path: path.display().to_string(),
                        reason: "project config escaped admitted root".to_string(),
                    })?;
            let bytes = project_content
                .read_file(relative, MAX_CONFIG_SOURCE_BYTES)?
                .ok_or_else(|| EngineError::InvalidRuntimeConfig {
                    path: path.display().to_string(),
                    reason: "project config is absent from admitted content".to_string(),
                })?;
            String::from_utf8(bytes).map_err(|error| EngineError::InvalidRuntimeConfig {
                path: path.display().to_string(),
                reason: format!("config file is not UTF-8: {error}"),
            })?
        }
        _ => String::from_utf8(
            lillux::read_regular_file_bounded_no_follow(path, MAX_CONFIG_SOURCE_BYTES).map_err(
                |e| EngineError::InvalidRuntimeConfig {
                    path: path.display().to_string(),
                    reason: format!("could not read config file: {e}"),
                },
            )?,
        )
        .map_err(|error| EngineError::InvalidRuntimeConfig {
            path: path.display().to_string(),
            reason: format!("config file is not UTF-8: {error}"),
        })?,
    };
    let source_hash = lillux::sha256_hex(content.as_bytes());

    let kind_schema = ctx
        .kinds
        .get("config")
        .ok_or_else(|| EngineError::InvalidRuntimeConfig {
            path: path.display().to_string(),
            reason: "config kind not registered — required for config loading".to_string(),
        })?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_else(|| ".yaml".to_owned());
    let ext_spec = kind_schema
        .spec_for(&ext)
        .or_else(|| kind_schema.spec_for(".yaml"))
        .ok_or_else(|| EngineError::InvalidRuntimeConfig {
            path: path.display().to_string(),
            reason: format!("config kind has no extension spec for `{ext}`"),
        })?;
    let envelope = &ext_spec.signature;

    match parse_signature_header(&content, envelope) {
        None if require_trusted_signature => {
            return Err(EngineError::InvalidRuntimeConfig {
                path: path.display().to_string(),
                reason: "config must carry a signature trusted by this node".to_string(),
            });
        }
        None => tracing::warn!(
            config_path = %path.display(),
            "config file is unsigned (allow_unsigned=true)"
        ),
        Some(header) => {
            let recomputed = content_hash_after_signature(&content, envelope).ok_or_else(|| {
                EngineError::InvalidRuntimeConfig {
                    path: path.display().to_string(),
                    reason: "could not locate signature line in config file".to_string(),
                }
            })?;
            // Hash compare happens inside the verify call; a mismatch
            // surfaces as the hard ContentHashMismatch arm below while
            // other trust failures only warn (allow_unsigned policy).
            match verify_item_signature_with_hash(&recomputed, &header, ctx.trust_store) {
                Ok((crate::contracts::TrustClass::Trusted, _fp)) => tracing::debug!(
                    config_path = %path.display(),
                    "config file signature verified"
                ),
                Ok((trust, _fp)) if require_trusted_signature => {
                    return Err(EngineError::InvalidRuntimeConfig {
                        path: path.display().to_string(),
                        reason: format!("config signature is not trusted by this node: {trust:?}"),
                    });
                }
                Ok((trust, _fp)) => tracing::debug!(
                    config_path = %path.display(),
                    ?trust,
                    "config file signature verified"
                ),
                Err(EngineError::ContentHashMismatch {
                    expected, actual, ..
                }) => {
                    return Err(EngineError::ContentHashMismatch {
                        canonical_ref: path.display().to_string(),
                        expected,
                        actual,
                    });
                }
                Err(error) if require_trusted_signature => return Err(error),
                Err(error) => tracing::warn!(
                    config_path = %path.display(),
                    error = %error,
                    "config file signature trust check failed (allow_unsigned=true)"
                ),
            }
        }
    }

    let parsed = ctx
        .parsers
        .dispatch(&ext_spec.parser, &content, Some(path), envelope)?;
    if parsed.is_null() {
        Ok((Value::Object(Map::new()), source_hash))
    } else {
        Ok((parsed, source_hash))
    }
}

fn config_candidate_exists(
    path: &Path,
    space: ItemSpace,
    ctx: &ConfigLoadContext<'_>,
) -> Result<bool, EngineError> {
    match (space, ctx.project_authority) {
        (ItemSpace::Project, Some((project_root, project_content))) => {
            let relative =
                path.strip_prefix(project_root)
                    .map_err(|_| EngineError::InvalidRuntimeConfig {
                        path: path.display().to_string(),
                        reason: "project config candidate escaped admitted root".to_string(),
                    })?;
            project_content
                .validates_absence(relative)
                .map(|absent| !absent)
        }
        _ => Ok(path.exists()),
    }
}

pub fn deep_merge(base: Value, override_: Value) -> Value {
    match (base, override_) {
        (Value::Object(mut b), Value::Object(o)) => {
            for (k, v) in o {
                if k == "extends" {
                    continue;
                }
                let existing = b.remove(&k);
                let merged = match existing {
                    Some(existing_val) => deep_merge(existing_val, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            Value::Object(b)
        }
        (_, o) => o,
    }
}

#[cfg(test)]
mod strict_project_bundle_tests {
    use super::load_strict_signed_project_bundle_config;
    use crate::trust::{TrustStore, TrustedSigner, compute_fingerprint};
    use lillux::crypto::SigningKey;
    use std::{fs, path::Path};

    const CONFIG_PATH: &str =
        "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml";
    const MANIFEST_PATH: &str = "bundles/bundle-release/.ai/manifest.yaml";
    const MANIFEST: &str =
        "name: bundle-release\nversion: 0.1.0\nprovides_kinds: []\nrequires_kinds: []\n";

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn trust(keys: &[SigningKey]) -> TrustStore {
        TrustStore::from_signers(
            keys.iter()
                .map(|key| TrustedSigner {
                    fingerprint: compute_fingerprint(&key.verifying_key()),
                    verifying_key: key.verifying_key(),
                    label: None,
                })
                .collect(),
        )
    }

    fn write(root: &Path, relative: &str, bytes: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn signed(body: &str, key: &SigningKey) -> String {
        lillux::signature::sign_content(body, key, "#", None)
    }

    #[test]
    fn exact_project_config_requires_registered_publisher_signer() {
        let fixture = tempfile::tempdir().unwrap();
        let installed = fixture.path().join("installed-bundle");
        let project = fixture.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let publisher = key(42);
        let other_trusted = key(43);
        let node_trust = trust(&[publisher.clone(), other_trusted.clone()]);
        write(
            &installed,
            ".ai/manifest.yaml",
            &signed(MANIFEST, &publisher),
        );
        write(&project, MANIFEST_PATH, &signed(MANIFEST, &publisher));
        write(
            &project,
            CONFIG_PATH,
            &signed("purpose: exact\n", &publisher),
        );
        let pinned = lillux::PinnedDirectory::open(&project).unwrap().unwrap();
        let loaded = load_strict_signed_project_bundle_config(
            &project,
            &pinned,
            &node_trust,
            &installed,
            "bundle-release",
            CONFIG_PATH,
        )
        .unwrap();
        assert_eq!(loaded.value["purpose"], "exact");
        assert_eq!(
            loaded.signer_fingerprint,
            compute_fingerprint(&publisher.verifying_key())
        );

        // A second node-trusted key may not take over either source item.
        write(
            &project,
            CONFIG_PATH,
            &signed("purpose: changed\n", &other_trusted),
        );
        assert!(
            load_strict_signed_project_bundle_config(
                &project,
                &pinned,
                &node_trust,
                &installed,
                "bundle-release",
                CONFIG_PATH,
            )
            .is_err()
        );
        write(
            &project,
            CONFIG_PATH,
            &signed("purpose: exact\n", &publisher),
        );
        write(&project, MANIFEST_PATH, &signed(MANIFEST, &other_trusted));
        assert!(
            load_strict_signed_project_bundle_config(
                &project,
                &pinned,
                &node_trust,
                &installed,
                "bundle-release",
                CONFIG_PATH,
            )
            .is_err()
        );
    }

    #[test]
    fn exact_project_config_rejects_unsigned_missing_and_shadow_paths() {
        let fixture = tempfile::tempdir().unwrap();
        let installed = fixture.path().join("installed-bundle");
        let project = fixture.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let publisher = key(44);
        let node_trust = trust(&[publisher.clone()]);
        write(
            &installed,
            ".ai/manifest.yaml",
            &signed(MANIFEST, &publisher),
        );
        write(&project, MANIFEST_PATH, &signed(MANIFEST, &publisher));
        let pinned = lillux::PinnedDirectory::open(&project).unwrap().unwrap();
        assert!(
            load_strict_signed_project_bundle_config(
                &project,
                &pinned,
                &node_trust,
                &installed,
                "bundle-release",
                CONFIG_PATH,
            )
            .is_err()
        );
        write(&project, CONFIG_PATH, "purpose: unsigned\n");
        assert!(
            load_strict_signed_project_bundle_config(
                &project,
                &pinned,
                &node_trust,
                &installed,
                "bundle-release",
                CONFIG_PATH,
            )
            .is_err()
        );
        write(
            &project,
            CONFIG_PATH,
            &signed("purpose: signed\n", &publisher),
        );
        for path in [
            "bundles/other/.ai/config/bundle-release/payload-ownership.yaml",
            "bundles/bundle-release/.ai/config/../payload-ownership.yaml",
            "bundles/bundle-release/.ai/config//payload-ownership.yaml",
        ] {
            assert!(
                load_strict_signed_project_bundle_config(
                    &project,
                    &pinned,
                    &node_trust,
                    &installed,
                    "bundle-release",
                    path,
                )
                .is_err(),
                "{path}"
            );
        }
    }
}
