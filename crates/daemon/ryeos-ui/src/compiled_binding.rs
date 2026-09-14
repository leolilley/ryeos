//! Session-bound compilation of signed UI content.
//!
//! Surfaces and views remain the data-driven source of requested UI behavior.
//! This module only compiles their exact effective closure into an enforcement
//! index attenuated by the authenticated caller and current node authority.
//! It does not define product actions, action profiles, or launch modes.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_api::surface_views::{
    EffectiveUiItemIdentity, EmbeddedSurfaceIdentity, EmbeddedViewIdentity,
};

/// Informational summary derived from the compiled grant. It is never an
/// authorization input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveUiPosture {
    ObservationOnly,
    Interactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompiledUiDispatchClass {
    Verified,
    SessionLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompiledUiResultEffect {
    ReplaceSession,
}

/// Exact execution target resolved from a signed source or affordance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledUiTarget {
    pub identity: EffectiveUiItemIdentity,
    pub required_caps: Vec<String>,
    /// Signed source-lane eligibility after authoritative state-access
    /// validation. This is not inferred from endpoint or handler names.
    pub source_safe: bool,
    pub dispatch_class: CompiledUiDispatchClass,
    pub result_effect: Option<CompiledUiResultEffect>,
    /// Closed signed service input contract retained for semantic-slot
    /// validation. The UI compiler never guesses authority-bearing fields
    /// from a target name.
    pub parameter_schema: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledUiSource {
    pub target: CompiledUiTarget,
    pub parameters: Value,
    pub dynamic_parameters: Vec<String>,
    pub activation: ryeos_client_base::ui::content::SourceActivation,
    pub requires_project: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledUiRouteBindings {
    pub input_parameter: String,
    pub thread_target_parameter: Option<String>,
    pub interrupt_intent_parameter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledUiRoute {
    pub target: CompiledUiTarget,
    pub parameters: Value,
    pub bindings: CompiledUiRouteBindings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompiledUiProducer {
    Selection,
    Input,
    Tokens,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "plane", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompiledUiAffordance {
    Local {
        producer: CompiledUiProducer,
        invoke: Value,
    },
    Execution {
        producer: CompiledUiProducer,
        invoke: Value,
        target: CompiledUiTarget,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttenuatedUiBinding {
    pub coordinate: String,
    pub reason: String,
}

/// Canonical session-bound binding body. `binding_digest` is computed over
/// this value and stored beside it by [`SessionCompiledUiBinding`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledUiBinding {
    pub contract_revision: String,
    pub principal_id: String,
    pub project_root: Option<String>,
    pub request_engine_generation_identity: String,
    pub node_policy_generation_digest: String,
    pub surface: EffectiveUiItemIdentity,
    pub views: BTreeMap<String, EmbeddedViewIdentity>,
    pub sources: BTreeMap<String, BTreeMap<String, CompiledUiSource>>,
    pub affordances: BTreeMap<String, BTreeMap<String, CompiledUiAffordance>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface_route: Option<CompiledUiRoute>,
    pub attenuated: Vec<AttenuatedUiBinding>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompiledUiBinding {
    pub binding_digest: String,
    pub posture: EffectiveUiPosture,
    pub binding: CompiledUiBinding,
}

#[derive(Debug, Clone)]
pub struct BindingCompileContext<'a> {
    pub contract_revision: &'a str,
    pub principal_id: &'a str,
    pub caller_scopes: &'a [String],
    pub project_root: Option<&'a str>,
    pub node_policy_generation_digest: &'a str,
    pub identities: EmbeddedSurfaceIdentity,
}

impl SessionCompiledUiBinding {
    pub fn compile(
        context: BindingCompileContext<'_>,
        effective_surface: &Value,
        mut resolve_target: impl FnMut(&str) -> Result<CompiledUiTarget>,
    ) -> Result<Self> {
        let views = effective_surface
            .get("views")
            .and_then(Value::as_object)
            .context("effective surface has no embedded views mapping")?;
        let source_ceiling = capability_lane(effective_surface, "sources")?;
        let affordance_ceiling = capability_lane(effective_surface, "affordances")?;

        let mut sources = BTreeMap::new();
        let mut affordances = BTreeMap::new();
        let mut attenuated = Vec::new();

        // Shell-wide observations are authored by the surface itself. They
        // use the surface's canonical identity as their coordinate namespace,
        // rather than living in a hard-coded browser bootstrap API.
        if let Some(source_map) = effective_surface.get("sources").and_then(Value::as_object) {
            let surface_ref = context.identities.surface.canonical_ref.clone();
            let trusted_surface = is_trusted(&context.identities.surface);
            compile_sources(
                &surface_ref,
                source_map,
                trusted_surface,
                &source_ceiling,
                context.caller_scopes,
                context.project_root.is_some(),
                &mut resolve_target,
                &mut sources,
                &mut attenuated,
                &[],
            )?;
        }

        for (declared_view_ref, identity_state) in &context.identities.views {
            let EmbeddedViewIdentity::Resolved { identity } = identity_state else {
                continue;
            };
            let Some(view) = views.get(declared_view_ref).and_then(Value::as_object) else {
                continue;
            };
            // The coordinate is the key authored by the surface. The resolved
            // canonical ref remains inside `identity`; substituting it here
            // would break aliases and let two declared aliases collide.
            let coordinate_view_ref = declared_view_ref.clone();
            let trusted_view = is_trusted(&context.identities.surface) && is_trusted(identity);
            let dynamic_parameters = view
                .get("input")
                .and_then(|input| input.get("feeds"))
                .map(source_dynamic_parameters)
                .unwrap_or_default();

            if let Some(source_map) = view.get("sources").and_then(Value::as_object) {
                compile_sources(
                    &coordinate_view_ref,
                    source_map,
                    trusted_view,
                    &source_ceiling,
                    context.caller_scopes,
                    context.project_root.is_some(),
                    &mut resolve_target,
                    &mut sources,
                    &mut attenuated,
                    &dynamic_parameters,
                )?;
            }

            if let Some(input) = view.get("input") {
                let input_id = input.get("id").and_then(Value::as_str).unwrap_or("input");
                for (suffix, declaration) in [
                    ("mentions", input.get("mentions")),
                    ("completion", input.get("completion")),
                ] {
                    let Some(declaration) = declaration else {
                        continue;
                    };
                    let Some(item_ref) = declaration.get("ref").and_then(Value::as_str) else {
                        continue;
                    };
                    let channel = format!("input.{input_id}.{suffix}");
                    let coordinate = format!("source:{coordinate_view_ref}:{channel}");
                    match resolve_target(item_ref) {
                        Ok(target)
                            if trusted_view
                                && target.source_safe
                                && lane_allows(&source_ceiling, &target.required_caps)
                                && caller_has_all(context.caller_scopes, &target.required_caps) =>
                        {
                            sources
                                .entry(coordinate_view_ref.clone())
                                .or_default()
                                .insert(
                                    channel,
                                    CompiledUiSource {
                                        target,
                                        parameters: Value::Object(Default::default()),
                                        dynamic_parameters: Vec::new(),
                                        activation: Default::default(),
                                        requires_project: false,
                                    },
                                );
                        }
                        Ok(_) if !trusted_view => attenuated.push(AttenuatedUiBinding {
                            coordinate,
                            reason: "untrusted surface/view authority excludes input sources"
                                .to_string(),
                        }),
                        Ok(_) => attenuated.push(AttenuatedUiBinding {
                            coordinate,
                            reason: "input source is not admitted by the surface and caller grant"
                                .to_string(),
                        }),
                        Err(error) => attenuated.push(AttenuatedUiBinding {
                            coordinate,
                            reason: format!("target resolution failed: {error}"),
                        }),
                    }
                }
            }

            let input_submit = view
                .get("input")
                .and_then(|value| value.get("submit"))
                .and_then(Value::as_str);
            if let Some(items) = view.get("affordances").and_then(Value::as_array) {
                compile_affordances(
                    &coordinate_view_ref,
                    items,
                    input_submit,
                    context.project_root.is_some(),
                    trusted_view,
                    &affordance_ceiling,
                    context.caller_scopes,
                    &mut resolve_target,
                    &mut affordances,
                    &mut attenuated,
                )?;
            }
        }

        // Shell-wide actions are ordinary signed surface affordances. They
        // are compiled through the same target/capability path as view
        // affordances; the daemon does not carry a parallel list of UI modes
        // or privileged endpoint names. A child surface can remove the whole
        // lane with `affordances: []`.
        if let Some(items) = effective_surface
            .get("affordances")
            .and_then(Value::as_array)
        {
            let surface_ref = context.identities.surface.canonical_ref.clone();
            compile_affordances(
                &surface_ref,
                items,
                None,
                context.project_root.is_some(),
                is_trusted(&context.identities.surface),
                &affordance_ceiling,
                context.caller_scopes,
                &mut resolve_target,
                &mut affordances,
                &mut attenuated,
            )?;
        }

        let surface_route = compile_surface_route(
            effective_surface,
            context.project_root.is_some(),
            is_trusted(&context.identities.surface),
            &affordance_ceiling,
            context.caller_scopes,
            &mut resolve_target,
            &mut attenuated,
        );
        let binding = CompiledUiBinding {
            contract_revision: context.contract_revision.to_string(),
            principal_id: context.principal_id.to_string(),
            project_root: context.project_root.map(str::to_string),
            request_engine_generation_identity: context
                .identities
                .request_engine_generation_identity
                .clone(),
            node_policy_generation_digest: context.node_policy_generation_digest.to_string(),
            surface: context.identities.surface,
            views: context.identities.views,
            sources,
            affordances,
            surface_route,
            attenuated,
        };
        let binding_digest = ryeos_state::objects::canonical_value_digest(
            &serde_json::to_value(&binding).context("serialize compiled UI binding")?,
        )?;
        let posture = if binding.surface_route.is_some()
            || binding.affordances.values().any(|view| {
                view.values()
                    .any(|entry| matches!(entry, CompiledUiAffordance::Execution { .. }))
            }) {
            EffectiveUiPosture::Interactive
        } else {
            EffectiveUiPosture::ObservationOnly
        };
        Ok(Self {
            binding_digest,
            posture,
            binding,
        })
    }

    /// Produce the browser presentation from the same compiled authority used
    /// by dispatch. Signed content may describe entries which the caller or
    /// trust lane attenuates; those entries must not remain renderer-local
    /// behavior merely because they do not cross the execution transport.
    pub fn sanitize_effective_surface(&self, effective_surface: &Value) -> Result<Value> {
        let mut surface = effective_surface.clone();
        let surface_fields = surface
            .as_object_mut()
            .context("effective surface presentation is not an object")?;
        sanitize_sources(
            surface_fields,
            self.binding
                .sources
                .get(&self.binding.surface.canonical_ref),
        );
        sanitize_affordances(
            surface_fields,
            self.binding
                .affordances
                .get(&self.binding.surface.canonical_ref),
        );
        if self.binding.surface_route.is_none() {
            if let Some(input) = surface_fields
                .get_mut("input")
                .and_then(Value::as_object_mut)
            {
                input.remove("route");
            }
        }

        if let Some(views) = surface_fields
            .get_mut("views")
            .and_then(Value::as_object_mut)
        {
            for (view_ref, view) in views {
                let Some(fields) = view.as_object_mut() else {
                    continue;
                };
                sanitize_sources(fields, self.binding.sources.get(view_ref));
                sanitize_affordances(fields, self.binding.affordances.get(view_ref));
                let retained = self.binding.affordances.get(view_ref);
                if let Some(input) = fields.get_mut("input").and_then(Value::as_object_mut) {
                    if input
                        .get("submit")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !retained.is_some_and(|items| items.contains_key(id)))
                    {
                        input.remove("submit");
                    }
                }
            }
        }
        Ok(surface)
    }
}

fn sanitize_sources(
    fields: &mut serde_json::Map<String, Value>,
    retained: Option<&BTreeMap<String, CompiledUiSource>>,
) {
    let Some(sources) = fields.get_mut("sources").and_then(Value::as_object_mut) else {
        return;
    };
    sources.retain(|id, _| retained.is_some_and(|items| items.contains_key(id)));
}

fn sanitize_affordances(
    fields: &mut serde_json::Map<String, Value>,
    retained: Option<&BTreeMap<String, CompiledUiAffordance>>,
) {
    let Some(items) = fields.get_mut("affordances").and_then(Value::as_array_mut) else {
        return;
    };
    items.retain(|item| {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return false;
        };
        item.get("invoke").is_none() || retained.is_some_and(|entries| entries.contains_key(id))
    });
}

#[allow(clippy::too_many_arguments)]
fn compile_sources(
    owner_ref: &str,
    source_map: &serde_json::Map<String, Value>,
    trusted_owner: bool,
    source_ceiling: &[String],
    caller_scopes: &[String],
    has_project: bool,
    resolve_target: &mut impl FnMut(&str) -> Result<CompiledUiTarget>,
    out: &mut BTreeMap<String, BTreeMap<String, CompiledUiSource>>,
    attenuated: &mut Vec<AttenuatedUiBinding>,
    primary_dynamic_parameters: &[String],
) -> Result<()> {
    let mut presentation_roles = BTreeSet::new();
    let primary_channel = if source_map.contains_key("default") {
        Some("default")
    } else if source_map.len() == 1 {
        source_map.keys().next().map(String::as_str)
    } else {
        None
    };
    for (channel, source) in source_map {
        let coordinate = format!("source:{owner_ref}:{channel}");
        let authored: ryeos_client_base::ui::content::SourceBinding =
            serde_json::from_value(source.clone()).with_context(|| {
                format!("source {owner_ref}:{channel} violates the closed source contract")
            })?;
        if authored.requires_project && !has_project {
            attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: "source requires a project-bound session".to_string(),
            });
            continue;
        }
        if let Some(role) = authored.role.as_deref()
            && !presentation_roles.insert(role.to_string())
        {
            bail!("UI item {owner_ref} repeats presentation source role `{role}`");
        }
        let item_ref = authored.item_ref.as_str();
        let mut dynamic_parameters = BTreeSet::new();
        for name in &authored.dynamic_parameters {
            if !valid_parameter_name(name) {
                bail!("source {owner_ref}:{channel} has invalid dynamic parameter `{name}`");
            }
            if !dynamic_parameters.insert(name.to_string()) {
                bail!("source {owner_ref}:{channel} repeats dynamic parameter `{name}`");
            }
        }
        if primary_channel == Some(channel.as_str()) {
            dynamic_parameters.extend(primary_dynamic_parameters.iter().cloned());
        }
        match resolve_target(item_ref) {
            Ok(target)
                if trusted_owner
                    && target.source_safe
                    && lane_allows(source_ceiling, &target.required_caps)
                    && caller_has_all(caller_scopes, &target.required_caps) =>
            {
                out.entry(owner_ref.to_string()).or_default().insert(
                    channel.clone(),
                    CompiledUiSource {
                        target,
                        parameters: authored.params,
                        dynamic_parameters: dynamic_parameters.into_iter().collect(),
                        activation: authored.activation,
                        requires_project: authored.requires_project,
                    },
                );
            }
            Ok(_) if !trusted_owner => attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: "untrusted surface/view authority excludes sources".to_string(),
            }),
            Ok(target) if !target.source_safe => attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: "source target is not declared read-only".to_string(),
            }),
            Ok(_) => attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: "caller lacks target capabilities".to_string(),
            }),
            Err(error) => attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: format!("target resolution failed: {error}"),
            }),
        }
    }
    Ok(())
}

