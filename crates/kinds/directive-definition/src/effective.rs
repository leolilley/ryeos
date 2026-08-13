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

/// Top-level composed fields that form the execution-facing directive header.
pub const DIRECTIVE_EFFECTIVE_HEADER_KEYS: &[&str] = &[
    "name",
    "extends",
    "effects",
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
                "execute": "worker:local-inference/local-tinygrad",
                "effect_class_ceiling": "recorded"
            }
        }))
        .unwrap();
        let authority = resolve_external_effect_authority(&header, &provider).unwrap();
        assert_eq!(
            authority.admitted_effect_class,
            Some(ryeos_effect_contract::EffectClass::Recorded)
        );

        let sealed = parse_effective_header(&json!({"effects": "sealed"})).unwrap();
        let error = resolve_external_effect_authority(&sealed, &provider).unwrap_err();
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
}
