use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::contracts::ItemSpace;
use crate::error::EngineError;
use crate::item_resolution::{parse_signature_header, ResolutionRoots};
use crate::kind_registry::KindRegistry;
use crate::parsers::dispatcher::ParserDispatcher;
use crate::trust::{content_hash_after_signature, verify_item_signature_with_hash, TrustStore};

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
                if candidate.exists() {
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
                    if candidate.exists() {
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
    let content = std::fs::read_to_string(path).map_err(|e| EngineError::InvalidRuntimeConfig {
        path: path.display().to_string(),
        reason: format!("could not read config file: {e}"),
    })?;

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
                Err(e) => tracing::warn!(
                    config_path = %path.display(),
                    error = %e,
                    "config file signature trust check failed (allow_unsigned=true)"
                ),
            }
        }
    }

    let parsed = ctx
        .parsers
        .dispatch(&ext_spec.parser, &content, Some(path), envelope)?;
    if parsed.is_null() {
        Ok(Value::Object(Map::new()))
    } else {
        Ok(parsed)
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