fn valid_parameter_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn compile_surface_route(
    surface: &Value,
    has_project: bool,
    trusted_surface: bool,
    affordance_ceiling: &[String],
    caller_scopes: &[String],
    resolve_target: &mut impl FnMut(&str) -> Result<CompiledUiTarget>,
    attenuated: &mut Vec<AttenuatedUiBinding>,
) -> Option<CompiledUiRoute> {
    let route = surface.get("input")?.get("route")?;
    if route
        .get("requires_project")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !has_project
    {
        attenuated.push(AttenuatedUiBinding {
            coordinate: "surface_route".to_string(),
            reason: "surface route requires a project-bound session".to_string(),
        });
        return None;
    }
    let item_ref = route.get("invoke")?.get("ref")?.as_str()?;
    let bindings = match compile_route_bindings(route.get("bindings")) {
        Ok(bindings) => bindings,
        Err(error) => {
            attenuated.push(AttenuatedUiBinding {
                coordinate: "surface_route".to_string(),
                reason: format!("surface route bindings are invalid: {error}"),
            });
            return None;
        }
    };
    let coordinate = "surface_route".to_string();
    if !trusted_surface {
        attenuated.push(AttenuatedUiBinding {
            coordinate,
            reason: "surface authority excludes an executable input route".to_string(),
        });
        return None;
    }
    match resolve_target(item_ref) {
        Ok(target)
            if lane_allows(affordance_ceiling, &target.required_caps)
                && caller_has_all(caller_scopes, &target.required_caps) =>
        {
            let parameters = route
                .get("params")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            if let Err(error) = validate_route_target_contract(&target, &bindings, &parameters) {
                attenuated.push(AttenuatedUiBinding {
                    coordinate,
                    reason: format!("surface route target contract is invalid: {error}"),
                });
                return None;
            }
            Some(CompiledUiRoute {
                target,
                parameters,
                bindings,
            })
        }
        Ok(_) => {
            attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: "surface route is not admitted by the surface and caller grant".to_string(),
            });
            None
        }
        Err(error) => {
            attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: format!("target resolution failed: {error}"),
            });
            None
        }
    }
}

