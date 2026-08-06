//! Engine-owned admitted hook policy.
//!
//! These are the only wire types accepted by launch admission, runtimes, and
//! callback authorization. Runtimes compile this captured plan; they never
//! rebuild it from source files.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::runtime_registry::{ConfigMergeMode, LaunchConfigInputDecl, LaunchItemSpace};
use ryeos_handler_protocol::{ItemSpaceWire, LaunchConfigSnapshotWire, TrustClassWire};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;

pub const EFFECTIVE_HOOK_PLAN_SCHEMA: &str = "ryeos.hooks.effective.v1";
pub const EFFECTIVE_HOOK_PLAN_DERIVED_KEY: &str = "effective_hook_plan";
pub const HOOK_CONTEXT_SCHEMA: &str = "ryeos.hooks.context.v1";
pub const MAX_EFFECTIVE_HOOKS: usize = 256;
pub const MAX_HOOK_DISPATCH_CAPS: usize = 256;
pub const MAX_HOOK_PLAN_CANONICAL_BYTES: usize = 256 * 1024;
pub const MAX_HOOK_ID_BYTES: usize = 256;
pub const MAX_HOOK_EVENT_BYTES: usize = 128;
pub const HOOK_SOURCE_SCHEMA: &str = "ryeos.hooks.source.v1";
pub const HOOK_SOURCE_CATEGORY: &str = "ryeos-runtime/hooks";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ExpressionCondition {
    #[default]
    Absent,
    Boolean(bool),
    Expression(String),
}

impl ExpressionCondition {
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub fn as_expression(&self) -> Option<&str> {
        match self {
            Self::Expression(source) => Some(source),
            Self::Absent | Self::Boolean(_) => None,
        }
    }
}

impl Serialize for ExpressionCondition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Absent => serializer.serialize_none(),
            Self::Boolean(value) => serializer.serialize_bool(*value),
            Self::Expression(source) => serializer.serialize_str(source),
        }
    }
}

impl<'de> Deserialize<'de> for ExpressionCondition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = ExpressionCondition;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a boolean or non-empty rye-expr/1 expression")
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(ExpressionCondition::Boolean(value))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                self.visit_string(value.to_owned())
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                if value.trim().is_empty() {
                    return Err(E::custom("condition expression must not be empty"));
                }
                Ok(ExpressionCondition::Expression(value))
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Err(E::custom(
                    "condition cannot be null; omit it for an unconditional hook",
                ))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                self.visit_none()
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Err(E::invalid_type(de::Unexpected::Signed(value), &self))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Err(E::invalid_type(de::Unexpected::Unsigned(value), &self))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Err(E::invalid_type(de::Unexpected::Float(value), &self))
            }

            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                _sequence: A,
            ) -> Result<Self::Value, A::Error> {
                Err(de::Error::custom(
                    "condition arrays are not valid rye-expr/1 conditions",
                ))
            }

            fn visit_map<A: de::MapAccess<'de>>(self, _map: A) -> Result<Self::Value, A::Error> {
                Err(de::Error::custom(
                    "structured path/op/value conditions are not supported; write one rye-expr/1 expression string",
                ))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDefinition {
    pub id: String,
    pub event: String,
    pub result: HookResultMode,
    #[serde(default, skip_serializing_if = "ExpressionCondition::is_absent")]
    pub condition: ExpressionCondition,
    pub action: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookResultMode {
    Discard,
    Control,
    Observation,
}

impl HookResultMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discard => "discard",
            Self::Control => "control",
            Self::Observation => "observation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum HookLayer {
    Authored = 1,
    Builtin = 2,
    Infrastructure = 3,
    Context = 4,
    Operator = 5,
    Project = 6,
}

impl HookLayer {
    pub const ALL: [Self; 6] = [
        Self::Authored,
        Self::Builtin,
        Self::Infrastructure,
        Self::Context,
        Self::Operator,
        Self::Project,
    ];

    pub const fn precedence(self) -> u8 {
        self as u8
    }

