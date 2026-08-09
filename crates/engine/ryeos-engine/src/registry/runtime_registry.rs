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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::canonical_ref::CanonicalRef;
use crate::contracts::NativeResumeSpec;
use crate::error::EngineError;
use crate::kind_registry::KindRegistry;
use crate::resolution::TrustClass;
use crate::trust::TrustStore;

/// ABI version this daemon understands for runtime binaries.
/// Bundles shipping a different `abi_version` in their runtime YAML
/// are rejected at registry load — we fail closed at load, not at
/// dispatch.
///
/// Bump when the LaunchEnvelope, callback ABI, or any other
/// daemon↔runtime contract surface changes incompatibly.
pub const SUPPORTED_RUNTIME_ABI_VERSION: &str = "v3";

const MAX_LAUNCH_BINDINGS: usize = 32;
const MAX_LAUNCH_RUNTIME_DATA_KEYS: usize = 32;
const MAX_LAUNCH_CONFIG_INPUTS: usize = 16;
const MAX_LAUNCH_RUNTIME_FACTS: usize = 128;
const MAX_LAUNCH_EXECUTION_DEPENDENCIES: usize = 8;
const MAX_LAUNCH_SECRET_NAMES: usize = 32;
const MAX_LAUNCH_FACT_BYTES: u32 = 16 * 1024;
const MAX_LAUNCH_NAME_BYTES: usize = 64;
const MAX_CONFIG_IDENTITY_BYTES: usize = 512;
const MAX_CONFIG_SEGMENT_BYTES: usize = 128;
const MAX_CHILD_OBSERVATION_KINDS: usize = 64;
const MAX_CHILD_OBSERVATION_RECORDS: u32 = 1024;
const MAX_CHILD_OBSERVATION_RECORD_BYTES: u32 = 64 * 1024;
const MAX_CHILD_OBSERVATION_CLOCK_BYTES: usize = 128;
const MAX_RUNTIME_LIMIT_DIMENSIONS: usize = 64;

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
    /// ABI contract version, e.g. `"v3"`.
    pub abi_version: String,
    #[serde(default)]
    pub required_caps: Vec<String>,
    /// Complete, runtime-owned declaration of the inputs and preparation
    /// required to construct its launch envelope. This is intentionally
    /// required: adding a runtime without declaring its launch boundary is a
    /// boot-time error.
    pub launch_contract: LaunchContractDecl,
    /// Runtime-owned, signed declaration of structured child observations the
    /// daemon may capture from this runtime's stderr. Observation names and
    /// payloads remain opaque to the engine and executor; this declaration
    /// only binds framing, version, clock domain, and admission bounds.
    #[serde(default)]
    pub observability: RuntimeObservabilityDecl,
    /// Runtime-owned declaration of additional numeric hard-limit dimensions.
    /// The executor treats their names as opaque and only admits, validates,
    /// merges, and clamps values declared by this signed descriptor.
    #[serde(default)]
    pub limits: RuntimeLimitsDecl,
    #[serde(default)]
    pub description: Option<String>,
    /// Replay-aware resume policy for this runtime. Presence ⇒ this runtime
    /// owns its own checkpoint/resume: the daemon allocates a per-thread
    /// checkpoint dir and injects `RYEOS_CHECKPOINT_DIR` for runtime-registry
    /// launches of the kinds it serves (and `RYEOS_RESUME=1` on resume).
    /// Accepts `native_resume: true` or the rich object form; `false` is
    /// rejected — omit the field to disable. Shares
    /// [`NativeResumeSpec::parse_declaration`] with the engine's chain-element
    /// `native_resume` handler so both accept identical shapes.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_native_resume"
    )]
    pub native_resume: Option<NativeResumeSpec>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeObservabilityDecl {
    #[serde(default)]
    pub child_records: BTreeMap<String, ChildObservationDecl>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChildObservationDecl {
    pub schema_version: u32,
    pub clock_domain: String,
    pub max_records: u32,
    pub max_record_bytes: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLimitsDecl {
    /// Extensionless config identity supplying defaults and operator caps for
    /// this runtime. Omit when the runtime has no limits config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_identity: Option<String>,
    /// Stable identity for inheritance of the opaque dimensions. Parent limits
    /// clamp a child only when both runtimes declare this exact contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    /// Opaque dimension name to inclusive maximum wire value. Zero remains the
    /// shared unlimited sentinel; maximums bound parsing and runtime casts.
    #[serde(default)]
    pub dimensions: BTreeMap<String, u64>,
}

/// Declarative launch boundary for one runtime.
///
/// Every collection is required in YAML, including collections that are
/// empty. Runtime-specific launch knowledge belongs here or in the declared
/// launch-preparer handler, never in the executor.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchContractDecl {
    pub primary_allowed_kinds: Vec<String>,
    pub primary_allowed_spaces: Vec<LaunchItemSpace>,
    pub primary_allowed_trust: Vec<TrustClass>,
    pub ref_bindings: BTreeMap<String, RefBindingDecl>,
    pub preparation: LaunchPreparationDecl,
    pub config_inputs: BTreeMap<String, LaunchConfigInputDecl>,
    pub secret_policy: LaunchSecretPolicyDecl,
    pub required_runtime_data: Vec<String>,
    pub runtime_facts: BTreeMap<String, RuntimeFactDecl>,
    /// Signed mechanical ceiling for executable dependencies a kind-owned
    /// launch preparer may select. Resolution and trust are always derived by
    /// the engine; the handler supplies only canonical item refs.
    pub execution_dependencies: LaunchExecutionDependencyPolicy,
    /// Required declaration of the financial authority this runtime's launch
    /// preparation must produce. `none` states the runtime exercises no
    /// direct financial boundary; an accounting runtime declares the exact
    /// mechanical authority contract so a mismatched preparer cannot satisfy
    /// it silently.
    pub financial_authority: FinancialAuthorityDecl,
    /// Required external-effect authority contract. The family carried by the
    /// authority remains opaque to the engine; runtimes with no such boundary
    /// declare `none` explicitly.
    pub external_effect_authority: ExternalEffectAuthorityDecl,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchExecutionDependencyPolicy {
    pub max_dependencies: u16,
    pub allowed_kinds: Vec<String>,
    pub allowed_spaces: Vec<LaunchItemSpace>,
    pub allowed_trust: Vec<TrustClass>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FinancialAuthorityDecl {
    None,
    Accounting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalEffectAuthorityDecl {
    None,
    External,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RefBindingDecl {
    pub required: bool,
    pub allowed_kinds: Vec<String>,
    pub allowed_spaces: Vec<LaunchItemSpace>,
    pub allowed_trust: Vec<TrustClass>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchPreparationDecl {
    None,
    Handler {
        handler: String,
        config: serde_json::Value,
        execution_cache: LaunchPreparationExecutionCache,
    },
}

/// Mandatory signed execution-elision contract for a launch preparer.
///
/// Handler isolation and capability denial establish purity boundaries; this
/// declaration additionally authorizes omitting a repeated invocation for an
/// exact content-addressed request. There is no implicit legacy default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchPreparationExecutionCache {
    ContentAddressed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchConfigInputDecl {
    Item {
        id: String,
        required: bool,
        merge: ConfigMergeMode,
        allowed_spaces: Vec<LaunchItemSpace>,
        allowed_trust: Vec<TrustClass>,
    },
    Catalog {
        prefix: String,
        required: bool,
        entry_merge: ConfigMergeMode,
        allowed_spaces: Vec<LaunchItemSpace>,
        allowed_trust: Vec<TrustClass>,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigMergeMode {
    DeepMerge,
    FirstMatch,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchItemSpace {
    Bundle,
    Project,
    Node,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSecretPolicyDecl {
    pub max_requirements: u16,
    pub allowed_names: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFactDecl {
    pub required: bool,
    pub kind: RuntimeFactKind,
    /// Maximum length of the fact's canonical JSON representation, in bytes.
    pub max_bytes: u32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeFactKind {
    Bool,
    Integer,
    String,
    Json,
}

/// `deserialize_with` for `RuntimeYaml::native_resume`: route the present value
/// (a bool or a mapping) through the shared [`NativeResumeSpec::parse_declaration`]
/// so the runtime-registry YAML accepts the same `true` / object / rejected-`false`
/// shapes as the engine handler. Absent ⇒ `None` via `#[serde(default)]`.
fn deserialize_native_resume<'de, D>(de: D) -> Result<Option<NativeResumeSpec>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(de)?;
    NativeResumeSpec::parse_declaration(&value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

/// A runtime YAML that has been parsed AND trust-verified.
#[derive(Debug, Clone)]
pub struct VerifiedRuntime {
    pub canonical_ref: CanonicalRef,
    pub raw_content_digest: String,
    pub signer_fingerprint: String,
    pub yaml: RuntimeYaml,
    pub trust_class: TrustClass,
    pub bundle_root: PathBuf,
    pub descriptor_path: PathBuf,
}

impl VerifiedRuntime {
    /// Derive the native executor identity from this verified runtime's signed
    /// `bin/<target>/<binary>` reference.
    pub fn native_executor_ref(&self) -> Result<String, EngineError> {
        let relative = self.yaml.binary_ref.strip_prefix("bin/").ok_or_else(|| {
            EngineError::Internal(format!(
                "verified runtime `{}` has an invalid binary_ref `{}`",
                self.canonical_ref, self.yaml.binary_ref
            ))
        })?;
        let (_, binary) = relative.split_once('/').ok_or_else(|| {
            EngineError::Internal(format!(
                "verified runtime `{}` binary_ref has no target/binary boundary",
                self.canonical_ref
            ))
        })?;
        if binary.is_empty() {
            return Err(EngineError::Internal(format!(
                "verified runtime `{}` binary_ref has an empty binary identity",
                self.canonical_ref
            )));
        }
        Ok(format!("native:{binary}"))
    }
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
                validate_launch_contract_kinds(path, &verified.yaml, kinds)?;
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

        // A runtime may serve only an executable kind. The kind schema's
        // verified terminator/protocol remains authoritative; the registry
        // never binds a kind name to one built-in protocol ref.
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
            if schema.execution().is_none() {
                return Err(EngineError::RuntimeServesKindNoExecution {
                    kind: kind.clone(),
                    runtime: list[0].canonical_ref.to_string(),
                });
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
                        candidates: list.iter().map(|r| r.canonical_ref.to_string()).collect(),
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

    /// Resolve the serving runtime for a (re)launch.
    ///
    /// `None` runtime_ref → the kind's default runtime. `Some(ref)` → that
    /// exact runtime by-ref; a malformed or unregistered ref is an ERROR — never
    /// silently the kind default. Distinguishing the two matters for
    /// continuation/reconstruction: silently switching to today's default could
    /// change the binary, envelope requirements, or `native_resume` policy out
    /// from under a thread that already launched under a specific runtime.
    pub fn resolve_for_launch(
        &self,
        runtime_ref: Option<&str>,
        kind: &str,
    ) -> Result<&VerifiedRuntime, String> {
        match runtime_ref {
            Some(r) => {
                let canon = CanonicalRef::parse(r)
                    .map_err(|e| format!("malformed captured runtime_ref `{r}`: {e}"))?;
                let rt = self.lookup_by_ref(&canon).ok_or_else(|| {
                    format!("captured runtime_ref `{r}` is not a registered runtime")
                })?;
                // The ref must still serve the resumed kind — a registered-but-
                // repurposed runtime would hand back the wrong binary / envelope
                // requirements / native_resume policy.
                if rt.yaml.serves != kind {
                    return Err(format!(
                        "captured runtime_ref `{r}` serves kind `{}`, not requested kind `{kind}`",
                        rt.yaml.serves
                    ));
                }
                Ok(rt)
            }
            None => self
                .lookup_for(kind)
                .map_err(|e| format!("no runtime registered for kind `{kind}`: {e}")),
        }
    }

    pub fn all(&self) -> impl Iterator<Item = &VerifiedRuntime> {
        self.by_ref.values()
    }

    pub fn requires_launch_preparer(&self) -> bool {
        self.by_ref.values().any(|runtime| {
            matches!(
                &runtime.yaml.launch_contract.preparation,
                LaunchPreparationDecl::Handler { .. }
            )
        })
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
    let content =
        std::fs::read_to_string(yaml_path).map_err(|e| EngineError::RuntimeYamlInvalid {
            path: yaml_path.to_owned(),
            reason: format!("cannot read file: {e}"),
        })?;

    let sig_header =
        lillux::signature::parse_signature_line(content.lines().next().unwrap_or(""), "#", None)
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
        raw_content_digest: sig_header.content_hash.clone(),
        signer_fingerprint: sig_header.signer_fingerprint.clone(),
        yaml,
        trust_class: root_trust,
        bundle_root: bundle_root.to_owned(),
        descriptor_path: yaml_path.to_owned(),
    })
}

/// Parse a runtime YAML body into the typed view.
///
/// Pub(crate) so the integration tests can exercise the parser
/// directly without standing up a trust store / bundle directory.
pub(crate) fn parse_runtime_yaml(yaml_path: &Path, body: &str) -> Result<RuntimeYaml, EngineError> {
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
    validate_runtime_binary_ref(yaml_path, &yaml.binary_ref)?;
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
    validate_launch_contract(yaml_path, yaml)?;
    validate_runtime_observability(yaml_path, &yaml.observability)?;
    validate_runtime_limits(yaml_path, &yaml.limits)?;
    Ok(())
}

fn validate_runtime_limits(
    yaml_path: &Path,
    limits: &RuntimeLimitsDecl,
) -> Result<(), EngineError> {
    if limits.dimensions.len() > MAX_RUNTIME_LIMIT_DIMENSIONS {
        return runtime_yaml_error(
            yaml_path,
            format!("limits.dimensions exceeds the limit of {MAX_RUNTIME_LIMIT_DIMENSIONS}"),
        );
    }
    if let Some(identity) = limits.config_identity.as_deref() {
        validate_config_identity(yaml_path, "limits.config_identity", identity)?;
    } else if !limits.dimensions.is_empty() {
        return runtime_yaml_error(
            yaml_path,
            "limits.dimensions requires limits.config_identity",
        );
    }
    if let Some(contract) = limits.contract.as_deref() {
        validate_config_identity(yaml_path, "limits.contract", contract)?;
    } else if !limits.dimensions.is_empty() {
        return runtime_yaml_error(yaml_path, "limits.dimensions requires limits.contract");
    }
    for (name, maximum) in &limits.dimensions {
        validate_launch_name(yaml_path, "limits.dimensions", name)?;
        if *maximum == 0 {
            return runtime_yaml_error(
                yaml_path,
                format!("limits.dimensions.{name} must be greater than zero"),
            );
        }
    }
    Ok(())
}

fn validate_runtime_observability(
    yaml_path: &Path,
    observability: &RuntimeObservabilityDecl,
) -> Result<(), EngineError> {
    if observability.child_records.len() > MAX_CHILD_OBSERVATION_KINDS {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "observability.child_records exceeds the limit of {MAX_CHILD_OBSERVATION_KINDS}"
            ),
        );
    }
    let mut total_records = 0_u32;
    for (name, declaration) in &observability.child_records {
        validate_launch_name(yaml_path, "observability.child_records", name)?;
        if declaration.schema_version == 0 {
            return runtime_yaml_error(
                yaml_path,
                format!(
                    "observability.child_records.{name}.schema_version must be greater than zero"
                ),
            );
        }
        if declaration.clock_domain.is_empty()
            || declaration.clock_domain.len() > MAX_CHILD_OBSERVATION_CLOCK_BYTES
            || declaration.clock_domain.chars().any(char::is_control)
            || declaration.clock_domain.chars().any(char::is_whitespace)
        {
            return runtime_yaml_error(
                yaml_path,
                format!(
                    "observability.child_records.{name}.clock_domain must be a bounded non-whitespace label"
                ),
            );
        }
        if declaration.max_records == 0 {
            return runtime_yaml_error(
                yaml_path,
                format!("observability.child_records.{name}.max_records must be greater than zero"),
            );
        }
        total_records = total_records
            .checked_add(declaration.max_records)
            .ok_or_else(|| EngineError::RuntimeYamlInvalid {
                path: yaml_path.to_owned(),
                reason: "observability child-record count overflow".to_string(),
            })?;
        if declaration.max_record_bytes == 0
            || declaration.max_record_bytes > MAX_CHILD_OBSERVATION_RECORD_BYTES
        {
            return runtime_yaml_error(
                yaml_path,
                format!(
                    "observability.child_records.{name}.max_record_bytes must be in 1..={MAX_CHILD_OBSERVATION_RECORD_BYTES}"
                ),
            );
        }
    }
    if total_records > MAX_CHILD_OBSERVATION_RECORDS {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "observability.child_records declares {total_records} total records (max {MAX_CHILD_OBSERVATION_RECORDS})"
            ),
        );
    }
    Ok(())
}

/// Re-validate a runtime descriptor retained in an admitted execution
/// closure. The canonical ref supplies the original filename identity; no
/// runtime registry lookup participates.
pub fn validate_admitted_runtime_descriptor(
    canonical_ref: &CanonicalRef,
    yaml: &RuntimeYaml,
) -> Result<(), EngineError> {
    if canonical_ref.kind != "runtime" || canonical_ref.suffix.is_some() {
        return Err(EngineError::RuntimeYamlInvalid {
            path: PathBuf::from(&canonical_ref.bare_id),
            reason: "admitted runtime ref must be an unsuffixed runtime ref".to_string(),
        });
    }
    validate_runtime_yaml(
        &PathBuf::from(format!("{}.yaml", canonical_ref.bare_id)),
        yaml,
    )
}

fn validate_runtime_binary_ref(yaml_path: &Path, binary_ref: &str) -> Result<(), EngineError> {
    let parts: Vec<&str> = binary_ref.split('/').collect();
    let valid = parts.len() >= 3
        && parts[0] == "bin"
        && parts[1..].iter().all(|segment| {
            !segment.is_empty()
                && *segment != "."
                && *segment != ".."
                && !segment.contains('\\')
                && !segment.chars().any(char::is_whitespace)
                && !segment.chars().any(char::is_control)
        });
    if !valid {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "runtime binary_ref `{binary_ref}` has unexpected shape; expected `bin/<triple>/<binary>`"
            ),
        );
    }
    Ok(())
}

fn validate_launch_contract(yaml_path: &Path, yaml: &RuntimeYaml) -> Result<(), EngineError> {
    let contract = &yaml.launch_contract;

    validate_non_empty_unique(
        yaml_path,
        "launch_contract.primary_allowed_kinds",
        &contract.primary_allowed_kinds,
    )?;
    validate_non_empty_unique(
        yaml_path,
        "launch_contract.primary_allowed_spaces",
        &contract.primary_allowed_spaces,
    )?;
    validate_non_empty_unique(
        yaml_path,
        "launch_contract.primary_allowed_trust",
        &contract.primary_allowed_trust,
    )?;
    if contract
        .primary_allowed_spaces
        .contains(&LaunchItemSpace::Node)
        || contract
            .primary_allowed_trust
            .contains(&TrustClass::TrustedNode)
    {
        return runtime_yaml_error(
            yaml_path,
            "node source/trust authority is valid only for launch_contract.config_inputs",
        );
    }
    if !contract
        .primary_allowed_kinds
        .iter()
        .any(|kind| kind == &yaml.serves)
    {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "launch_contract.primary_allowed_kinds must include the served kind `{}`",
                yaml.serves
            ),
        );
    }

    if contract.ref_bindings.len() > MAX_LAUNCH_BINDINGS {
        return runtime_yaml_error(
            yaml_path,
            format!("launch_contract.ref_bindings exceeds the limit of {MAX_LAUNCH_BINDINGS}"),
        );
    }
    for (name, binding) in &contract.ref_bindings {
        validate_launch_name(yaml_path, "launch_contract.ref_bindings", name)?;
        validate_non_empty_unique(
            yaml_path,
            &format!("launch_contract.ref_bindings.{name}.allowed_kinds"),
            &binding.allowed_kinds,
        )?;
        validate_non_empty_unique(
            yaml_path,
            &format!("launch_contract.ref_bindings.{name}.allowed_spaces"),
            &binding.allowed_spaces,
        )?;
        validate_non_empty_unique(
            yaml_path,
            &format!("launch_contract.ref_bindings.{name}.allowed_trust"),
            &binding.allowed_trust,
        )?;
        if binding.allowed_spaces.contains(&LaunchItemSpace::Node)
            || binding.allowed_trust.contains(&TrustClass::TrustedNode)
        {
            return runtime_yaml_error(
                yaml_path,
                format!(
                    "launch_contract.ref_bindings.{name} cannot use node source/trust authority; node authority is config-only"
                ),
            );
        }
    }

    if let LaunchPreparationDecl::Handler { handler, .. } = &contract.preparation {
        let parsed = CanonicalRef::parse(handler).map_err(|error| {
            EngineError::RuntimeYamlInvalid {
                path: yaml_path.to_owned(),
                reason: format!(
                    "launch_contract.preparation.handler `{handler}` is not a canonical ref: {error}"
                ),
            }
        })?;
        if parsed.kind != "handler" || parsed.suffix.is_some() {
            return runtime_yaml_error(
                yaml_path,
                format!(
                    "launch_contract.preparation.handler must be an unsuffixed `handler:` ref, got `{handler}`"
                ),
            );
        }
    }

    if contract.config_inputs.len() > MAX_LAUNCH_CONFIG_INPUTS {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "launch_contract.config_inputs exceeds the limit of {MAX_LAUNCH_CONFIG_INPUTS}"
            ),
        );
    }
    for (name, input) in &contract.config_inputs {
        validate_launch_name(yaml_path, "launch_contract.config_inputs", name)?;
        let (identity_field, identity, allowed_spaces, allowed_trust) = match input {
            LaunchConfigInputDecl::Item {
                id,
                allowed_spaces,
                allowed_trust,
                ..
            } => ("id", id, allowed_spaces, allowed_trust),
            LaunchConfigInputDecl::Catalog {
                prefix,
                allowed_spaces,
                allowed_trust,
                ..
            } => ("prefix", prefix, allowed_spaces, allowed_trust),
        };
        validate_config_identity(
            yaml_path,
            &format!("launch_contract.config_inputs.{name}.{identity_field}"),
            identity,
        )?;
        validate_non_empty_unique(
            yaml_path,
            &format!("launch_contract.config_inputs.{name}.allowed_spaces"),
            allowed_spaces,
        )?;
        validate_non_empty_unique(
            yaml_path,
            &format!("launch_contract.config_inputs.{name}.allowed_trust"),
            allowed_trust,
        )?;
    }

    let secret_policy = &contract.secret_policy;
    if secret_policy.allowed_names.len() > MAX_LAUNCH_SECRET_NAMES {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "launch_contract.secret_policy.allowed_names exceeds the limit of {MAX_LAUNCH_SECRET_NAMES}"
            ),
        );
    }
    if usize::from(secret_policy.max_requirements) > MAX_LAUNCH_SECRET_NAMES {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "launch_contract.secret_policy.max_requirements exceeds the daemon limit of {MAX_LAUNCH_SECRET_NAMES}"
            ),
        );
    }
    if usize::from(secret_policy.max_requirements) > secret_policy.allowed_names.len() {
        return runtime_yaml_error(
            yaml_path,
            "launch_contract.secret_policy.max_requirements exceeds allowed_names length",
        );
    }
    if has_duplicates(&secret_policy.allowed_names) {
        return runtime_yaml_error(
            yaml_path,
            "launch_contract.secret_policy.allowed_names contains duplicates",
        );
    }
    for name in &secret_policy.allowed_names {
        crate::protocol_vocabulary::validate_env_name(name).map_err(|error| {
            EngineError::RuntimeYamlInvalid {
                path: yaml_path.to_owned(),
                reason: format!(
                    "launch_contract.secret_policy.allowed_names contains invalid name `{name}`: {error}"
                ),
            }
        })?;
    }

    if contract.required_runtime_data.len() > MAX_LAUNCH_RUNTIME_DATA_KEYS {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "launch_contract.required_runtime_data exceeds the limit of {MAX_LAUNCH_RUNTIME_DATA_KEYS}"
            ),
        );
    }
    if has_duplicates(&contract.required_runtime_data) {
        return runtime_yaml_error(
            yaml_path,
            "launch_contract.required_runtime_data contains duplicates",
        );
    }
    for name in &contract.required_runtime_data {
        validate_launch_name(yaml_path, "launch_contract.required_runtime_data", name)?;
    }

    if contract.runtime_facts.len() > MAX_LAUNCH_RUNTIME_FACTS {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "launch_contract.runtime_facts exceeds the limit of {MAX_LAUNCH_RUNTIME_FACTS}"
            ),
        );
    }
    for (name, fact) in &contract.runtime_facts {
        validate_launch_name(yaml_path, "launch_contract.runtime_facts", name)?;
        if fact.max_bytes == 0 || fact.max_bytes > MAX_LAUNCH_FACT_BYTES {
            return runtime_yaml_error(
                yaml_path,
                format!(
                    "launch_contract.runtime_facts.{name}.max_bytes must be in 1..={MAX_LAUNCH_FACT_BYTES}"
                ),
            );
        }
    }

    let dependency_policy = &contract.execution_dependencies;
    if usize::from(dependency_policy.max_dependencies) > MAX_LAUNCH_EXECUTION_DEPENDENCIES {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "launch_contract.execution_dependencies.max_dependencies exceeds the limit of {MAX_LAUNCH_EXECUTION_DEPENDENCIES}"
            ),
        );
    }
    for (field, values) in [
        ("allowed_kinds", dependency_policy.allowed_kinds.len()),
        ("allowed_spaces", dependency_policy.allowed_spaces.len()),
        ("allowed_trust", dependency_policy.allowed_trust.len()),
    ] {
        if (dependency_policy.max_dependencies == 0) != (values == 0) {
            return runtime_yaml_error(
                yaml_path,
                format!(
                    "launch_contract.execution_dependencies.{field} must be empty exactly when max_dependencies is zero"
                ),
            );
        }
    }
    if dependency_policy.max_dependencies != 0 {
        validate_non_empty_unique(
            yaml_path,
            "launch_contract.execution_dependencies.allowed_kinds",
            &dependency_policy.allowed_kinds,
        )?;
        validate_non_empty_unique(
            yaml_path,
            "launch_contract.execution_dependencies.allowed_spaces",
            &dependency_policy.allowed_spaces,
        )?;
        validate_non_empty_unique(
            yaml_path,
            "launch_contract.execution_dependencies.allowed_trust",
            &dependency_policy.allowed_trust,
        )?;
        if dependency_policy
            .allowed_spaces
            .contains(&LaunchItemSpace::Node)
            || dependency_policy
                .allowed_trust
                .contains(&TrustClass::TrustedNode)
        {
            return runtime_yaml_error(
                yaml_path,
                "launch execution dependencies cannot use node source/trust authority",
            );
        }
        if dependency_policy.allowed_spaces.as_slice() != [LaunchItemSpace::Bundle]
            || dependency_policy.allowed_trust.as_slice() != [TrustClass::TrustedBundle]
        {
            return runtime_yaml_error(
                yaml_path,
                "launch execution dependencies currently require exact installed bundle source and trusted-bundle provenance",
            );
        }
    }

    if matches!(&contract.preparation, LaunchPreparationDecl::None)
        && (!contract.config_inputs.is_empty()
            || secret_policy.max_requirements != 0
            || !secret_policy.allowed_names.is_empty()
            || !contract.required_runtime_data.is_empty()
            || !contract.runtime_facts.is_empty()
            || contract.execution_dependencies.max_dependencies != 0)
    {
        return runtime_yaml_error(
            yaml_path,
            "launch_contract.preparation kind `none` requires empty config inputs, secret policy, runtime data, runtime facts, and execution dependencies",
        );
    }

    Ok(())
}