fn validate_route_target_contract(
    target: &CompiledUiTarget,
    bindings: &CompiledUiRouteBindings,
    parameters: &Value,
) -> Result<()> {
    let authored = parameters
        .as_object()
        .context("surface route params are not an object")?;
    let required = [
        (&bindings.input_parameter, "string", "input"),
        (
            bindings
                .thread_target_parameter
                .as_ref()
                .unwrap_or(&bindings.input_parameter),
            "object",
            "thread target",
        ),
        (
            bindings
                .interrupt_intent_parameter
                .as_ref()
                .unwrap_or(&bindings.input_parameter),
            "string?",
            "interrupt intent",
        ),
    ];
    for (name, expected, semantic) in required {
        if semantic != "input" && name == &bindings.input_parameter {
            continue;
        }
        let actual = target
            .parameter_schema
            .get(name)
            .with_context(|| format!("target has no {semantic} parameter `{name}`"))?;
        if actual != expected {
            bail!("target {semantic} parameter `{name}` is `{actual}`, expected `{expected}`");
        }
    }
    if authored.contains_key(&bindings.input_parameter)
        || bindings
            .thread_target_parameter
            .as_ref()
            .is_some_and(|name| authored.contains_key(name))
        || bindings
            .interrupt_intent_parameter
            .as_ref()
            .is_some_and(|name| authored.contains_key(name))
    {
        bail!("authored route params collide with renderer-produced semantic slots");
    }
    Ok(())
}

