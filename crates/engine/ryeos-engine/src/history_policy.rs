//! Typed, kind-agnostic thread-history policy resolution.
//!
//! A kind may bind one atomic top-level field in its final verified composed
//! value through `execution.history_policy.composed_path`. This module owns the
//! authored wire shape, the signed node valve/default, and the concrete launch
//! contract. It never reads `metadata.extra` or switches on a kind or item ref.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::{
    fs::File,
    io::Read,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::ffi::OsStrExt,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config_loading::ConfigLoadContext;
use crate::contracts::{ItemSpace, TrustClass, VerifiedItem};
use crate::error::EngineError;
use crate::item_resolution::parse_signature_header;
use crate::kind_registry::{KindRegistry, KindSchema};
use crate::resolution::{ResolutionOutput, TrustClass as ResolutionTrustClass};
use crate::trust::{content_hash_after_signature, verify_item_signature_with_hash};

/// Deserialize a nullable field while still requiring its key to be present.
/// The resolved policy is a closed current-format contract; a missing signer
/// key must not be confused with an explicitly unsigned item.
fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub const NODE_HISTORY_POLICY_CONFIG: &str = "config/execution/execution.yaml";

/// Concrete history behavior captured on a newly-created root chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadHistoryRetention {
    Durable,
    TerminalFor { seconds: u64 },
}