fn validate_launch_contract_kinds(
    yaml_path: &Path,
    yaml: &RuntimeYaml,
    kinds: &KindRegistry,
) -> Result<(), EngineError> {
    let contract = &yaml.launch_contract;
    for kind in &contract.primary_allowed_kinds {
        // The existing registry-level `serves` check below reports the
        // dedicated RuntimeServesUnknownKind error for the served kind.
        if kind != &yaml.serves && !kinds.contains(kind) {
            return runtime_yaml_error(
                yaml_path,
                format!("launch_contract.primary_allowed_kinds contains unknown kind `{kind}`"),
            );
        }
    }
    for (name, binding) in &contract.ref_bindings {
        for kind in &binding.allowed_kinds {
            if !kinds.contains(kind) {
                return runtime_yaml_error(
                    yaml_path,
                    format!(
                        "launch_contract.ref_bindings.{name}.allowed_kinds contains unknown kind `{kind}`"
                    ),
                );
            }
        }
    }

    let registered_extensions: HashSet<&str> = if contract.config_inputs.is_empty() {
        HashSet::new()
    } else {
        kinds
            .extension_strs("config")
            .ok_or_else(|| EngineError::RuntimeYamlInvalid {
                path: yaml_path.to_owned(),
                reason: "launch_contract.config_inputs requires the registered `config` kind"
                    .to_owned(),
            })?
            .into_iter()
            .collect()
    };
    for (name, input) in &contract.config_inputs {
        let (field, identity) = match input {
            LaunchConfigInputDecl::Item { id, .. } => ("id", id),
            LaunchConfigInputDecl::Catalog { prefix, .. } => ("prefix", prefix),
        };
        if let Some(extension) = registered_extensions
            .iter()
            .find(|extension| identity.ends_with(**extension))
        {
            return runtime_yaml_error(
                yaml_path,
                format!(
                    "launch_contract.config_inputs.{name}.{field} must omit the registered file extension `{extension}`"
                ),
            );
        }
    }
    Ok(())
}

