//! Wire protocol for handler binaries (parsers, composers, and launch
//! preparers).
//!
//! Handler binaries read a single `HandlerRequest` from stdin as a
//! single JSON object (one-shot, pipe closed), do their work, and
//! write a single `HandlerResponse` to stdout (one-shot, then
//! exit). Tracing / logging goes to STDERR ONLY. Stdout is reserved
//! for the response envelope.
//!
//! Exit code:
//!   - 0 on a well-formed response (whether `Ok` or `Err`).
//!   - non-zero ONLY for unrecoverable failures that prevent
//!     producing any response (panic, OOM, malformed stdin).
//!
//! Timeout enforced by the engine; binaries do not need to set their
//! own.

use serde::de::{DeserializeOwned, DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt;

pub const HANDLER_PROTOCOL_JSON_MAX_DEPTH: usize = 32;

// ── Request / Response envelope ──────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum HandlerRequest {
    Parse(ParseRequest),
    ValidateParserConfig(ValidateParserConfigRequest),
    Compose(ComposeRequest),
    ValidateComposerConfig(ValidateComposerConfigRequest),
    LaunchPrepare(LaunchPrepareRequest),
    ValidateLaunchPreparerConfig(ValidateLaunchPreparerConfigRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum HandlerResponse {
    ParseOk {
        value: Value,
    },
    ParseErr {
        kind: ParseErrKind,
        message: String,
    },
    ValidateOk,
    /// Composer-specific validation success. Unlike the parser-only
    /// `ValidateOk`, this echoes the exact field-semantics requirements the
    /// composer validated. That makes an old or permissive composer unable to
    /// silently acknowledge a new security-sensitive composition contract.
    ValidateComposerOk {
        field_requirements: Vec<ComposerFieldRequirement>,
    },
    ValidateErr {
        message: String,
    },
    ComposeOk(ComposeSuccess),
    ComposeErr {
        step: ResolutionStepNameWire,
        reason: String,
    },
    LaunchPrepare {
        response: LaunchPrepareResponse,
    },
    ValidateLaunchPreparerConfig {
        response: ValidateLaunchPreparerConfigResponse,
    },
}

// ── Parser ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseRequest {
    pub parser_config: Value,
    pub content: String,
    /// Optional file path for diagnostics. Wire format only — handlers
    /// must not assume the file exists at this path on their fs.
    #[serde(default)]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateParserConfigRequest {
    pub parser_config: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ParseErrKind {
    Syntax,
    Schema,
    Internal,
}

// ── Composer ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeRequest {
    pub composer_config: Value,
    pub root: ComposeInput,
    /// Deepest ancestor first. Same semantic order as the engine
    /// in-process compose call site uses today.
    #[serde(default)]
    pub ancestors: Vec<ComposeInput>,
}

/// Slim ancestor payload. Strips raw_content / raw_content_digest /
/// alias_resolution / added_by / source_path / source_space from
/// ResolvedAncestor —
/// composers only need identity + trust + parsed value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeInput {
    pub item: ComposeItemContext,
    pub parsed: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeItemContext {
    pub requested_id: String,
    pub resolved_ref: String,
    pub trust_class: TrustClassWire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TrustClassWire {
    TrustedBundle,
    TrustedProject,
    UntrustedProject,
    Unsigned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateComposerConfigRequest {
    pub composer_config: Value,
    /// Exact top-level fields whose composition behavior is security- or
    /// lifecycle-sensitive. The engine derives these requirements from the
    /// verified kind schema; the verified composer must reject any config that
    /// cannot provide the requested semantics.
    pub field_requirements: Vec<ComposerFieldRequirement>,
}

/// One generic, top-level composition invariant requested by the engine.
///
/// This protocol deliberately knows nothing about the consumer of the field.
/// History retention, authorization, and future policy consumers all use the
/// same exact-value semantics rather than teaching the engine composer names
/// or strategy strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposerFieldRequirement {
    pub field: String,
    pub semantics: ComposerFieldSemantics,
}

/// Required behavior for one composed top-level field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ComposerFieldSemantics {
    /// The final field is exactly the root's value, or absent when the root
    /// omits it. The composer may not merge, normalize, synthesize, or inspect
    /// the value.
    RootVerbatim,
    /// The final field is exactly the root's complete value when present;
    /// otherwise it is exactly the nearest ancestor's complete value. No
    /// partial/deep merge or value reinterpretation is permitted.
    InheritOrReplace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeSuccess {
    pub composed: Value,
    #[serde(default)]
    pub derived: HashMap<String, Value>,
    #[serde(default)]
    pub policy_facts: HashMap<String, Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolutionStepNameWire {
    PipelineInit,
    ResolveExtendsChain,
    ResolveReferences,
}

// ── Launch preparation ──────────────────────────────────────────

/// Complete verified input to a pure, threadless launch preparer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchPrepareRequest {
    pub handler_config: Value,
    pub primary: LaunchPreparedItemWire,
    pub ref_bindings: BTreeMap<String, LaunchPreparedItemWire>,
    pub config_inputs: BTreeMap<String, LaunchConfigSnapshotWire>,
}

/// One independently resolved primary or bound execution identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchPreparedItemWire {
    pub canonical_ref: String,
    pub source_space: ItemSpaceWire,
    pub effective_trust_class: TrustClassWire,
    pub composed: LaunchComposedViewWire,
    /// Opaque, daemon-computed as-launched resolution digest.
    pub resolution_digest: Value,
}

/// Path-free composed view exposed to the launch preparer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchComposedViewWire {
    pub composed: Value,
    pub derived: BTreeMap<String, Value>,
    pub policy_facts: BTreeMap<String, Value>,
}