fn compile_route_bindings(value: Option<&Value>) -> Result<CompiledUiRouteBindings> {
    let fields = value
        .and_then(Value::as_object)
        .context("surface route has no signed bindings mapping")?;
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "input_parameter" | "thread_target_parameter" | "interrupt_intent_parameter"
        )
    }) {
        bail!("surface route bindings contain an unknown field");
    }
    let input_parameter = route_parameter_name(fields, "input_parameter")?
        .context("surface route has no input_parameter binding")?;
    let thread_target_parameter = route_parameter_name(fields, "thread_target_parameter")?;
    let interrupt_intent_parameter = route_parameter_name(fields, "interrupt_intent_parameter")?;
    let names = [
        Some(input_parameter.as_str()),
        thread_target_parameter.as_deref(),
        interrupt_intent_parameter.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<BTreeSet<_>>();
    let declared_count = 1
        + usize::from(thread_target_parameter.is_some())
        + usize::from(interrupt_intent_parameter.is_some());
    if names.len() != declared_count {
        bail!("surface route bindings must name distinct parameters");
    }
    Ok(CompiledUiRouteBindings {
        input_parameter,
        thread_target_parameter,
        interrupt_intent_parameter,
    })
}

fn route_parameter_name(
    fields: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>> {
    let Some(value) = fields.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .with_context(|| format!("surface route binding `{key}` is not a string"))?;
    if !valid_parameter_name(value) {
        bail!("surface route binding `{key}` is not a bounded parameter name");
    }
    Ok(Some(value.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn compile_affordances(
    view_ref: &str,
    items: &[Value],
    input_submit: Option<&str>,
    has_project: bool,
    trusted_surface: bool,
    affordance_ceiling: &[String],
    caller_scopes: &[String],
    resolve_target: &mut impl FnMut(&str) -> Result<CompiledUiTarget>,
    out: &mut BTreeMap<String, BTreeMap<String, CompiledUiAffordance>>,
    attenuated: &mut Vec<AttenuatedUiBinding>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for affordance in items {
        let id = affordance
            .get("id")
            .and_then(Value::as_str)
            .context("view affordance has no id")?;
        if id.is_empty() || !seen.insert(id) {
            bail!("view {view_ref} has empty or duplicate affordance id `{id}`");
        }
        let Some(invoke) = affordance.get("invoke") else {
            continue;
        };
        let producer = match affordance.get("producer").and_then(Value::as_str) {
            Some("selection") => CompiledUiProducer::Selection,
            Some("input") => CompiledUiProducer::Input,
            Some("tokens") => CompiledUiProducer::Tokens,
            Some(other) => {
                bail!("UI item {view_ref} affordance `{id}` has unsupported producer `{other}`")
            }
            None if input_submit == Some(id) => CompiledUiProducer::Input,
            None => CompiledUiProducer::Selection,
        };
        let coordinate = format!("affordance:{view_ref}:{id}");
        if affordance
            .get("requires_project")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && !has_project
        {
            attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: "affordance requires a project-bound session".to_string(),
            });
            continue;
        }
        if !trusted_surface {
            attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: "untrusted surface/view authority excludes affordances".to_string(),
            });
            continue;
        }
        match invoke.get("plane").and_then(Value::as_str) {
            Some("ui") => {
                out.entry(view_ref.to_string()).or_default().insert(
                    id.to_string(),
                    CompiledUiAffordance::Local {
                        producer,
                        invoke: invoke.clone(),
                    },
                );
            }
            Some("rye") => {
                let Some(target_ref) = invoke.get("ref").and_then(Value::as_str) else {
                    attenuated.push(AttenuatedUiBinding {
                        coordinate,
                        reason: "executable affordance has no signed target ref".to_string(),
                    });
                    continue;
                };
                match resolve_target(target_ref) {
                    Ok(target)
                        if lane_allows(affordance_ceiling, &target.required_caps)
                            && caller_has_all(caller_scopes, &target.required_caps) =>
                    {
                        out.entry(view_ref.to_string()).or_default().insert(
                            id.to_string(),
                            CompiledUiAffordance::Execution {
                                producer,
                                invoke: invoke.clone(),
                                target,
                            },
                        );
                    }
                    Ok(_) => attenuated.push(AttenuatedUiBinding {
                        coordinate,
                        reason: "caller lacks target capabilities".to_string(),
                    }),
                    Err(error) => attenuated.push(AttenuatedUiBinding {
                        coordinate,
                        reason: format!("target resolution failed: {error}"),
                    }),
                }
            }
            _ => attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: "affordance has an unsupported invocation plane".to_string(),
            }),
        }
    }
    Ok(())
}