fn validate_non_empty_unique<T>(
    yaml_path: &Path,
    field: &str,
    values: &[T],
) -> Result<(), EngineError>
where
    T: Eq + Hash,
{
    if values.is_empty() {
        return runtime_yaml_error(yaml_path, format!("{field} must be non-empty"));
    }
    if has_duplicates(values) {
        return runtime_yaml_error(yaml_path, format!("{field} contains duplicates"));
    }
    Ok(())
}

fn has_duplicates<T>(values: &[T]) -> bool
where
    T: Eq + Hash,
{
    let mut seen = HashSet::with_capacity(values.len());
    values.iter().any(|value| !seen.insert(value))
}

fn validate_launch_name(yaml_path: &Path, field: &str, name: &str) -> Result<(), EngineError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_LAUNCH_NAME_BYTES
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !name.ends_with('_')
        && !name.contains("__");
    if !valid {
        return runtime_yaml_error(
            yaml_path,
            format!(
                "{field} name `{name}` must match [a-z][a-z0-9]*(?:_[a-z0-9]+)* and be at most {MAX_LAUNCH_NAME_BYTES} bytes"
            ),
        );
    }
    Ok(())
}

fn validate_config_identity(
    yaml_path: &Path,
    field: &str,
    identity: &str,
) -> Result<(), EngineError> {
    let valid = !identity.is_empty()
        && identity.len() <= MAX_CONFIG_IDENTITY_BYTES
        && !identity.starts_with('/')
        && !identity.ends_with('/')
        && !identity.contains('\\')
        && !identity.contains('\0')
        && identity.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.len() <= MAX_CONFIG_SEGMENT_BYTES
                && segment
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && segment.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_' || byte == b'-'
                })
        });
    if !valid {
        return runtime_yaml_error(
            yaml_path,
            format!("{field} `{identity}` is not a valid extensionless config identity"),
        );
    }
    Ok(())
}