/// Whether a trusted item may supply the kind-declared authored override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemAuthoredRetentionMode {
    Allow,
    Prohibit,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeHistoryPolicyDocument {
    default_retention: Value,
    item_authored_retention: ItemAuthoredRetentionMode,
    #[serde(default)]
    minimum_terminal_for: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalRetentionDocument {
    terminal_for: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoredHistoryDocument {
    retention: TerminalRetentionDocument,
}

/// Auditable authority for the node-wide default and clamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeHistoryPolicyProvenance {
    /// No signed node block existed. This built-in carries no age: Durable,
    /// authored trusted overrides allowed, and no minimum clamp.
    MissingConfig,
    SignedConfig {
        path: PathBuf,
        space: ItemSpace,
        content_hash: String,
        signer_fingerprint: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNodeThreadHistoryPolicy {
    pub item_authored_retention: ItemAuthoredRetentionMode,
    pub default_retention: ThreadHistoryRetention,
    /// Lower bound for every finite terminal-retention decision, including
    /// the node default and any trusted item-authored override.
    pub minimum_terminal_for_seconds: Option<u64>,
    pub provenance: NodeHistoryPolicyProvenance,
}

impl ResolvedNodeThreadHistoryPolicy {
    pub fn durable_without_config() -> Self {
        Self {
            item_authored_retention: ItemAuthoredRetentionMode::Allow,
            default_retention: ThreadHistoryRetention::Durable,
            minimum_terminal_for_seconds: None,
            provenance: NodeHistoryPolicyProvenance::MissingConfig,
        }
    }

    fn validate(&self) -> Result<(), EngineError> {
        if let NodeHistoryPolicyProvenance::SignedConfig { path, .. } = &self.provenance {
            if path != Path::new(NODE_HISTORY_POLICY_CONFIG) {
                return Err(invalid_node_policy(format!(
                    "signed node history provenance path must be exactly `{NODE_HISTORY_POLICY_CONFIG}`"
                )));
            }
        }
        let (ThreadHistoryRetention::TerminalFor { seconds }, Some(minimum_terminal_for_seconds)) =
            (&self.default_retention, self.minimum_terminal_for_seconds)
        else {
            return Ok(());
        };
        if *seconds < minimum_terminal_for_seconds {
            return Err(invalid_node_policy(format!(
                "`default_retention.terminal_for` resolves to {seconds} seconds, below `minimum_terminal_for` ({minimum_terminal_for_seconds} seconds)"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadHistoryMinimumClamp {
    pub requested_seconds: u64,
    pub minimum_seconds: u64,
}

/// Why the concrete retention value was selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PolicyProvenance {
    NodeDefault {
        node_policy: NodeHistoryPolicyProvenance,
    },
    ItemAuthored {
        composed_path: String,
        requested_seconds: u64,
        effective_trust_class: ResolutionTrustClass,
        minimum_clamp: Option<ThreadHistoryMinimumClamp>,
        node_policy: NodeHistoryPolicyProvenance,
    },
}

/// Fully resolved engine contract which root creation converts directly to
/// the state-owned captured wire type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedThreadHistoryPolicy {
    pub retention: ThreadHistoryRetention,
    pub canonical_item_ref: String,
    pub item_content_hash: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub item_signer_fingerprint: Option<String>,
    pub item_trust_class: TrustClass,
    pub kind_schema_content_hash: String,
    pub source: PolicyProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedLaunchPolicy {
    pub history: ResolvedThreadHistoryPolicy,
}

/// Input available at the normal verified-composition boundary. Callers pass
/// the final composed value directly; they never need to manufacture a
/// `ResolutionOutput` just to resolve launch policy.
pub struct ResolveLaunchPolicyInput<'a> {
    pub verified_item: &'a VerifiedItem,
    pub composed_value: &'a Value,
    pub effective_trust_class: ResolutionTrustClass,
    pub kinds: &'a KindRegistry,
    pub node_history: &'a ResolvedNodeThreadHistoryPolicy,
}

/// Load the highest-precedence signed node history block. Missing config is a
/// built-in Durable policy with no Rust age cutoff.
///
/// `ctx.roots` must be rooted at the node app tree, followed by bundle roots;
/// an executed project's tree must not be allowed to change node GC policy.
pub fn load_node_thread_history_policy(
    ctx: &ConfigLoadContext<'_>,
) -> Result<ResolvedNodeThreadHistoryPolicy, EngineError> {
    let config_kind = ctx
        .kinds
        .get("config")
        .ok_or_else(|| invalid_node_policy("config kind is not registered"))?;

    for root in &ctx.roots.ordered {
        let path = root.ai_root.join(NODE_HISTORY_POLICY_CONFIG);
        let raw = match read_optional_regular_file_no_follow(&path) {
            Ok(Some(raw)) => raw,
            Ok(None) => continue,
            Err(error) => {
                return Err(invalid_node_policy(format!(
                    "could not read {}: {error:#}",
                    path.display()
                )))
            }
        };
        let raw = String::from_utf8(raw).map_err(|error| {
            invalid_node_policy(format!("could not read {}: {error}", path.display()))
        })?;
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .ok_or_else(|| {
                invalid_node_policy(format!(
                    "node history policy path has no declared extension: {}",
                    path.display()
                ))
            })?;
        let extension_spec = config_kind.spec_for(&extension).ok_or_else(|| {
            invalid_node_policy(format!(
                "config kind has no extension declaration for `{extension}`"
            ))
        })?;
        let parsed = ctx.parsers.dispatch(
            &extension_spec.parser,
            &raw,
            Some(&path),
            &extension_spec.signature,
        )?;
        let Some(policy_value) = parsed.get("history") else {
            continue;
        };

        // This block authorizes destructive retirement, so the selected layer
        // must be signed by the active node trust store.
        let header = parse_signature_header(&raw, &extension_spec.signature).ok_or_else(|| {
            invalid_node_policy(format!(
                "node history policy {} is unsigned",
                path.display()
            ))
        })?;
        let actual_hash = content_hash_after_signature(&raw, &extension_spec.signature)
            .ok_or_else(|| {
                invalid_node_policy(format!("could not hash node policy {}", path.display()))
            })?;
        let (trust, signer) =
            verify_item_signature_with_hash(&actual_hash, &header, ctx.trust_store)?;
        if trust != TrustClass::Trusted {
            return Err(EngineError::UntrustedSigner {
                canonical_ref: path.display().to_string(),
                fingerprint: header.signer_fingerprint,
            });
        }
        let signer_fingerprint = signer
            .map(|value| value.0)
            .ok_or_else(|| invalid_node_policy("trusted policy has no signer fingerprint"))?;
        let provenance = NodeHistoryPolicyProvenance::SignedConfig {
            path: PathBuf::from(NODE_HISTORY_POLICY_CONFIG),
            space: root.space,
            content_hash: actual_hash,
            signer_fingerprint,
        };
        return resolve_node_thread_history_policy(policy_value.clone(), provenance);
    }

    Ok(ResolvedNodeThreadHistoryPolicy::durable_without_config())
}

#[cfg(unix)]
fn read_optional_regular_file_no_follow(path: &std::path::Path) -> anyhow::Result<Option<Vec<u8>>> {
    use std::path::Component;

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("node history policy path has no filename"))?;
    let file_name = std::ffi::CString::new(file_name.as_bytes())?;
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let start = if parent.is_absolute() { "/" } else { "." };
    let start = std::ffi::CString::new(start).expect("static path contains no NUL");
    let descriptor = unsafe {
        libc::open(
            start.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut directory = unsafe { File::from_raw_fd(descriptor) };
    for component in parent.components() {
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(component) => component,
            Component::ParentDir | Component::Prefix(_) => {
                anyhow::bail!("node history policy path contains an unsafe parent component")
            }
        };
        let component = std::ffi::CString::new(component.as_bytes())?;
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                // The policy is optional in every resolution root. Most
                // bundles do not carry config/execution, so an absent parent
                // is the same optional miss as an absent execution.yaml leaf.
                return Ok(None);
            }
            // A symlink, non-directory component, or inaccessible parent is
            // not an optional miss and must remain fail-closed.
            return Err(error.into());
        }
        directory = unsafe { File::from_raw_fd(descriptor) };
    }
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!("node history policy source is not a regular file");
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

#[cfg(not(unix))]
fn read_optional_regular_file_no_follow(path: &std::path::Path) -> anyhow::Result<Option<Vec<u8>>> {
    let _ = path;
    anyhow::bail!("secure no-follow node history policy loading is unavailable on this platform")
}

pub fn resolve_node_thread_history_policy(
    value: Value,
    provenance: NodeHistoryPolicyProvenance,
) -> Result<ResolvedNodeThreadHistoryPolicy, EngineError> {
    let document = serde_json::from_value::<NodeHistoryPolicyDocument>(value)
        .map_err(|error| invalid_node_policy(format!("invalid `history` policy: {error}")))?;
    let default_retention =
        parse_node_default_retention(document.default_retention).map_err(invalid_node_policy)?;
    let minimum_terminal_for_seconds = document
        .minimum_terminal_for
        .as_deref()
        .map(parse_terminal_duration)
        .transpose()
        .map_err(invalid_node_policy)?;
    let policy = ResolvedNodeThreadHistoryPolicy {
        item_authored_retention: document.item_authored_retention,
        default_retention,
        minimum_terminal_for_seconds,
        provenance,
    };
    policy.validate()?;
    Ok(policy)
}

pub fn resolve_launch_policy(
    input: ResolveLaunchPolicyInput<'_>,
) -> Result<ResolvedLaunchPolicy, EngineError> {
    let kind = input.verified_item.resolved.kind.as_str();
    let kind_schema = input
        .kinds
        .get(kind)
        .ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!("item uses unregistered kind `{kind}`"),
        })?;
    let kind_schema_content_hash =
        input
            .kinds
            .schema_content_hash(kind)
            .ok_or_else(|| EngineError::SchemaLoaderError {
                reason: format!("registered kind `{kind}` has no verified schema content hash"),
            })?;
    resolve_launch_policy_with_schema(
        input.verified_item,
        input.composed_value,
        input.effective_trust_class,
        kind_schema,
        kind_schema_content_hash,
        input.node_history,
    )
}

/// Convenience adapter for callers which already own the normal resolution
/// output. The core API above intentionally accepts the composed value itself.
pub fn resolve_launch_policy_from_resolution(
    verified_item: &VerifiedItem,
    resolution: &ResolutionOutput,
    kinds: &KindRegistry,
    node_history: &ResolvedNodeThreadHistoryPolicy,
) -> Result<ResolvedLaunchPolicy, EngineError> {
    let canonical_item_ref = verified_item.resolved.canonical_ref.to_string();
    if resolution.root.resolved_ref != canonical_item_ref {
        return Err(EngineError::InvalidMetadata {
            canonical_ref: canonical_item_ref,
            reason: format!(
                "launch-policy resolution root mismatch: verified `{}` but composed `{}`",
                verified_item.resolved.canonical_ref, resolution.root.resolved_ref
            ),
        });
    }
    // The composition pass may occur after the initial item resolution and
    // verification. Bind it back to the exact verified bytes, not merely the
    // same canonical ref, so an in-place source change cannot pair old trust
    // provenance with a newly authored destructive policy.
    let raw_digest = lillux::signature::content_hash(&resolution.root.raw_content);
    if resolution.root.raw_content_digest != raw_digest {
        return Err(EngineError::InvalidMetadata {
            canonical_ref: canonical_item_ref.clone(),
            reason: format!(
                "resolution root raw-content digest is internally inconsistent: declared `{}`, computed `{raw_digest}`",
                resolution.root.raw_content_digest
            ),
        });
    }
    // Resolution strips the signature envelope before exposing raw_content.
    // Its digest therefore binds to the signed body hash for signed items and
    // to the whole source hash for unsigned items.
    let expected_verified_digest = verified_item
        .resolved
        .signature_header
        .as_ref()
        .map(|header| header.content_hash.as_str())
        .unwrap_or(verified_item.resolved.content_hash.as_str());
    let actual_verified_digest = resolution.root.raw_content_digest.as_str();
    if actual_verified_digest != expected_verified_digest {
        return Err(EngineError::InvalidMetadata {
            canonical_ref: canonical_item_ref.clone(),
            reason: format!(
                "resolution root content changed after verification: expected digest `{expected_verified_digest}`, composed digest `{actual_verified_digest}`"
            ),
        });
    }
    resolve_launch_policy(ResolveLaunchPolicyInput {
        verified_item,
        composed_value: &resolution.composed.composed,
        effective_trust_class: resolution.effective_trust_class,
        kinds,
        node_history,
    })
}

fn resolve_launch_policy_with_schema(
    verified_item: &VerifiedItem,
    composed_value: &Value,
    effective_trust_class: ResolutionTrustClass,
    kind_schema: &KindSchema,
    kind_schema_content_hash: &str,
    node_history: &ResolvedNodeThreadHistoryPolicy,
) -> Result<ResolvedLaunchPolicy, EngineError> {
    // Resolved policies normally come from the signed loader, but this keeps
    // the destructive-retention boundary fail-closed if an in-memory caller
    // ever constructs an internally inconsistent policy.
    node_history.validate()?;
    let canonical_item_ref = verified_item.resolved.canonical_ref.to_string();
    let declaration = kind_schema
        .execution
        .as_ref()
        .and_then(|execution| execution.history_policy.as_ref());
    let authored =
        declaration.and_then(|declaration| composed_value.get(&declaration.composed_path));

    let (retention, source) = match (declaration, authored) {
        (Some(declaration), Some(value)) => {
            if node_history.item_authored_retention == ItemAuthoredRetentionMode::Prohibit {
                return Err(EngineError::InvalidMetadata {
                    canonical_ref: canonical_item_ref.clone(),
                    reason: format!(
                        "authored history policy at `{}` is prohibited by node policy",
                        declaration.composed_path
                    ),
                });
            }
            if verified_item.trust_class != TrustClass::Trusted {
                return Err(EngineError::InvalidMetadata {
                    canonical_ref: canonical_item_ref.clone(),
                    reason: format!(
                        "untrusted item cannot request terminal history retention at `{}`",
                        declaration.composed_path
                    ),
                });
            }
            if !matches!(
                effective_trust_class,
                ResolutionTrustClass::TrustedBundle | ResolutionTrustClass::TrustedProject
            ) {
                return Err(EngineError::InvalidMetadata {
                    canonical_ref: canonical_item_ref.clone(),
                    reason: format!(
                        "composed item cannot request terminal history retention at `{}` with effective trust `{effective_trust_class:?}`",
                        declaration.composed_path
                    ),
                });
            }
            let authored = serde_json::from_value::<AuthoredHistoryDocument>(value.clone())
                .map_err(|error| EngineError::InvalidMetadata {
                    canonical_ref: canonical_item_ref.clone(),
                    reason: format!(
                        "invalid authored history policy at `{}`: {error}",
                        declaration.composed_path
                    ),
                })?;
            let requested_seconds = parse_terminal_duration(&authored.retention.terminal_for)
                .map_err(|reason| EngineError::InvalidMetadata {
                    canonical_ref: canonical_item_ref.clone(),
                    reason: format!(
                        "invalid `{}.retention.terminal_for`: {reason}",
                        declaration.composed_path
                    ),
                })?;
            let (seconds, minimum_clamp) = match node_history.minimum_terminal_for_seconds {
                Some(minimum_seconds) if requested_seconds < minimum_seconds => (
                    minimum_seconds,
                    Some(ThreadHistoryMinimumClamp {
                        requested_seconds,
                        minimum_seconds,
                    }),
                ),
                _ => (requested_seconds, None),
            };
            (
                ThreadHistoryRetention::TerminalFor { seconds },
                PolicyProvenance::ItemAuthored {
                    composed_path: declaration.composed_path.clone(),
                    requested_seconds,
                    effective_trust_class,
                    minimum_clamp,
                    node_policy: node_history.provenance.clone(),
                },
            )
        }
        _ => (
            node_history.default_retention.clone(),
            PolicyProvenance::NodeDefault {
                node_policy: node_history.provenance.clone(),
            },
        ),
    };

    let discovered_item_signer = verified_item
        .signer
        .as_ref()
        .map(|value| value.0.clone())
        .or_else(|| {
            verified_item
                .resolved
                .signature_header
                .as_ref()
                .map(|header| header.signer_fingerprint.clone())
        });
    let item_signer_fingerprint = match verified_item.trust_class {
        TrustClass::Unsigned => None,
        TrustClass::Trusted | TrustClass::Untrusted => {
            let signer = discovered_item_signer.ok_or_else(|| EngineError::InvalidMetadata {
                canonical_ref: canonical_item_ref.clone(),
                reason: "signed item has no signer fingerprint".to_string(),
            })?;
            if !lillux::valid_hash(&signer) || signer.bytes().any(|byte| byte.is_ascii_uppercase())
            {
                return Err(EngineError::InvalidMetadata {
                    canonical_ref: canonical_item_ref.clone(),
                    reason: "signed item has a non-canonical signer fingerprint".to_string(),
                });
            }
            Some(signer)
        }
    };
    Ok(ResolvedLaunchPolicy {
        history: ResolvedThreadHistoryPolicy {
            retention,
            canonical_item_ref,
            item_content_hash: verified_item.resolved.content_hash.clone(),
            item_signer_fingerprint,
            item_trust_class: verified_item.trust_class,
            kind_schema_content_hash: kind_schema_content_hash.to_string(),
            source,
        },
    })
}

fn parse_node_default_retention(value: Value) -> Result<ThreadHistoryRetention, String> {
    if value.as_str() == Some("durable") {
        return Ok(ThreadHistoryRetention::Durable);
    }
    let terminal = serde_json::from_value::<TerminalRetentionDocument>(value).map_err(|error| {
        format!(
            "`default_retention` must be `durable` or exactly {{ terminal_for: <duration> }}: {error}"
        )
    })?;
    Ok(ThreadHistoryRetention::TerminalFor {
        seconds: parse_terminal_duration(&terminal.terminal_for)?,
    })
}

/// Parse exactly one positive base-10 integer followed by `s`, `m`, `h`, or
/// `d` into seconds.
///
/// The canonical authoritative timestamp domain ends at Unix second
/// 253,402,300,799 (`9999-12-31T23:59:59Z`). This upper bound is therefore the
/// exact largest duration whose deadline remains representable by signed
/// whole-second checked-add arithmetic for every accepted terminal instant.
pub const MAX_TERMINAL_DURATION_SECONDS: u64 = (i64::MAX as u64) - 253_402_300_799;

pub fn parse_terminal_duration(raw: &str) -> Result<u64, String> {
    if raw.len() < 2 || raw.trim() != raw {
        return Err(format!(
            "duration `{raw}` must be a positive integer followed by one of s|m|h|d"
        ));
    }
    let (digits, multiplier) = if let Some(digits) = raw.strip_suffix('s') {
        (digits, 1)
    } else if let Some(digits) = raw.strip_suffix('m') {
        (digits, 60)
    } else if let Some(digits) = raw.strip_suffix('h') {
        (digits, 60 * 60)
    } else if let Some(digits) = raw.strip_suffix('d') {
        (digits, 24 * 60 * 60)
    } else {
        let unit = raw.chars().next_back().unwrap_or_default();
        return Err(format!(
            "duration `{raw}` has unsupported unit `{unit}` (expected s|m|h|d)"
        ));
    };
    if digits.starts_with('0') || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "duration `{raw}` must use a positive base-10 integer with no leading zeroes"
        ));
    }
    let count = digits
        .parse::<u64>()
        .map_err(|_| format!("duration `{raw}` has an out-of-range integer component"))?;
    let seconds = count
        .checked_mul(multiplier)
        .ok_or_else(|| format!("duration `{raw}` overflows seconds"))?;
    if seconds > MAX_TERMINAL_DURATION_SECONDS {
        return Err(format!(
            "duration `{raw}` exceeds the maximum representable retention duration of {MAX_TERMINAL_DURATION_SECONDS} seconds"
        ));
    }
    Ok(seconds)
}

fn invalid_node_policy(reason: impl Into<String>) -> EngineError {
    EngineError::InvalidRuntimeConfig {
        path: NODE_HISTORY_POLICY_CONFIG.to_string(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::canonical_ref::CanonicalRef;
    use crate::contracts::{
        ItemMetadata, ResolvedItem, ResolvedSourceFormat, SignatureEnvelope, SignatureHeader,
        SignerFingerprint,
    };
    use crate::kind_registry::{ExecutionSchema, ThreadHistoryPolicyDecl};

    fn node_policy(
        mode: ItemAuthoredRetentionMode,
        default_retention: ThreadHistoryRetention,
        minimum: Option<u64>,
    ) -> ResolvedNodeThreadHistoryPolicy {
        ResolvedNodeThreadHistoryPolicy {
            item_authored_retention: mode,
            default_retention,
            minimum_terminal_for_seconds: minimum,
            provenance: NodeHistoryPolicyProvenance::MissingConfig,
        }
    }

    fn verified_item(trust_class: TrustClass) -> VerifiedItem {
        VerifiedItem {
            resolved: ResolvedItem {
                canonical_ref: CanonicalRef::parse("service:test/example").unwrap(),
                kind: "service".to_string(),
                source_path: PathBuf::from("/bundle/.ai/services/test/example.yaml"),
                source_space: ItemSpace::Bundle,
                resolved_from: "bundle:core".to_string(),
                shadowed: Vec::new(),
                materialized_project_root: None,
                raw_content_digest: "raw-item-hash".to_string(),
                content_hash: "item-hash".to_string(),
                signature_header: Some(SignatureHeader {
                    timestamp: "2026-01-01T00:00:00Z".to_string(),
                    content_hash: "raw-item-hash".to_string(),
                    signature_b64: "signature".to_string(),
                    signer_fingerprint: "22".repeat(32),
                }),
                source_format: ResolvedSourceFormat {
                    extension: ".yaml".to_string(),
                    parser: "parser:yaml".to_string(),
                    signature: SignatureEnvelope {
                        prefix: "#".to_string(),
                        suffix: None,
                        after_shebang: false,
                    },
                },
                metadata: ItemMetadata::default(),
            },
            signer: (trust_class == TrustClass::Trusted)
                .then(|| SignerFingerprint("22".repeat(32))),
            trust_class,
            pinned_version: None,
        }
    }

    fn kind_schema(opted_in: bool) -> KindSchema {
        KindSchema {
            directory: "services".to_string(),
            excluded_directories: Vec::new(),
            extensions: Vec::new(),
            extraction_rules: HashMap::new(),
            resolution: Vec::new(),
            effective_trust: Default::default(),
            execution: Some(ExecutionSchema {
                aliases: HashMap::new(),
                alias_max_depth: 8,
                terminator: None,
                delegate: None,
                thread_profile: None,
                history_policy: opted_in.then(|| ThreadHistoryPolicyDecl {
                    composed_path: "history".to_string(),
                }),
                method_dispatch: None,
                methods: Default::default(),
                launch_augmentations: Vec::new(),
            }),
            composed_value_contract: crate::contracts::ValueShape::any_mapping(),
            composer: "handler:identity".to_string(),
            composer_config: Value::Null,
            runtime: None,
            inventory_kinds: Vec::new(),
            inventory_schema_keys: Vec::new(),
        }
    }

    fn resolve(
        item: &VerifiedItem,
        composed: &Value,
        schema: &KindSchema,
        node: &ResolvedNodeThreadHistoryPolicy,
    ) -> Result<ResolvedLaunchPolicy, EngineError> {
        resolve_launch_policy_with_schema(
            item,
            composed,
            ResolutionTrustClass::TrustedBundle,
            schema,
            "kind-hash",
            node,
        )
    }

    #[test]
    fn strict_duration_accepts_only_reviewed_units() {
        assert_eq!(parse_terminal_duration("1s").unwrap(), 1);
        assert_eq!(parse_terminal_duration("15m").unwrap(), 900);
        assert_eq!(parse_terminal_duration("2d").unwrap(), 172_800);
        for raw in [
            "0s", "01h", "1.5h", "1h30m", " 1h", "1H", "1w", "1", "", "1é", "1💥", "1秒", "１s",
            "💥",
        ] {
            assert!(parse_terminal_duration(raw).is_err(), "accepted `{raw}`");
        }
        assert!(
            parse_terminal_duration(&format!("{}s", MAX_TERMINAL_DURATION_SECONDS + 1)).is_err()
        );
    }

    #[test]
    fn missing_node_config_is_durable_without_an_age() {
        let policy = ResolvedNodeThreadHistoryPolicy::durable_without_config();
        assert_eq!(policy.default_retention, ThreadHistoryRetention::Durable);
        assert_eq!(policy.minimum_terminal_for_seconds, None);
        assert_eq!(
            policy.provenance,
            NodeHistoryPolicyProvenance::MissingConfig
        );
    }

    #[test]
    fn absent_authored_history_uses_node_default_with_complete_identity() {
        let item = verified_item(TrustClass::Trusted);
        let node = node_policy(
            ItemAuthoredRetentionMode::Allow,
            ThreadHistoryRetention::Durable,
            None,
        );
        let resolved = resolve(&item, &json!({}), &kind_schema(true), &node).unwrap();
        assert_eq!(resolved.history.retention, ThreadHistoryRetention::Durable);
        assert_eq!(resolved.history.canonical_item_ref, "service:test/example");
        assert_eq!(resolved.history.item_content_hash, "item-hash");
        assert_eq!(
            resolved.history.item_signer_fingerprint,
            Some("22".repeat(32))
        );
        assert_eq!(resolved.history.kind_schema_content_hash, "kind-hash");
        assert!(matches!(
            resolved.history.source,
            PolicyProvenance::NodeDefault { .. }
        ));
    }

    #[test]
    fn resolved_signer_is_explicit_and_consistent_with_item_trust() {
        let node = node_policy(
            ItemAuthoredRetentionMode::Allow,
            ThreadHistoryRetention::Durable,
            None,
        );

        let unsigned = verified_item(TrustClass::Unsigned);
        let resolved = resolve(&unsigned, &json!({}), &kind_schema(true), &node).unwrap();
        assert_eq!(resolved.history.item_signer_fingerprint, None);

        let untrusted = verified_item(TrustClass::Untrusted);
        let resolved = resolve(&untrusted, &json!({}), &kind_schema(true), &node).unwrap();
        assert_eq!(
            resolved.history.item_signer_fingerprint,
            Some("22".repeat(32))
        );

        let mut missing = verified_item(TrustClass::Untrusted);
        missing.resolved.signature_header = None;
        assert!(resolve(&missing, &json!({}), &kind_schema(true), &node).is_err());

        let mut noncanonical = verified_item(TrustClass::Trusted);
        noncanonical.signer = Some(SignerFingerprint("AA".repeat(32)));
        assert!(resolve(&noncanonical, &json!({}), &kind_schema(true), &node).is_err());
    }

    #[test]
    fn resolved_policy_requires_nullable_signer_key_on_the_wire() {
        let item = verified_item(TrustClass::Unsigned);
        let node = node_policy(
            ItemAuthoredRetentionMode::Allow,
            ThreadHistoryRetention::Durable,
            None,
        );
        let resolved = resolve(&item, &json!({}), &kind_schema(true), &node).unwrap();
        let mut value = serde_json::to_value(&resolved.history).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("item_signer_fingerprint");
        assert!(serde_json::from_value::<ResolvedThreadHistoryPolicy>(value).is_err());
    }

    #[test]
    fn authored_terminal_policy_is_typed_and_clamped() {
        let item = verified_item(TrustClass::Trusted);
        let node = node_policy(
            ItemAuthoredRetentionMode::Allow,
            ThreadHistoryRetention::Durable,
            Some(86_400),
        );
        let resolved = resolve(
            &item,
            &json!({"history": {"retention": {"terminal_for": "30m"}}}),
            &kind_schema(true),
            &node,
        )
        .unwrap();
        assert_eq!(
            resolved.history.retention,
            ThreadHistoryRetention::TerminalFor { seconds: 86_400 }
        );
        assert!(matches!(
            resolved.history.source,
            PolicyProvenance::ItemAuthored {
                minimum_clamp: Some(ThreadHistoryMinimumClamp {
                    requested_seconds: 1_800,
                    minimum_seconds: 86_400
                }),
                ..
            }
        ));
    }

    #[test]
    fn authored_shape_is_strict_and_has_no_durable_magic_value() {
        let item = verified_item(TrustClass::Trusted);
        let node = node_policy(
            ItemAuthoredRetentionMode::Allow,
            ThreadHistoryRetention::Durable,
            None,
        );
        for value in [
            json!({"history": "durable"}),
            json!({"history": {"retention": "terminal_for:1d"}}),
            json!({"history": {"retention": {"terminal_for": "1d", "extra": true}}}),
        ] {
            assert!(resolve(&item, &value, &kind_schema(true), &node).is_err());
        }
    }

    #[test]
    fn node_can_prohibit_authored_retention() {
        let item = verified_item(TrustClass::Trusted);
        let node = node_policy(
            ItemAuthoredRetentionMode::Prohibit,
            ThreadHistoryRetention::Durable,
            None,
        );
        let error = resolve(
            &item,
            &json!({"history": {"retention": {"terminal_for": "1d"}}}),
            &kind_schema(true),
            &node,
        )
        .unwrap_err();
        assert!(error.to_string().contains("prohibited by node policy"));
    }

    #[test]
    fn untrusted_item_cannot_request_terminal_retention() {
        let item = verified_item(TrustClass::Unsigned);
        let node = ResolvedNodeThreadHistoryPolicy::durable_without_config();
        let error = resolve(
            &item,
            &json!({"history": {"retention": {"terminal_for": "1d"}}}),
            &kind_schema(true),
            &node,
        )
        .unwrap_err();
        assert!(error.to_string().contains("untrusted item"));
    }

    #[test]
    fn untrusted_composed_ancestor_cannot_supply_terminal_retention() {
        let item = verified_item(TrustClass::Trusted);
        let node = ResolvedNodeThreadHistoryPolicy::durable_without_config();
        let error = resolve_launch_policy_with_schema(
            &item,
            &json!({"history": {"retention": {"terminal_for": "1d"}}}),
            ResolutionTrustClass::UntrustedProject,
            &kind_schema(true),
            "kind-hash",
            &node,
        )
        .unwrap_err();
        assert!(error.to_string().contains("effective trust"));
    }

    #[test]
    fn undeclared_kind_does_not_read_a_history_field() {
        let item = verified_item(TrustClass::Trusted);
        let node = ResolvedNodeThreadHistoryPolicy::durable_without_config();
        let resolved = resolve(
            &item,
            &json!({"history": {"retention": {"terminal_for": "1d"}}}),
            &kind_schema(false),
            &node,
        )
        .unwrap();
        assert_eq!(resolved.history.retention, ThreadHistoryRetention::Durable);
    }

    #[test]
    fn resolution_adapter_rejects_same_ref_with_changed_content() {
        use crate::resolution::{
            KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor,
        };

        let item = verified_item(TrustClass::Trusted);
        let changed_body = "history:\n  retention:\n    terminal_for: 1s\n".to_string();
        let changed_digest = lillux::signature::content_hash(&changed_body);
        let resolution = ResolutionOutput {
            root: ResolvedAncestor {
                requested_id: item.resolved.canonical_ref.to_string(),
                resolved_ref: item.resolved.canonical_ref.to_string(),
                source_path: item.resolved.source_path.clone(),
                source_space: item.resolved.source_space,
                trust_class: ResolutionTrustClass::TrustedBundle,
                alias_resolution: None,
                added_by: ResolutionStepName::PipelineInit,
                raw_content: changed_body,
                source_content_digest: item.resolved.content_hash.clone(),
                raw_content_digest: changed_digest,
            },
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: HashMap::new(),
            effective_trust_class: ResolutionTrustClass::TrustedBundle,
            composed: KindComposedView::identity(json!({
                "history": {"retention": {"terminal_for": "1s"}}
            })),
        };

        let error = resolve_launch_policy_from_resolution(
            &item,
            &resolution,
            &KindRegistry::empty(),
            &ResolvedNodeThreadHistoryPolicy::durable_without_config(),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("content changed after verification"));
    }

    #[test]
    fn node_policy_document_uses_the_reviewed_wire_shape() {
        let policy = resolve_node_thread_history_policy(
            json!({
                "default_retention": {"terminal_for": "30d"},
                "item_authored_retention": "allow",
                "minimum_terminal_for": "7d"
            }),
            NodeHistoryPolicyProvenance::MissingConfig,
        )
        .unwrap();
        assert_eq!(
            policy.default_retention,
            ThreadHistoryRetention::TerminalFor { seconds: 2_592_000 }
        );
        assert_eq!(policy.minimum_terminal_for_seconds, Some(604_800));
    }

    #[test]
    fn node_policy_rejects_a_default_below_its_minimum() {
        let error = resolve_node_thread_history_policy(
            json!({
                "default_retention": {"terminal_for": "1d"},
                "item_authored_retention": "allow",
                "minimum_terminal_for": "7d"
            }),
            NodeHistoryPolicyProvenance::MissingConfig,
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("`default_retention.terminal_for` resolves to 86400 seconds, below `minimum_terminal_for` (604800 seconds)"));
    }

    #[test]
    fn signed_node_policy_provenance_requires_the_canonical_relative_config_path() {
        let error = resolve_node_thread_history_policy(
            json!({
                "default_retention": "durable",
                "item_authored_retention": "allow",
                "minimum_terminal_for": null
            }),
            NodeHistoryPolicyProvenance::SignedConfig {
                path: PathBuf::from("/app/.ai/config/execution/execution.yaml"),
                space: ItemSpace::Project,
                content_hash: "11".repeat(32),
                signer_fingerprint: "22".repeat(32),
            },
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("must be exactly `config/execution/execution.yaml`"));
    }

    #[test]
    fn launch_resolution_rejects_an_internally_inconsistent_node_policy() {
        let item = verified_item(TrustClass::Trusted);
        let node = node_policy(
            ItemAuthoredRetentionMode::Allow,
            ThreadHistoryRetention::TerminalFor { seconds: 60 },
            Some(3_600),
        );

        let error = resolve(&item, &json!({}), &kind_schema(true), &node).unwrap_err();
        assert!(error.to_string().contains("below `minimum_terminal_for`"));
    }

    #[test]
    fn durable_or_sufficient_node_defaults_obey_the_minimum() {
        for default_retention in [
            ThreadHistoryRetention::Durable,
            ThreadHistoryRetention::TerminalFor { seconds: 604_800 },
            ThreadHistoryRetention::TerminalFor { seconds: 1_209_600 },
        ] {
            let policy = node_policy(
                ItemAuthoredRetentionMode::Allow,
                default_retention,
                Some(604_800),
            );
            policy.validate().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn node_history_policy_reader_rejects_a_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.yaml");
        let link = dir.path().join("policy.yaml");
        std::fs::write(&target, b"history: {}\n").unwrap();
        symlink(&target, &link).unwrap();
        assert!(read_optional_regular_file_no_follow(&link).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn node_history_policy_reader_rejects_a_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let linked = dir.path().join("linked");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("policy.yaml"), b"history: {}\n").unwrap();
        symlink(&real, &linked).unwrap();
        assert!(read_optional_regular_file_no_follow(&linked.join("policy.yaml")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn node_history_policy_reader_treats_a_missing_leaf_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let policy = dir.path().join("config/execution/execution.yaml");
        std::fs::create_dir_all(policy.parent().unwrap()).unwrap();

        assert_eq!(read_optional_regular_file_no_follow(&policy).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn node_history_policy_reader_treats_missing_optional_parents_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let policy = dir.path().join("config/execution/execution.yaml");

        assert_eq!(read_optional_regular_file_no_follow(&policy).unwrap(), None);
    }
}