fn caller_has_all(caller_scopes: &[String], required: &[String]) -> bool {
    required.iter().all(|required| {
        caller_scopes
            .iter()
            .any(|granted| ryeos_state::capability::grant_matches(granted, required))
    })
}

fn capability_lane(surface: &Value, lane: &str) -> Result<Vec<String>> {
    let values = surface
        .get("capabilities")
        .and_then(|value| value.get(lane))
        .and_then(Value::as_array)
        .with_context(|| format!("effective surface has no `{lane}` capability lane"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("effective surface `{lane}` capability is not a string"))
        })
        .collect()
}

fn lane_allows(patterns: &[String], required: &[String]) -> bool {
    !patterns.is_empty()
        && required.iter().all(|required| {
            patterns
                .iter()
                .any(|granted| ryeos_state::capability::grant_matches(granted, required))
        })
}

fn source_dynamic_parameters(feeds: &Value) -> Vec<String> {
    let mut parameters = BTreeSet::new();
    if let Some(param) = feeds.get("param").and_then(Value::as_str)
        && !param.is_empty()
    {
        parameters.insert(param.to_string());
    }
    if let Some(fields) = feeds.get("fields").and_then(Value::as_array) {
        parameters.extend(fields.iter().filter_map(|field| {
            field
                .get("param")
                .and_then(Value::as_str)
                .filter(|param| !param.is_empty())
                .map(str::to_string)
        }));
    }
    parameters.into_iter().collect()
}

