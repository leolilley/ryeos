use std::collections::HashMap;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Kind-owned typed view of the directive fields consumed during execution.
///
/// The generic composer preserves other fields owned by launch preparation and
/// the engine. [`parse_effective_header`] projects the composed mapping to this
/// exact surface before strict deserialization, so admission and runtime use
/// one implementation without teaching generic substrate directive semantics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectiveHeader {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub effects: DirectiveEffectClass,
    #[serde(default)]
    pub provider_contact: ProviderContact,
    #[serde(default)]
    pub limits: Option<LimitsSpec>,
    #[serde(default)]
    pub outputs: Option<Vec<OutputSpec>>,
    #[serde(default)]
    pub context: Option<HashMap<String, Vec<String>>>,
    #[serde(default)]
    pub hooks: Option<Vec<ryeos_runtime::HookDefinition>>,
    #[serde(default)]
    pub continuation: ContinuationConfig,
    #[serde(default)]
    pub return_nudge: ReturnNudge,
}

impl DirectiveHeader {
    fn validate(&self) -> Result<()> {
        if let Some(hooks) = &self.hooks {
            for (index, hook) in hooks.iter().enumerate() {
                if hook.event == "continuation" && !self.continuation.enabled() {
                    anyhow::bail!(
                        "directive hooks[{index}] declares `continuation` while continuation is disabled"
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DirectiveEffectClass {
    #[default]
    Live,
    Recorded,
    Sealed,
}

impl DirectiveEffectClass {
    pub const fn admitted(self) -> Option<ryeos_effect_contract::EffectClass> {
        match self {
            Self::Live => None,
            Self::Recorded => Some(ryeos_effect_contract::EffectClass::Recorded),
            Self::Sealed => Some(ryeos_effect_contract::EffectClass::Sealed),
        }
    }

    pub const fn records_provider_calls(self) -> bool {
        !matches!(self, Self::Live)
    }
}

/// Author declaration about issuing this directive's model calls more than
/// once. `Repeatable` vouches that a second identical call has no effect
/// beyond its charge (no provider-hosted tools, no server-side state). It is
/// independent of [`DirectiveEffectClass`], which describes durable record and
/// replay of the answer.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderContact {
    #[default]
    Once,
    Repeatable,
}

/// Classes of provider failure after which it is unknown whether the provider
/// acted on the request. Closed set: each member is a classification outcome
/// of the runtime's provider stream.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum UnknownOutcomeClass {
    Timeout,
    Send,
    MidStream,
}

/// Signed execution-config policy for retrying after an unknown provider
/// outcome. Absence of the block means such outcomes are never retried.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UnknownOutcomeRetryPolicy {
    /// The first N retries of a turn may follow an unknown outcome.
    pub max_retries: u32,
    pub on: Vec<UnknownOutcomeClass>,
}

impl UnknownOutcomeRetryPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.on.is_empty() {
            anyhow::bail!("execution.unknown_outcome_retry.on must name at least one class");
        }
        let mut seen = Vec::with_capacity(self.on.len());
        for class in &self.on {
            if seen.contains(class) {
                anyhow::bail!("execution.unknown_outcome_retry.on lists a class twice");
            }
            seen.push(*class);
        }
        Ok(())
    }
}

/// Number of leading retries of one turn that may follow an unknown provider
/// outcome. Zero unless the directive declares [`ProviderContact::Repeatable`],
/// the selected provider is a remote transport, and the signed policy admits
/// retries. The launch preparer seals this value and the runtime derives the
/// same value, so both sides share one implementation.
pub fn unknown_outcome_retry_ceiling(
    header: &DirectiveHeader,
    provider: &crate::ProviderConfig,
    policy: Option<&UnknownOutcomeRetryPolicy>,
) -> u32 {
    match (header.provider_contact, &provider.transport, policy) {
        (
            ProviderContact::Repeatable,
            crate::ProviderTransportConfig::RemoteHttp { .. },
            Some(policy),
        ) => policy.max_retries,
        _ => 0,
    }
}

/// Top-level composed fields that form the execution-facing directive header.
pub const DIRECTIVE_EFFECTIVE_HEADER_KEYS: &[&str] = &[
    "name",
    "extends",
    "effects",
    "provider_contact",
    "limits",
    "outputs",
    "context",
    "hooks",
    "continuation",
    "return_nudge",
];

/// Parse the execution-facing header from one already-composed directive.
///
/// This function is intentionally independent of `KindComposedView`: callers
/// pass its `composed` value, keeping the kind definition below engine
/// resolution while ensuring every trust transition invokes the same parser.
pub fn parse_effective_header(composed: &Value) -> Result<DirectiveHeader> {
    let projected = match composed.as_object() {
        Some(map) => {
            let mut out = serde_json::Map::with_capacity(DIRECTIVE_EFFECTIVE_HEADER_KEYS.len());
            for &key in DIRECTIVE_EFFECTIVE_HEADER_KEYS {
                if let Some(value) = map.get(key) {
                    out.insert(key.to_owned(), value.clone());
                }
            }
            Value::Object(out)
        }
        None => composed.clone(),
    };
    let header: DirectiveHeader = serde_json::from_value(projected)
        .map_err(|error| anyhow!("deserialize composed directive header: {error}"))?;
    header.validate()?;
    Ok(header)
}

/// Derive the provider-effect authority from the same parsed directive header
/// consumed by the runtime. Transport ceilings are provider-owned and are
/// checked here before generic launch machinery seals the authority.
pub fn resolve_external_effect_authority(
    header: &DirectiveHeader,
    provider: &crate::ProviderConfig,
    unknown_outcome_policy: Option<&UnknownOutcomeRetryPolicy>,
) -> Result<ryeos_effect_contract::AdmittedExternalEffectAuthority> {
    let admitted_effect_class = header.effects.admitted();
    if admitted_effect_class
        .is_some_and(|requested| !provider.transport.effect_class_ceiling().permits(requested))
    {
        anyhow::bail!(
            "directive effect class exceeds the selected provider transport's admitted boundary"
        );
    }
    let family = serde_json::to_value(provider.family)?;
    let family = family
        .as_str()
        .ok_or_else(|| anyhow!("provider family did not serialize as a string"))?;
    let authority = ryeos_effect_contract::AdmittedExternalEffectAuthority {
        authority_family: family.to_owned(),
        admitted_effect_class,
        unknown_outcome_retry_ceiling: unknown_outcome_retry_ceiling(
            header,
            provider,
            unknown_outcome_policy,
        ),
    };
    authority.validate()?;
    Ok(authority)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ReturnNudge {
    Flag(bool),
    Message(String),
}

impl Default for ReturnNudge {
    fn default() -> Self {
        Self::Flag(false)
    }
}

impl ReturnNudge {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Flag(enabled) => *enabled,
            Self::Message(_) => true,
        }
    }