/// One verified, provenance-bearing launch configuration input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchConfigSnapshotWire {
    Item {
        present: bool,
        value: Option<Value>,
        value_digest: Option<String>,
        contributors: Vec<LaunchConfigContributorWire>,
    },
    Catalog {
        entries: BTreeMap<String, LaunchConfigEntryWire>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfigEntryWire {
    pub value: Value,
    pub value_digest: String,
    pub contributors: Vec<LaunchConfigContributorWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfigContributorWire {
    pub space: ItemSpaceWire,
    pub root_label: String,
    pub canonical_id: String,
    pub content_digest: String,
    pub trust_class: TrustClassWire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ItemSpaceWire {
    Bundle,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchPrepareSuccess {
    pub runtime_data: BTreeMap<String, Value>,
    pub required_secrets: Vec<LaunchSecretRequirement>,
    pub runtime_facts: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchPrepareResponse {
    Success { result: LaunchPrepareSuccess },
    Error { error: LaunchPrepareError },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSecretRequirement {
    pub name: String,
    pub origin: LaunchSecretOriginWire,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchSecretOriginWire {
    Binding {
        name: String,
    },
    ConfigInput {
        name: String,
        canonical_id: String,
        value_digest: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchPrepareError {
    pub code: String,
    pub message: String,
    pub classification: LaunchPrepareErrorClass,
    pub binding: Option<String>,
    pub details: BTreeMap<String, LaunchDiagnosticScalarWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum LaunchDiagnosticScalarWire {
    Bool(bool),
    Integer(i64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchPrepareErrorClass {
    Caller,
    Configuration,
    Internal,
}

// ── Launch-preparer configuration validation ────────────────────

/// Protocol-owned mirror of the normalized signed launch contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateLaunchPreparerConfigRequest {
    pub handler_config: Value,
    pub primary_allowed_kinds: Vec<String>,
    pub primary_allowed_spaces: Vec<ItemSpaceWire>,
    pub primary_allowed_trust: Vec<TrustClassWire>,
    pub ref_bindings: BTreeMap<String, RefBindingDeclWire>,
    pub config_inputs: BTreeMap<String, LaunchConfigInputDeclWire>,
    pub secret_policy: LaunchSecretPolicyDeclWire,
    pub required_runtime_data: Vec<String>,
    pub runtime_facts: BTreeMap<String, RuntimeFactDeclWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefBindingDeclWire {
    pub required: bool,
    pub allowed_kinds: Vec<String>,
    pub allowed_spaces: Vec<ItemSpaceWire>,
    pub allowed_trust: Vec<TrustClassWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchConfigInputDeclWire {
    Item {
        id: String,
        required: bool,
        merge: ConfigMergeModeWire,
        allowed_spaces: Vec<ItemSpaceWire>,
        allowed_trust: Vec<TrustClassWire>,
    },
    Catalog {
        prefix: String,
        required: bool,
        entry_merge: ConfigMergeModeWire,
        allowed_spaces: Vec<ItemSpaceWire>,
        allowed_trust: Vec<TrustClassWire>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigMergeModeWire {
    DeepMerge,
    FirstMatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSecretPolicyDeclWire {
    pub max_requirements: u16,
    pub allowed_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFactDeclWire {
    pub required: bool,
    pub kind: RuntimeFactKindWire,
    pub max_bytes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeFactKindWire {
    Bool,
    Integer,
    String,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateLaunchPreparerConfigSuccess {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValidateLaunchPreparerConfigResponse {
    Valid {
        result: ValidateLaunchPreparerConfigSuccess,
    },
    Invalid {
        code: String,
        message: String,
    },
}

// ── Strict JSON decoding ────────────────────────────────────────

/// Decode handler protocol JSON while rejecting duplicate object keys at
/// every nesting level. `serde_json`'s normal map deserializer is
/// last-write-wins, which is not acceptable for control-plane requests or
/// responses.
pub fn from_json_slice_strict<T>(input: &[u8]) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let value = StrictJsonValue { depth: 0 }.deserialize(&mut deserializer)?;
    deserializer.end()?;
    serde_json::from_value(value)
}

pub fn from_json_str_strict<T>(input: &str) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    from_json_slice_strict(input.as_bytes())
}

struct StrictJsonValue {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for StrictJsonValue {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonValueVisitor { depth: self.depth })
    }
}

struct StrictJsonValueVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for StrictJsonValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if self.depth >= HANDLER_PROTOCOL_JSON_MAX_DEPTH {
            return Err(A::Error::custom(format!(
                "JSON nesting exceeds {} levels",
                HANDLER_PROTOCOL_JSON_MAX_DEPTH
            )));
        }
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        while let Some(value) = sequence.next_element_seed(StrictJsonValue {
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut mapping: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        if self.depth >= HANDLER_PROTOCOL_JSON_MAX_DEPTH {
            return Err(A::Error::custom(format!(
                "JSON nesting exceeds {} levels",
                HANDLER_PROTOCOL_JSON_MAX_DEPTH
            )));
        }
        let mut values = serde_json::Map::new();
        while let Some(key) = mapping.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom(format!(
                    "duplicate JSON object key `{key}`"
                )));
            }
            let value = mapping.next_value_seed(StrictJsonValue {
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_round_trips() {
        let req = HandlerRequest::Parse(ParseRequest {
            parser_config: serde_json::json!({"strict": true}),
            content: "key: value\n".into(),
            source_path: Some("/tmp/x.yaml".into()),
        });
        let s = serde_json::to_string(&req).unwrap();
        let back: HandlerRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, HandlerRequest::Parse(_)));
    }

    #[test]
    fn compose_request_strips_to_slim_input() {
        let req = HandlerRequest::Compose(ComposeRequest {
            composer_config: serde_json::json!({}),
            root: ComposeInput {
                item: ComposeItemContext {
                    requested_id: "@alias".into(),
                    resolved_ref: "directive:foo".into(),
                    trust_class: TrustClassWire::TrustedBundle,
                },
                parsed: serde_json::json!({"body": "hi"}),
            },
            ancestors: vec![],
        });
        let s = serde_json::to_string(&req).unwrap();
        // Must NOT contain raw_content / source_path / added_by /
        // alias_resolution — confirm the slim shape.
        assert!(!s.contains("raw_content"));
        assert!(!s.contains("source_path"));
        assert!(!s.contains("added_by"));
        assert!(!s.contains("alias_resolution"));
    }

    #[test]
    fn handler_response_variants_serialize_with_result_tag() {
        let r = HandlerResponse::ParseOk {
            value: serde_json::json!(42),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""result":"parse_ok""#));
    }

    #[test]
    fn composer_validation_round_trips_exact_field_requirements() {
        let requirements = vec![ComposerFieldRequirement {
            field: "lifecycle_policy".into(),
            semantics: ComposerFieldSemantics::InheritOrReplace,
        }];
        let request = HandlerRequest::ValidateComposerConfig(ValidateComposerConfigRequest {
            composer_config: serde_json::json!({"fields": []}),
            field_requirements: requirements.clone(),
        });
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: HandlerRequest = serde_json::from_str(&encoded).unwrap();
        match decoded {
            HandlerRequest::ValidateComposerConfig(decoded) => {
                assert_eq!(decoded.field_requirements, requirements);
            }
            other => panic!("unexpected request: {other:?}"),
        }

        let response = HandlerResponse::ValidateComposerOk {
            field_requirements: requirements,
        };
        assert!(serde_json::to_string(&response)
            .unwrap()
            .contains("validate_composer_ok"));
    }
}