    pub const fn is_observer_only(self) -> bool {
        matches!(self, Self::Infrastructure)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authored => "authored",
            Self::Builtin => "builtin",
            Self::Infrastructure => "infrastructure",
            Self::Context => "context",
            Self::Operator => "operator",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookContextContract {
    pub schema: String,
    pub allowed_roots: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEventContract {
    pub context_contract: HookContextContract,
    pub allowed_results: BTreeSet<HookResultMode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveHookLayer {
    pub hooks: Vec<HookDefinition>,
    pub dispatch_caps: Vec<String>,
}

impl EffectiveHookLayer {
    pub fn empty() -> Self {
        Self {
            hooks: Vec::new(),
            dispatch_caps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSourceEvidence {
    pub layer: HookLayer,
    pub canonical_ref: String,
    pub source_space: crate::contracts::ItemSpace,
    pub trust_class: crate::resolution::TrustClass,
    pub signer_fingerprint: String,
    pub source_raw_content_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveHookPlan {
    pub schema: String,
    pub owner_kind: String,
    pub event_contracts: BTreeMap<String, HookEventContract>,
    pub authored: EffectiveHookLayer,
    pub builtin: EffectiveHookLayer,
    pub infrastructure: EffectiveHookLayer,
    pub context: EffectiveHookLayer,
    pub operator: EffectiveHookLayer,
    pub project: EffectiveHookLayer,
    pub sources: Vec<HookSourceEvidence>,
}

impl EffectiveHookPlan {
    pub fn from_value(value: &Value) -> Result<Self, HookPlanError> {
        let plan: Self = serde_json::from_value(value.clone())
            .map_err(|error| HookPlanError(format!("decode effective hook plan: {error}")))?;
        plan.validate()?;
        Ok(plan)
    }

    pub fn to_value(&self) -> Result<Value, HookPlanError> {
        self.validate()?;
        serde_json::to_value(self)
            .map_err(|error| HookPlanError(format!("encode effective hook plan: {error}")))
    }

    pub fn layer(&self, layer: HookLayer) -> &EffectiveHookLayer {
        match layer {
            HookLayer::Authored => &self.authored,
            HookLayer::Builtin => &self.builtin,
            HookLayer::Infrastructure => &self.infrastructure,
            HookLayer::Context => &self.context,
            HookLayer::Operator => &self.operator,
            HookLayer::Project => &self.project,
        }
    }

    pub fn iter_layers(&self) -> impl Iterator<Item = (HookLayer, &EffectiveHookLayer)> {
        HookLayer::ALL
            .into_iter()
            .map(|layer| (layer, self.layer(layer)))
    }

    pub fn validate(&self) -> Result<(), HookPlanError> {
        if self.schema != EFFECTIVE_HOOK_PLAN_SCHEMA {
            return Err(HookPlanError(format!(
                "unsupported hook plan schema `{}`",
                self.schema
            )));
        }
        if self.owner_kind.is_empty() || self.owner_kind.len() > MAX_HOOK_EVENT_BYTES {
            return Err(HookPlanError(
                "hook plan owner_kind is empty or exceeds its bound".to_string(),
            ));
        }
        if self.event_contracts.is_empty() {
            return Err(HookPlanError(
                "hook-capable kind has no event contracts".to_string(),
            ));
        }
        for (event, contract) in &self.event_contracts {
            if event.is_empty()
                || event.len() > MAX_HOOK_EVENT_BYTES
                || contract.context_contract.schema != HOOK_CONTEXT_SCHEMA
            {
                return Err(HookPlanError(format!(
                    "hook event `{event}` has an invalid context contract"
                )));
            }
            if contract.context_contract.allowed_roots.is_empty()
                || contract
                    .context_contract
                    .allowed_roots
                    .iter()
                    .any(String::is_empty)
                || contract.allowed_results.is_empty()
            {
                return Err(HookPlanError(format!(
                    "hook event `{event}` has an empty context/result contract"
                )));
            }
        }

        let mut ids = HashSet::new();
        let mut hook_count = 0usize;
        for (layer, body) in self.iter_layers() {
            if body.dispatch_caps.len() > MAX_HOOK_DISPATCH_CAPS {
                return Err(HookPlanError(format!(
                    "{} layer exceeds dispatch capability bound",
                    layer.as_str()
                )));
            }
            let mut caps = HashSet::new();
            for capability in &body.dispatch_caps {
                if capability.is_empty() || !caps.insert(capability) {
                    return Err(HookPlanError(format!(
                        "{} layer has an empty or duplicate dispatch capability",
                        layer.as_str()
                    )));
                }
            }
            for hook in &body.hooks {
                hook_count += 1;
                if hook.id.is_empty()
                    || hook.id.len() > MAX_HOOK_ID_BYTES
                    || !ids.insert(hook.id.as_str())
                {
                    return Err(HookPlanError(format!(
                        "hook id `{}` is empty or duplicated across layers",
                        hook.id
                    )));
                }
                let contract = self.event_contracts.get(&hook.event).ok_or_else(|| {
                    HookPlanError(format!(
                        "{} hook `{}` targets undeclared event `{}`",
                        layer.as_str(),
                        hook.id,
                        hook.event
                    ))
                })?;
                if !contract.allowed_results.contains(&hook.result) {
                    return Err(HookPlanError(format!(
                        "{} hook `{}` result `{}` is not admitted by event `{}`",
                        layer.as_str(),
                        hook.id,
                        hook.result.as_str(),
                        hook.event
                    )));
                }
                validate_layer_result(layer, hook.result, &hook.id)?;
            }
        }
        if hook_count > MAX_EFFECTIVE_HOOKS {
            return Err(HookPlanError(format!(
                "effective hook plan has {hook_count} hooks; maximum is {MAX_EFFECTIVE_HOOKS}"
            )));
        }
        let mut source_layers = HashSet::new();
        for source in &self.sources {
            if source.layer == HookLayer::Authored || !source_layers.insert(source.layer) {
                return Err(HookPlanError(format!(
                    "hook source evidence has an invalid or duplicate {} layer",
                    source.layer.as_str()
                )));
            }
            let canonical = crate::canonical_ref::CanonicalRef::parse(&source.canonical_ref)
                .map_err(|error| HookPlanError(format!("invalid hook source ref: {error}")))?;
            if canonical.kind != "config" {
                return Err(HookPlanError(format!(
                    "hook source `{}` is not a config item",
                    source.canonical_ref
                )));
            }
            if !is_canonical_sha256(&source.signer_fingerprint)
                || !is_canonical_sha256(&source.source_raw_content_digest)
            {
                return Err(HookPlanError(format!(
                    "hook source `{}` has invalid signer/digest evidence",
                    source.canonical_ref
                )));
            }
            let authority_matches = match source.layer {
                HookLayer::Builtin | HookLayer::Infrastructure | HookLayer::Context => {
                    source.source_space == crate::contracts::ItemSpace::Bundle
                        && source.trust_class == crate::resolution::TrustClass::TrustedBundle
                }
                HookLayer::Operator => {
                    source.source_space == crate::contracts::ItemSpace::Node
                        && source.trust_class == crate::resolution::TrustClass::TrustedNode
                }
                HookLayer::Project => {
                    source.source_space == crate::contracts::ItemSpace::Project
                        && source.trust_class == crate::resolution::TrustClass::TrustedProject
                }
                HookLayer::Authored => false,
            };
            if !authority_matches {
                return Err(HookPlanError(format!(
                    "hook source `{}` authority does not match the {} layer",
                    source.canonical_ref,
                    source.layer.as_str()
                )));
            }
        }
        for (layer, body) in self
            .iter_layers()
            .filter(|(layer, _)| *layer != HookLayer::Authored)
        {
            let populated = !body.hooks.is_empty() || !body.dispatch_caps.is_empty();
            if populated && !source_layers.contains(&layer) {
                return Err(HookPlanError(format!(
                    "{} hook layer and source evidence do not correspond",
                    layer.as_str()
                )));
            }
        }
        let canonical = lillux::cas::canonical_json(
            &serde_json::to_value(self)
                .map_err(|error| HookPlanError(format!("encode hook plan: {error}")))?,
        )
        .map_err(|error| HookPlanError(format!("canonicalize hook plan: {error}")))?;
        if canonical.len() > MAX_HOOK_PLAN_CANONICAL_BYTES {
            return Err(HookPlanError(format!(
                "effective hook plan is {} bytes; maximum is {}",
                canonical.len(),
                MAX_HOOK_PLAN_CANONICAL_BYTES
            )));
        }
        Ok(())
    }
}

fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Exact optional configured-source inputs captured for every hook-capable
/// launch. Space/trust are source-owned and cannot be borrowed from the item
/// being launched.
pub fn hook_source_declarations() -> BTreeMap<String, LaunchConfigInputDecl> {
    BTreeMap::from([
        (
            "base".to_string(),
            LaunchConfigInputDecl::Item {
                id: "ryeos-runtime/hooks/base".to_string(),
                required: false,
                merge: ConfigMergeMode::FirstMatch,
                allowed_spaces: vec![LaunchItemSpace::Bundle],
                allowed_trust: vec![crate::resolution::TrustClass::TrustedBundle],
            },
        ),
        (
            "operator".to_string(),
            LaunchConfigInputDecl::Item {
                id: "ryeos-runtime/hooks/operator".to_string(),
                required: false,
                merge: ConfigMergeMode::FirstMatch,
                allowed_spaces: vec![LaunchItemSpace::Node],
                allowed_trust: vec![crate::resolution::TrustClass::TrustedNode],
            },
        ),
        (
            "project".to_string(),
            LaunchConfigInputDecl::Item {
                id: "ryeos-runtime/hooks/project".to_string(),
                required: false,
                merge: ConfigMergeMode::FirstMatch,
                allowed_spaces: vec![LaunchItemSpace::Project],
                allowed_trust: vec![crate::resolution::TrustClass::TrustedProject],
            },
        ),
    ])
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSourceLayerWire {
    requires: HookSourceRequiresWire,
    hooks: Vec<ConfiguredHookDefinitionWire>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredHookDefinitionWire {
    id: String,
    target: ConfiguredHookTargetWire,
    result: HookResultMode,
    #[serde(default)]
    condition: ExpressionCondition,
    action: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredHookTargetWire {
    kind: String,
    event: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSourceRequiresWire {
    capabilities: HookSourceCapabilitiesWire,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSourceCapabilitiesWire {
    declared: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaseHookSourceWire {
    category: String,
    schema: String,
    builtin: HookSourceLayerWire,
    infrastructure: HookSourceLayerWire,
    context: HookSourceLayerWire,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperatorHookSourceWire {
    category: String,
    schema: String,
    operator: HookSourceLayerWire,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectHookSourceWire {
    category: String,
    schema: String,
    project: HookSourceLayerWire,
}

/// Build and strictly validate one complete captured plan from the signed kind
/// declaration, composed authored list, and provenance-bearing config
/// snapshots.
pub fn capture_effective_hook_plan(
    owner_kind: &str,
    event_contracts: BTreeMap<String, HookEventContract>,
    known_event_contracts: &BTreeMap<String, BTreeMap<String, HookEventContract>>,
    authored_value: Option<&Value>,
    authored_dispatch_caps: Vec<String>,
    snapshots: &BTreeMap<String, LaunchConfigSnapshotWire>,
) -> Result<EffectiveHookPlan, HookPlanError> {
    let authored_hooks = match authored_value {
        None => Vec::new(),
        Some(Value::Null) => {
            return Err(HookPlanError(
                "authored hook path is explicit null; omit it or declare a list".to_string(),
            ));
        }
        Some(value) => serde_json::from_value::<Vec<HookDefinition>>(value.clone())
            .map_err(|error| HookPlanError(format!("decode composed authored hooks: {error}")))?,
    };
    let mut plan = EffectiveHookPlan {
        schema: EFFECTIVE_HOOK_PLAN_SCHEMA.to_string(),
        owner_kind: owner_kind.to_string(),
        event_contracts,
        authored: EffectiveHookLayer {
            hooks: authored_hooks,
            dispatch_caps: authored_dispatch_caps,
        },
        builtin: EffectiveHookLayer::empty(),
        infrastructure: EffectiveHookLayer::empty(),
        context: EffectiveHookLayer::empty(),
        operator: EffectiveHookLayer::empty(),
        project: EffectiveHookLayer::empty(),
        sources: Vec::new(),
    };

    if let Some((value, contributor)) = present_item_snapshot(snapshots, "base")? {
        let source: BaseHookSourceWire = serde_json::from_value(value.clone())
            .map_err(|error| HookPlanError(format!("decode base hook source: {error}")))?;
        validate_source_header(&source.category, &source.schema, "base")?;
        plan.builtin = layer_from_wire(
            owner_kind,
            HookLayer::Builtin,
            source.builtin,
            known_event_contracts,
        )?;
        plan.infrastructure = layer_from_wire(
            owner_kind,
            HookLayer::Infrastructure,
            source.infrastructure,
            known_event_contracts,
        )?;
        plan.context = layer_from_wire(
            owner_kind,
            HookLayer::Context,
            source.context,
            known_event_contracts,
        )?;
        for layer in [
            HookLayer::Builtin,
            HookLayer::Infrastructure,
            HookLayer::Context,
        ] {
            plan.sources.push(source_evidence(layer, contributor)?);
        }
    }
    if let Some((value, contributor)) = present_item_snapshot(snapshots, "operator")? {
        let source: OperatorHookSourceWire = serde_json::from_value(value.clone())
            .map_err(|error| HookPlanError(format!("decode operator hook source: {error}")))?;
        validate_source_header(&source.category, &source.schema, "operator")?;
        plan.operator = layer_from_wire(
            owner_kind,
            HookLayer::Operator,
            source.operator,
            known_event_contracts,
        )?;
        plan.sources
            .push(source_evidence(HookLayer::Operator, contributor)?);
    }
    if let Some((value, contributor)) = present_item_snapshot(snapshots, "project")? {
        let source: ProjectHookSourceWire = serde_json::from_value(value.clone())
            .map_err(|error| HookPlanError(format!("decode project hook source: {error}")))?;
        validate_source_header(&source.category, &source.schema, "project")?;
        plan.project = layer_from_wire(
            owner_kind,
            HookLayer::Project,
            source.project,
            known_event_contracts,
        )?;
        plan.sources
            .push(source_evidence(HookLayer::Project, contributor)?);
    }
    plan.validate()?;
    Ok(plan)
}

fn layer_from_wire(
    owner_kind: &str,
    layer: HookLayer,
    value: HookSourceLayerWire,
    known_event_contracts: &BTreeMap<String, BTreeMap<String, HookEventContract>>,
) -> Result<EffectiveHookLayer, HookPlanError> {
    let caps = value.requires.capabilities.declared;
    if caps.iter().any(String::is_empty) {
        return Err(HookPlanError(
            "configured hook source has an empty declared capability".to_string(),
        ));
    }
    let hooks = value
        .hooks
        .into_iter()
        .filter_map(|hook| {
            let Some(events) = known_event_contracts.get(&hook.target.kind) else {
                return Some(Err(HookPlanError(format!(
                    "configured hook `{}` targets unknown hook-capable kind `{}`",
                    hook.id, hook.target.kind
                ))));
            };
            let Some(contract) = events.get(&hook.target.event) else {
                return Some(Err(HookPlanError(format!(
                    "configured hook `{}` targets unknown event `{}/{}`",
                    hook.id, hook.target.kind, hook.target.event
                ))));
            };
            if !contract.allowed_results.contains(&hook.result) {
                return Some(Err(HookPlanError(format!(
                    "configured hook `{}` result `{}` is not admitted by event `{}/{}`",
                    hook.id,
                    hook.result.as_str(),
                    hook.target.kind,
                    hook.target.event
                ))));
            }
            if let Err(error) = validate_layer_result(layer, hook.result, &hook.id) {
                return Some(Err(error));
            }
            (hook.target.kind == owner_kind).then_some(Ok(HookDefinition {
                id: hook.id,
                event: hook.target.event,
                result: hook.result,
                condition: hook.condition,
                action: hook.action,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(EffectiveHookLayer {
        hooks,
        dispatch_caps: caps,
    })
}

fn present_item_snapshot<'a>(
    snapshots: &'a BTreeMap<String, LaunchConfigSnapshotWire>,
    name: &str,
) -> Result<
    Option<(
        &'a Value,
        &'a ryeos_handler_protocol::LaunchConfigContributorWire,
    )>,
    HookPlanError,
> {
    let snapshot = snapshots.get(name).ok_or_else(|| {
        HookPlanError(format!("hook capture omitted `{name}` dependency snapshot"))
    })?;
    match snapshot {
        LaunchConfigSnapshotWire::Item {
            present: false,
            value: None,
            value_digest: None,
            contributors,
        } if contributors.is_empty() => Ok(None),
        LaunchConfigSnapshotWire::Item {
            present: true,
            value: Some(value),
            value_digest: Some(_),
            contributors,
        } if contributors.len() == 1 => Ok(Some((value, &contributors[0]))),
        _ => Err(HookPlanError(format!(
            "hook source snapshot `{name}` has an invalid item shape"
        ))),
    }
}

fn validate_source_header(category: &str, schema: &str, name: &str) -> Result<(), HookPlanError> {
    if category != HOOK_SOURCE_CATEGORY || schema != HOOK_SOURCE_SCHEMA {
        return Err(HookPlanError(format!(
            "hook source `{name}` must declare category `{HOOK_SOURCE_CATEGORY}` and schema `{HOOK_SOURCE_SCHEMA}`"
        )));
    }
    Ok(())
}

fn source_evidence(
    layer: HookLayer,
    contributor: &ryeos_handler_protocol::LaunchConfigContributorWire,
) -> Result<HookSourceEvidence, HookPlanError> {
    let source_space = match contributor.space {
        ItemSpaceWire::Bundle => crate::contracts::ItemSpace::Bundle,
        ItemSpaceWire::Project => crate::contracts::ItemSpace::Project,
        ItemSpaceWire::Node => crate::contracts::ItemSpace::Node,
    };
    let trust_class = match contributor.trust_class {
        TrustClassWire::TrustedBundle => crate::resolution::TrustClass::TrustedBundle,
        TrustClassWire::TrustedProject => crate::resolution::TrustClass::TrustedProject,
        TrustClassWire::TrustedNode => crate::resolution::TrustClass::TrustedNode,
        TrustClassWire::UntrustedProject => crate::resolution::TrustClass::UntrustedProject,
        TrustClassWire::Unsigned => crate::resolution::TrustClass::Unsigned,
    };
    Ok(HookSourceEvidence {
        layer,
        canonical_ref: format!("config:{}", contributor.canonical_id),
        source_space,
        trust_class,
        signer_fingerprint: contributor.signer_fingerprint.clone(),
        source_raw_content_digest: contributor.content_digest.clone(),
    })
}

fn validate_layer_result(
    layer: HookLayer,
    result: HookResultMode,
    hook_id: &str,
) -> Result<(), HookPlanError> {
    if layer.is_observer_only() && result == HookResultMode::Control {
        return Err(HookPlanError(format!(
            "infrastructure hook `{hook_id}` cannot declare control"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookPlanError(pub String);

impl std::fmt::Display for HookPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for HookPlanError {}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_handler_protocol::{LaunchConfigContributorWire, LaunchConfigSnapshotWire};

    fn event_contract(results: &[HookResultMode]) -> HookEventContract {
        HookEventContract {
            context_contract: HookContextContract {
                schema: HOOK_CONTEXT_SCHEMA.to_string(),
                allowed_roots: BTreeSet::from(["event".to_string()]),
            },
            allowed_results: results.iter().copied().collect(),
        }
    }

    fn known_events() -> BTreeMap<String, BTreeMap<String, HookEventContract>> {
        BTreeMap::from([
            (
                "graph".to_string(),
                BTreeMap::from([(
                    "graph_completed".to_string(),
                    event_contract(&[HookResultMode::Discard, HookResultMode::Observation]),
                )]),
            ),
            (
                "directive".to_string(),
                BTreeMap::from([(
                    "after_step".to_string(),
                    event_contract(&[
                        HookResultMode::Discard,
                        HookResultMode::Observation,
                        HookResultMode::Control,
                    ]),
                )]),
            ),
        ])
    }

    fn absent_snapshots() -> BTreeMap<String, LaunchConfigSnapshotWire> {
        ["base", "operator", "project"]
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    LaunchConfigSnapshotWire::Item {
                        present: false,
                        value: None,
                        value_digest: None,
                        contributors: Vec::new(),
                    },
                )
            })
            .collect()
    }

    fn operator_snapshot(value: Value) -> LaunchConfigSnapshotWire {
        LaunchConfigSnapshotWire::Item {
            present: true,
            value: Some(value),
            value_digest: Some("b".repeat(64)),
            contributors: vec![LaunchConfigContributorWire {
                space: ItemSpaceWire::Node,
                root_label: "node".to_string(),
                canonical_id: "ryeos-runtime/hooks/operator".to_string(),
                content_digest: "c".repeat(64),
                trust_class: TrustClassWire::TrustedNode,
                signer_fingerprint: "d".repeat(64),
            }],
        }
    }

    #[derive(Debug, Deserialize)]
    struct ConditionFixture {
        #[serde(default)]
        condition: ExpressionCondition,
    }

    #[test]
    fn condition_omission_is_distinct_from_explicit_null() {
        let omitted: ConditionFixture = serde_yaml::from_str("{}").unwrap();
        assert_eq!(omitted.condition, ExpressionCondition::Absent);

        let error = serde_yaml::from_str::<ConditionFixture>("condition: null")
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot be null"));
    }

    #[test]
    fn condition_accepts_boolean_and_non_empty_expression() {
        let boolean: ConditionFixture = serde_yaml::from_str("condition: true").unwrap();
        assert_eq!(boolean.condition, ExpressionCondition::Boolean(true));

        let expression: ConditionFixture =
            serde_yaml::from_str("condition: 'state.ready && result.ok'").unwrap();
        assert_eq!(
            expression.condition,
            ExpressionCondition::Expression("state.ready && result.ok".to_string())
        );
    }

    #[test]
    fn condition_rejects_empty_and_structured_non_contract_forms() {
        assert!(serde_yaml::from_str::<ConditionFixture>("condition: '   '").is_err());
        let error = serde_yaml::from_str::<ConditionFixture>(
            "condition:\n  path: state.ready\n  op: eq\n  value: true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("structured path/op/value"));
    }

    #[test]
    fn configured_source_hooks_are_validated_and_projected_by_target_kind() {
        let wire: HookSourceLayerWire = serde_json::from_value(serde_json::json!({
            "requires": {"capabilities": {"declared": ["ryeos.execute.tool.test/audit"]}},
            "hooks": [
                {
                    "id": "graph-audit",
                    "target": {"kind": "graph", "event": "graph_completed"},
                    "result": "observation",
                    "action": {"item_id": "tool:test/audit"}
                },
                {
                    "id": "directive-control",
                    "target": {"kind": "directive", "event": "after_step"},
                    "result": "control",
                    "action": {"item_id": "tool:test/audit"}
                }
            ]
        }))
        .unwrap();

        let projected =
            layer_from_wire("graph", HookLayer::Operator, wire, &known_events()).unwrap();
        assert_eq!(projected.hooks.len(), 1);
        assert_eq!(projected.hooks[0].id, "graph-audit");
        assert_eq!(projected.hooks[0].event, "graph_completed");
        assert_eq!(
            projected.dispatch_caps,
            vec!["ryeos.execute.tool.test/audit".to_string()]
        );
    }

    #[test]
    fn configured_source_rejects_unknown_target_pairs_before_projection() {
        let unknown_kind: HookSourceLayerWire = serde_json::from_value(serde_json::json!({
            "requires": {"capabilities": {"declared": []}},
            "hooks": [{
                "id": "unknown",
                "target": {"kind": "missing", "event": "done"},
                "result": "discard",
                "action": {"item_id": "tool:test/audit"}
            }]
        }))
        .unwrap();
        assert!(
            layer_from_wire("graph", HookLayer::Operator, unknown_kind, &known_events())
                .unwrap_err()
                .to_string()
                .contains("unknown hook-capable kind")
        );

        let unknown_event: HookSourceLayerWire = serde_json::from_value(serde_json::json!({
            "requires": {"capabilities": {"declared": []}},
            "hooks": [{
                "id": "unknown",
                "target": {"kind": "directive", "event": "missing"},
                "result": "discard",
                "action": {"item_id": "tool:test/audit"}
            }]
        }))
        .unwrap();
        assert!(
            layer_from_wire("graph", HookLayer::Operator, unknown_event, &known_events())
                .unwrap_err()
                .to_string()
                .contains("unknown event")
        );
    }

    #[test]
    fn signed_event_contract_is_authoritative_for_graph_control_hooks() {
        let mut events = known_events();
        events.get_mut("graph").unwrap().insert(
            "graph_controlled".to_string(),
            event_contract(&[
                HookResultMode::Discard,
                HookResultMode::Observation,
                HookResultMode::Control,
            ]),
        );
        let authored = serde_json::json!([{
            "id": "admit-control",
            "event": "graph_controlled",
            "result": "control",
            "action": {"item_id": "tool:test/control"}
        }]);

        let plan = capture_effective_hook_plan(
            "graph",
            events.get("graph").unwrap().clone(),
            &events,
            Some(&authored),
            vec!["ryeos.execute.tool.test/control".to_string()],
            &absent_snapshots(),
        )
        .unwrap();
        assert_eq!(plan.authored.hooks[0].result, HookResultMode::Control);

        events.get_mut("graph").unwrap().insert(
            "graph_controlled".to_string(),
            event_contract(&[HookResultMode::Discard, HookResultMode::Observation]),
        );
        let error = capture_effective_hook_plan(
            "graph",
            events.get("graph").unwrap().clone(),
            &events,
            Some(&authored),
            vec!["ryeos.execute.tool.test/control".to_string()],
            &absent_snapshots(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("result `control` is not admitted by event `graph_controlled`")
        );
    }

    #[test]
    fn capture_requires_explicit_snapshots_and_distinguishes_absent_from_null() {
        let events = known_events();
        let graph_events = events.get("graph").unwrap().clone();
        let plan = capture_effective_hook_plan(
            "graph",
            graph_events.clone(),
            &events,
            None,
            Vec::new(),
            &absent_snapshots(),
        )
        .unwrap();
        assert!(plan.sources.is_empty());
        assert!(plan.iter_layers().all(|(_, layer)| layer.hooks.is_empty()));

        let mut missing = absent_snapshots();
        missing.remove("project");
        let error = capture_effective_hook_plan(
            "graph",
            graph_events.clone(),
            &events,
            None,
            Vec::new(),
            &missing,
        )
        .unwrap_err();
        assert!(error.to_string().contains("omitted `project` dependency"));

        let error = capture_effective_hook_plan(
            "graph",
            graph_events,
            &events,
            Some(&Value::Null),
            Vec::new(),
            &absent_snapshots(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("explicit null"));
    }

    #[test]
    fn capture_rejects_duplicate_ids_across_authored_and_configured_layers() {
        let events = known_events();
        let mut snapshots = absent_snapshots();
        snapshots.insert(
            "operator".to_string(),
            operator_snapshot(serde_json::json!({
                "category": HOOK_SOURCE_CATEGORY,
                "schema": HOOK_SOURCE_SCHEMA,
                "operator": {
                    "requires": {"capabilities": {"declared": ["ryeos.execute.tool.test/audit"]}},
                    "hooks": [{
                        "id": "audit",
                        "target": {"kind": "graph", "event": "graph_completed"},
                        "result": "observation",
                        "action": {"item_id": "tool:test/audit"}
                    }]
                }
            })),
        );
        let authored = serde_json::json!([{
            "id": "audit",
            "event": "graph_completed",
            "result": "observation",
            "action": {"item_id": "tool:test/audit"}
        }]);
        let error = capture_effective_hook_plan(
            "graph",
            events.get("graph").unwrap().clone(),
            &events,
            Some(&authored),
            vec!["ryeos.execute.tool.test/audit".to_string()],
            &snapshots,
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicated across layers"));
    }
}