fn is_trusted(identity: &EffectiveUiItemIdentity) -> bool {
    matches!(
        identity.effective_trust_class,
        ryeos_engine::resolution::TrustClass::TrustedBundle
            | ryeos_engine::resolution::TrustClass::TrustedProject
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

    fn identity(item_ref: &str, byte: char) -> EffectiveUiItemIdentity {
        EffectiveUiItemIdentity {
            canonical_ref: item_ref.to_string(),
            effective_definition_digest: EffectiveDefinitionDigest::parse(
                byte.to_string().repeat(64),
            )
            .unwrap(),
            effective_trust_class: TrustClass::TrustedBundle,
        }
    }

    fn compile(surface: Value) -> SessionCompiledUiBinding {
        let view_ref = "view:test/input";
        SessionCompiledUiBinding::compile(
            BindingCompileContext {
                contract_revision: "test.v1",
                principal_id: "principal:test",
                caller_scopes: &["cap.read".into(), "cap.write".into()],
                project_root: Some("/project"),
                node_policy_generation_digest: "policy:test",
                identities: EmbeddedSurfaceIdentity {
                    request_engine_generation_identity: "engine:test".into(),
                    surface: identity("surface:test/root", 'a'),
                    views: BTreeMap::from([(
                        view_ref.into(),
                        EmbeddedViewIdentity::Resolved {
                            identity: identity(view_ref, 'b'),
                        },
                    )]),
                },
            },
            &surface,
            |item_ref| {
                let read_only = item_ref.ends_with("/read");
                Ok(CompiledUiTarget {
                    identity: identity(item_ref, if read_only { 'c' } else { 'd' }),
                    required_caps: vec![if read_only { "cap.read" } else { "cap.write" }.into()],
                    source_safe: read_only,
                    dispatch_class: CompiledUiDispatchClass::Verified,
                    result_effect: None,
                    parameter_schema: BTreeMap::from([
                        ("input".to_string(), "string".to_string()),
                        ("target".to_string(), "object".to_string()),
                        ("intent".to_string(), "string?".to_string()),
                    ]),
                })
            },
        )
        .unwrap()
    }

    fn surface(affordance_caps: Value) -> Value {
        serde_json::json!({
            "capabilities": {"sources":["cap.read"], "affordances":affordance_caps},
            "sources": {
                "shell": {
                    "ref":"service:test/read",
                    "params":{"query":""},
                    "dynamic_parameters":["query"]
                }
            },
            "views": {"view:test/input": {
                "sources": {"default":{"ref":"service:test/read","params":{}}},
                "affordances": [{
                    "id":"run", "producer":"tokens",
                    "invoke":{"plane":"rye","ref":"service:test/write","args":{"tokens":"{tokens}"}}
                }]
            }},
            "input":{"route":{
                "invoke":{"ref":"service:test/write"},
                "bindings":{
                    "input_parameter":"input",
                    "thread_target_parameter":"target",
                    "interrupt_intent_parameter":"intent"
                },
                "params":{}
            }}
        })
    }

    #[test]
    fn empty_affordance_lane_retains_reads_and_erases_execution() {
        let compiled = compile(surface(serde_json::json!([])));
        assert_eq!(
            compiled.binding.sources["surface:test/root"]["shell"].dynamic_parameters,
            ["query"]
        );
        assert!(compiled.binding.sources["view:test/input"].contains_key("default"));
        assert!(compiled.binding.affordances.is_empty());
        assert!(compiled.binding.surface_route.is_none());
        assert_eq!(compiled.posture, EffectiveUiPosture::ObservationOnly);
    }

    #[test]
    fn signed_affordance_lane_compiles_exact_targets_and_derived_posture() {
        let compiled = compile(surface(serde_json::json!(["cap.write"])));
        assert!(matches!(
            compiled.binding.affordances["view:test/input"]["run"],
            CompiledUiAffordance::Execution {
                producer: CompiledUiProducer::Tokens,
                ..
            }
        ));
        assert_eq!(
            compiled
                .binding
                .surface_route
                .as_ref()
                .unwrap()
                .target
                .identity
                .canonical_ref,
            "service:test/write"
        );
        assert_eq!(compiled.posture, EffectiveUiPosture::Interactive);
    }

    #[test]
    fn malformed_dynamic_source_parameter_contract_is_refused() {
        let mut value = surface(serde_json::json!([]));
        value["sources"]["shell"]["dynamic_parameters"] = serde_json::json!(["query", "query"]);
        let result = SessionCompiledUiBinding::compile(
            BindingCompileContext {
                contract_revision: "test.v1",
                principal_id: "principal:test",
                caller_scopes: &["cap.read".into()],
                project_root: None,
                node_policy_generation_digest: "policy:test",
                identities: EmbeddedSurfaceIdentity {
                    request_engine_generation_identity: "engine:test".into(),
                    surface: identity("surface:test/root", 'a'),
                    views: BTreeMap::new(),
                },
            },
            &value,
            |item_ref| {
                Ok(CompiledUiTarget {
                    identity: identity(item_ref, 'c'),
                    required_caps: vec!["cap.read".into()],
                    source_safe: true,
                    dispatch_class: CompiledUiDispatchClass::Verified,
                    result_effect: None,
                    parameter_schema: BTreeMap::new(),
                })
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn source_lane_refuses_targets_without_read_only_state_authority() {
        let mut value = surface(serde_json::json!([]));
        value["sources"]["shell"]["ref"] = serde_json::json!("service:test/write");
        let compiled = compile(value);
        assert!(compiled.binding.sources.get("surface:test/root").is_none());
        assert!(
            compiled
                .binding
                .attenuated
                .iter()
                .any(|entry| entry.coordinate == "source:surface:test/root:shell")
        );
    }

    #[test]
    fn route_semantic_slots_must_match_the_signed_target_schema() {
        let mut value = surface(serde_json::json!(["cap.write"]));
        value["input"]["route"]["bindings"]["input_parameter"] = serde_json::json!("target");
        let compiled = compile(value);
        assert!(compiled.binding.surface_route.is_none());
        assert!(compiled.binding.attenuated.iter().any(|entry| {
            entry.coordinate == "surface_route"
                && entry
                    .reason
                    .contains("bindings must name distinct parameters")
        }));
    }

    #[test]
    fn authored_route_params_cannot_supply_renderer_thread_target() {
        let mut value = surface(serde_json::json!(["cap.write"]));
        value["input"]["route"]["params"]["target"] =
            serde_json::json!({"thread_id":"attacker-selected"});
        let compiled = compile(value);
        assert!(compiled.binding.surface_route.is_none());
        assert!(compiled.binding.attenuated.iter().any(|entry| {
            entry.coordinate == "surface_route"
                && entry.reason.contains("renderer-produced semantic slots")
        }));
    }

    #[test]
    fn signed_project_requirements_attenuate_projectless_behavior() {
        let mut value = surface(serde_json::json!(["cap.write"]));
        value["input"]["route"]["requires_project"] = serde_json::json!(true);
        value["views"]["view:test/input"]["affordances"][0]["requires_project"] =
            serde_json::json!(true);
        value["sources"]["shell"]["requires_project"] = serde_json::json!(true);
        let compiled = SessionCompiledUiBinding::compile(
            BindingCompileContext {
                contract_revision: "test.v1",
                principal_id: "principal:test",
                caller_scopes: &["cap.read".into(), "cap.write".into()],
                project_root: None,
                node_policy_generation_digest: "policy:test",
                identities: EmbeddedSurfaceIdentity {
                    request_engine_generation_identity: "engine:test".into(),
                    surface: identity("surface:test/root", 'a'),
                    views: BTreeMap::from([(
                        "view:test/input".into(),
                        EmbeddedViewIdentity::Resolved {
                            identity: identity("view:test/input", 'b'),
                        },
                    )]),
                },
            },
            &value,
            |item_ref| {
                Ok(CompiledUiTarget {
                    identity: identity(item_ref, 'c'),
                    required_caps: vec![
                        if item_ref.ends_with("/read") {
                            "cap.read"
                        } else {
                            "cap.write"
                        }
                        .into(),
                    ],
                    source_safe: item_ref.ends_with("/read"),
                    dispatch_class: CompiledUiDispatchClass::Verified,
                    result_effect: None,
                    parameter_schema: BTreeMap::from([
                        ("input".into(), "string".into()),
                        ("target".into(), "object".into()),
                        ("intent".into(), "string?".into()),
                    ]),
                })
            },
        )
        .unwrap();
        assert!(compiled.binding.surface_route.is_none());
        assert!(compiled.binding.affordances.is_empty());
        assert!(
            compiled
                .binding
                .sources
                .get("surface:test/root")
                .is_none_or(|sources| !sources.contains_key("shell"))
        );
        assert_eq!(compiled.posture, EffectiveUiPosture::ObservationOnly);
    }
}