    pub fn message(&self, declared_outputs: &[String]) -> String {
        if let Self::Message(text) = self
            && !text.trim().is_empty()
        {
            return text.clone();
        }
        format!(
            "This directive declares structured outputs ({}) that have not been \
             emitted. Call the `directive_return` tool now with every declared \
             output. This is the final turn.",
            declared_outputs.join(", ")
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSpec {
    #[serde(default)]
    pub turns: Option<u32>,
    #[serde(default)]
    pub tool_calls: Option<u32>,
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub provider_request_body_bytes: Option<u64>,
    #[serde(default)]
    pub spend_usd: Option<ryeos_accounting::UsdNanos>,
    #[serde(default)]
    pub spawns: Option<u32>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub duration_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ContinuationConfig {
    Flag(bool),
    Enabled(ContinuationEnabled),
}

impl Default for ContinuationConfig {
    fn default() -> Self {
        Self::Flag(false)
    }
}

impl ContinuationConfig {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Flag(enabled) => *enabled,
            Self::Enabled(_) => true,
        }
    }

    pub fn declared_carry_turns(&self) -> Option<u32> {
        match self {
            Self::Enabled(config) => config.carry_turns,
            Self::Flag(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContinuationEnabled {
    #[serde(default)]
    pub carry_turns: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parser_projects_engine_owned_fields_and_keeps_strict_owned_shapes() {
        let parsed = parse_effective_header(&json!({
            "body": "engine-owned",
            "required_secrets": ["TOKEN"],
            "return_nudge": true,
            "outputs": [{"name": "answer"}],
        }))
        .unwrap();
        assert!(parsed.return_nudge.enabled());
        assert_eq!(parsed.outputs.unwrap()[0].name, "answer");

        let error = parse_effective_header(&json!({
            "outputs": [{"name": "answer", "unknown": true}],
        }))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));

        let error = parse_effective_header(&json!({"effects": "durable"})).unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn effect_authority_uses_the_parsed_class_and_provider_ceiling() {
        let header = parse_effective_header(&json!({"effects": "recorded"})).unwrap();
        assert!(header.effects.records_provider_calls());
        assert_eq!(
            header.effects.admitted(),
            Some(ryeos_effect_contract::EffectClass::Recorded)
        );

        let provider: crate::ProviderConfig = serde_json::from_value(json!({
            "family": "chat_completions",
            "transport": {
                "kind": "admitted_local_worker",
                "execute": "worker:fixtures/local-recorded",
                "effect_class_ceiling": "recorded"
            }
        }))
        .unwrap();
        let authority = resolve_external_effect_authority(&header, &provider, None).unwrap();
        assert_eq!(
            authority.admitted_effect_class,
            Some(ryeos_effect_contract::EffectClass::Recorded)
        );

        let sealed = parse_effective_header(&json!({"effects": "sealed"})).unwrap();
        let error = resolve_external_effect_authority(&sealed, &provider, None).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn return_nudge_parses_flag_and_custom_message_forms() {
        let off = parse_effective_header(&json!({"name": "t"})).unwrap();
        assert!(!off.return_nudge.enabled());

        let flag = parse_effective_header(&json!({"return_nudge": true})).unwrap();
        let outputs = vec!["model_body".to_owned(), "notes".to_owned()];
        assert!(
            flag.return_nudge
                .message(&outputs)
                .contains("model_body, notes")
        );

        let custom = parse_effective_header(&json!({
            "return_nudge": "Call directive_return with the model.",
        }))
        .unwrap();
        assert_eq!(
            custom.return_nudge.message(&outputs),
            "Call directive_return with the model."
        );

        let blank = parse_effective_header(&json!({"return_nudge": "  "})).unwrap();
        assert!(blank.return_nudge.enabled());
        assert!(
            blank
                .return_nudge
                .message(&outputs)
                .contains("directive_return")
        );
    }

    #[test]
    fn hooks_use_the_typed_scalar_condition_grammar() {
        let parsed = parse_effective_header(&json!({
            "hooks": [{
                "id": "selected",
                "event": "after_step",
                "result": "control",
                "condition": "turn >= 2",
                "action": {"item_id": "tool:test/hook"},
            }],
        }))
        .unwrap();
        assert_eq!(parsed.hooks.unwrap().len(), 1);

        let error = parse_effective_header(&json!({
            "hooks": [{
                "id": "noncanonical",
                "event": "after_step",
                "result": "control",
                "condition": {"path": "turn", "op": "gte", "value": 2},
                "action": {"item_id": "tool:test/hook"},
            }],
        }))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("structured path/op/value conditions are not supported")
        );

        let disabled = parse_effective_header(&json!({
            "continuation": false,
            "hooks": [{
                "id": "never",
                "event": "continuation",
                "result": "control",
                "action": {"item_id": "tool:test/hook"},
            }],
        }))
        .unwrap_err();
        assert!(disabled.to_string().contains("continuation is disabled"));
    }

    fn provider_with(transport: serde_json::Value) -> crate::ProviderConfig {
        serde_json::from_value(json!({
            "family": "chat_completions",
            "transport": transport,
        }))
        .unwrap()
    }

    #[test]
    fn provider_contact_parses_and_defaults_to_once() {
        assert_eq!(
            parse_effective_header(&json!({})).unwrap().provider_contact,
            ProviderContact::Once
        );
        assert_eq!(
            parse_effective_header(&json!({"provider_contact": "repeatable"}))
                .unwrap()
                .provider_contact,
            ProviderContact::Repeatable
        );
        let error = parse_effective_header(&json!({"provider_contact": "twice"})).unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn unknown_outcome_ceiling_truth_table() {
        let remote =
            provider_with(json!({"kind": "remote_http", "base_url": "https://example.test"}));
        let local = provider_with(json!({
            "kind": "admitted_local_worker",
            "execute": "worker:fixtures/local",
            "effect_class_ceiling": "recorded"
        }));
        let policy = UnknownOutcomeRetryPolicy {
            max_retries: 2,
            on: vec![UnknownOutcomeClass::MidStream],
        };
        let once = parse_effective_header(&json!({})).unwrap();
        let repeatable =
            parse_effective_header(&json!({"provider_contact": "repeatable"})).unwrap();

        assert_eq!(
            unknown_outcome_retry_ceiling(&once, &remote, Some(&policy)),
            0
        );
        assert_eq!(
            unknown_outcome_retry_ceiling(&repeatable, &remote, Some(&policy)),
            2
        );
        assert_eq!(unknown_outcome_retry_ceiling(&repeatable, &remote, None), 0);
        assert_eq!(
            unknown_outcome_retry_ceiling(&repeatable, &local, Some(&policy)),
            0
        );
        let authority =
            resolve_external_effect_authority(&repeatable, &remote, Some(&policy)).unwrap();
        assert_eq!(authority.unknown_outcome_retry_ceiling, 2);
        let authority = resolve_external_effect_authority(&once, &remote, Some(&policy)).unwrap();
        assert_eq!(authority.unknown_outcome_retry_ceiling, 0);
    }

    #[test]
    fn unknown_outcome_policy_validation() {
        let empty = UnknownOutcomeRetryPolicy {
            max_retries: 1,
            on: vec![],
        };
        assert!(empty.validate().is_err());
        let duplicate = UnknownOutcomeRetryPolicy {
            max_retries: 1,
            on: vec![UnknownOutcomeClass::Send, UnknownOutcomeClass::Send],
        };
        assert!(duplicate.validate().is_err());
        let ok = UnknownOutcomeRetryPolicy {
            max_retries: 1,
            on: vec![UnknownOutcomeClass::Timeout, UnknownOutcomeClass::MidStream],
        };
        ok.validate().unwrap();
        assert!(
            serde_json::from_value::<UnknownOutcomeRetryPolicy>(
                json!({"max_retries": 1, "on": ["nope"]})
            )
            .is_err()
        );
    }
}