fn runtime_yaml_error<T>(yaml_path: &Path, reason: impl Into<String>) -> Result<T, EngineError> {
    Err(EngineError::RuntimeYamlInvalid {
        path: yaml_path.to_owned(),
        reason: reason.into(),
    })
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
            launch_contract: LaunchContractDecl {
                primary_allowed_kinds: vec!["test_kind".to_owned()],
                primary_allowed_spaces: vec![LaunchItemSpace::Bundle],
                primary_allowed_trust: vec![TrustClass::TrustedBundle],
                ref_bindings: BTreeMap::new(),
                preparation: LaunchPreparationDecl::None,
                config_inputs: BTreeMap::new(),
                secret_policy: LaunchSecretPolicyDecl {
                    max_requirements: 0,
                    allowed_names: vec![],
                },
                required_runtime_data: vec![],
                runtime_facts: BTreeMap::new(),
                execution_dependencies: LaunchExecutionDependencyPolicy {
                    max_dependencies: 0,
                    allowed_kinds: vec![],
                    allowed_spaces: vec![],
                    allowed_trust: vec![],
                },
                financial_authority: FinancialAuthorityDecl::None,
                external_effect_authority: ExternalEffectAuthorityDecl::None,
            },
            observability: RuntimeObservabilityDecl::default(),
            limits: RuntimeLimitsDecl::default(),
            description: None,
            native_resume: None,
        }
    }

    fn test_path() -> PathBuf {
        PathBuf::from("/tmp/test-runtime.yaml")
    }

    /// Minimal valid runtime YAML body; callers append a `native_resume:` line.
    const BASE_YAML: &str = concat!(
        "kind: runtime\n",
        "serves: test_kind\n",
        "binary_ref: bin/test-triple/test\n",
        "abi_version: v3\n",
        "launch_contract:\n",
        "  primary_allowed_kinds: [test_kind]\n",
        "  primary_allowed_spaces: [bundle]\n",
        "  primary_allowed_trust: [trusted_bundle]\n",
        "  ref_bindings: {}\n",
        "  preparation:\n",
        "    kind: none\n",
        "  config_inputs: {}\n",
        "  secret_policy:\n",
        "    max_requirements: 0\n",
        "    allowed_names: []\n",
        "  required_runtime_data: []\n",
        "  runtime_facts: {}\n",
        "  execution_dependencies:\n",
        "    max_dependencies: 0\n",
        "    allowed_kinds: []\n",
        "    allowed_spaces: []\n",
        "    allowed_trust: []\n",
        "  financial_authority:\n",
        "    kind: none\n",
        "  external_effect_authority:\n",
        "    kind: none\n",
    );

    #[test]
    fn native_resume_absent_is_none() {
        let yaml: RuntimeYaml = serde_yaml::from_str(BASE_YAML).unwrap();
        assert!(yaml.native_resume.is_none());
    }

    #[test]
    fn native_resume_true_is_default_spec() {
        let body = format!("{BASE_YAML}native_resume: true\n");
        let yaml: RuntimeYaml = serde_yaml::from_str(&body).unwrap();
        assert_eq!(yaml.native_resume, Some(NativeResumeSpec::default()));
    }

    #[test]
    fn native_resume_object_form_parses_fields() {
        let body = format!(
            "{BASE_YAML}native_resume:\n  checkpoint_interval_secs: 5\n  max_auto_resume_attempts: 3\n"
        );
        let yaml: RuntimeYaml = serde_yaml::from_str(&body).unwrap();
        assert_eq!(
            yaml.native_resume,
            Some(NativeResumeSpec {
                checkpoint_interval_secs: 5,
                max_auto_resume_attempts: 3,
            })
        );
    }

    #[test]
    fn native_resume_object_form_defaults_missing_fields() {
        let body = format!("{BASE_YAML}native_resume:\n  checkpoint_interval_secs: 5\n");
        let yaml: RuntimeYaml = serde_yaml::from_str(&body).unwrap();
        // max_auto_resume_attempts defaults to the NativeResumeSpec default (1).
        assert_eq!(
            yaml.native_resume,
            Some(NativeResumeSpec {
                checkpoint_interval_secs: 5,
                max_auto_resume_attempts: NativeResumeSpec::default().max_auto_resume_attempts,
            })
        );
    }

    #[test]
    fn native_resume_false_is_rejected() {
        let body = format!("{BASE_YAML}native_resume: false\n");
        let err = serde_yaml::from_str::<RuntimeYaml>(&body).unwrap_err();
        assert!(
            err.to_string().contains("native_resume: false"),
            "error should explain the false rejection: {err}"
        );
    }

    #[test]
    fn native_resume_none_serializes_without_null() {
        // `skip_serializing_if` must omit the field entirely — emitting
        // `native_resume: null` would be rejected by the custom deserializer on
        // the round trip.
        let yaml = minimal_yaml(); // native_resume: None
        let s = serde_yaml::to_string(&yaml).expect("serialize");
        assert!(
            !s.contains("native_resume"),
            "None must be omitted, got:\n{s}"
        );
        let _round: RuntimeYaml = serde_yaml::from_str(&s).expect("round-trips");
    }

    fn registry_with(serves: &str, ref_str: &str) -> RuntimeRegistry {
        let mut yaml = minimal_yaml();
        yaml.serves = serves.to_owned();
        yaml.launch_contract.primary_allowed_kinds = vec![serves.to_owned()];
        let canon = CanonicalRef::parse(ref_str).expect("valid ref");
        let vr = VerifiedRuntime {
            canonical_ref: canon.clone(),
            raw_content_digest: "0".repeat(64),
            signer_fingerprint: "fp:test-runtime".to_string(),
            yaml,
            trust_class: TrustClass::TrustedBundle,
            bundle_root: test_path(),
            descriptor_path: test_path().join("runtime.yaml"),
        };
        let mut reg = RuntimeRegistry::default();
        reg.by_kind
            .entry(serves.to_owned())
            .or_default()
            .push(vr.clone());
        reg.by_ref.insert(canon, vr);
        reg
    }

    #[test]
    fn resolve_for_launch_none_uses_kind_default() {
        let reg = registry_with("graph", "runtime:graph-runtime");
        let rt = reg.resolve_for_launch(None, "graph").expect("kind default");
        assert_eq!(rt.yaml.serves, "graph");
    }

    #[test]
    fn resolve_for_launch_some_resolves_exact_ref() {
        let reg = registry_with("graph", "runtime:graph-runtime");
        let rt = reg
            .resolve_for_launch(Some("runtime:graph-runtime"), "graph")
            .expect("by-ref");
        assert_eq!(
            rt.canonical_ref,
            CanonicalRef::parse("runtime:graph-runtime").unwrap()
        );
    }

    #[test]
    fn resolve_for_launch_malformed_ref_errors() {
        let reg = registry_with("graph", "runtime:graph-runtime");
        let err = reg
            .resolve_for_launch(Some("not a ref"), "graph")
            .unwrap_err();
        assert!(err.contains("malformed"), "got: {err}");
    }

    #[test]
    fn resolve_for_launch_unregistered_ref_errors() {
        let reg = registry_with("graph", "runtime:graph-runtime");
        let err = reg
            .resolve_for_launch(Some("runtime:other-runtime"), "graph")
            .unwrap_err();
        assert!(err.contains("not a registered runtime"), "got: {err}");
    }

    #[test]
    fn resolve_for_launch_wrong_serves_kind_errors() {
        // Registered + parseable, but the runtime serves a different kind.
        let reg = registry_with("graph", "runtime:graph-runtime");
        let err = reg
            .resolve_for_launch(Some("runtime:graph-runtime"), "directive")
            .unwrap_err();
        assert!(err.contains("serves kind"), "got: {err}");
    }

    #[test]
    fn native_resume_empty_object_is_all_defaults() {
        // `native_resume: {}` ⇒ the rich form with every field defaulted,
        // i.e. the same as `native_resume: true`.
        let body = format!("{BASE_YAML}native_resume: {{}}\n");
        let yaml: RuntimeYaml = serde_yaml::from_str(&body).unwrap();
        assert_eq!(yaml.native_resume, Some(NativeResumeSpec::default()));
    }

    #[test]
    fn native_resume_unknown_field_is_rejected() {
        let body = format!("{BASE_YAML}native_resume:\n  bogus: 1\n");
        let err = serde_yaml::from_str::<RuntimeYaml>(&body)
            .expect_err("unknown native_resume field must be rejected (deny_unknown_fields)");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("native_resume"),
            "error should name the unknown field and the native_resume context: {msg}"
        );
    }

    #[test]
    fn accepts_runtime_with_supported_abi_version() {
        let yaml = minimal_yaml();
        assert!(
            validate_runtime_yaml(&test_path(), &yaml).is_ok(),
            "the supported runtime ABI should be accepted"
        );
    }

    #[test]
    fn runtime_limits_require_a_bounded_signed_contract() {
        let mut yaml = minimal_yaml();
        yaml.limits.config_identity = Some("example-runtime/limits".to_string());
        yaml.limits.contract = Some("example-runtime/v1".to_string());
        yaml.limits.dimensions.insert("actions".to_string(), 100);
        validate_runtime_yaml(&test_path(), &yaml).expect("valid runtime limit declaration");

        yaml.limits.contract = None;
        let error = validate_runtime_yaml(&test_path(), &yaml)
            .expect_err("dimensions without an inheritance contract must fail");
        assert!(error.to_string().contains("limits.contract"));

        yaml.limits.contract = Some("example-runtime/v1".to_string());
        yaml.limits.dimensions.insert("actions".to_string(), 0);
        let error = validate_runtime_yaml(&test_path(), &yaml)
            .expect_err("a zero maximum cannot bound an admitted dimension");
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn child_observations_are_bounded_by_the_runtime_descriptor() {
        let mut yaml = minimal_yaml();
        yaml.observability.child_records.insert(
            "phase_sample".to_string(),
            ChildObservationDecl {
                schema_version: 1,
                clock_domain: "child_monotonic".to_string(),
                max_records: 4,
                max_record_bytes: 4096,
            },
        );
        validate_runtime_yaml(&test_path(), &yaml).expect("valid observation declaration");

        yaml.observability
            .child_records
            .get_mut("phase_sample")
            .expect("inserted declaration")
            .max_record_bytes = MAX_CHILD_OBSERVATION_RECORD_BYTES + 1;
        let error = validate_runtime_yaml(&test_path(), &yaml)
            .expect_err("oversized child records must fail closed");
        assert!(error.to_string().contains("max_record_bytes"));
    }

    #[test]
    fn rejects_config_only_node_authority_for_primary_items() {
        let mut yaml = minimal_yaml();
        yaml.launch_contract.primary_allowed_spaces =
            vec![LaunchItemSpace::Bundle, LaunchItemSpace::Node];
        let error = validate_runtime_yaml(&test_path(), &yaml)
            .expect_err("node space must remain launch-config-only");
        assert!(error.to_string().contains("config_inputs"));

        let mut yaml = minimal_yaml();
        yaml.launch_contract.primary_allowed_trust =
            vec![TrustClass::TrustedBundle, TrustClass::TrustedNode];
        let error = validate_runtime_yaml(&test_path(), &yaml)
            .expect_err("node trust must remain launch-config-only");
        assert!(error.to_string().contains("config_inputs"));
    }

    #[test]
    fn rejects_config_only_node_authority_for_ref_bindings() {
        let mut yaml = minimal_yaml();
        yaml.launch_contract.ref_bindings.insert(
            "model".to_string(),
            RefBindingDecl {
                required: true,
                allowed_kinds: vec!["test_kind".to_string()],
                allowed_spaces: vec![LaunchItemSpace::Node],
                allowed_trust: vec![TrustClass::TrustedNode],
            },
        );
        let error = validate_runtime_yaml(&test_path(), &yaml)
            .expect_err("node authority must not authorize general ref bindings");
        assert!(error.to_string().contains("config-only"));
    }

    #[test]
    fn runtime_binary_ref_validation_preserves_nested_binary_paths() {
        assert!(
            validate_runtime_binary_ref(
                &test_path(),
                "bin/x86_64-unknown-linux-gnu/tools/test-runtime",
            )
            .is_ok()
        );
    }

    #[test]
    fn runtime_binary_ref_validation_rejects_malformed_or_unsafe_segments() {
        for binary_ref in [
            "badshape",
            "bin//test-runtime",
            "bin/../test-runtime",
            "bin/test-triple/../test-runtime",
            "bin/test triple/test-runtime",
            "bin/test-triple/test runtime",
            "bin/test-triple/bad\\name",
        ] {
            let error = validate_runtime_binary_ref(&test_path(), binary_ref)
                .expect_err("malformed runtime binary_ref must be rejected");
            assert!(
                error.to_string().contains(binary_ref),
                "diagnostic must identify `{binary_ref}`: {error}"
            );
        }
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
                assert_eq!(expected, SUPPORTED_RUNTIME_ABI_VERSION);
                assert_eq!(found, "v999");
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }
}
