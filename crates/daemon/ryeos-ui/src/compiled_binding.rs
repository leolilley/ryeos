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
    SurfaceOpening,
    ResumeParticularViewSet,
    ResumeParticularSurface,
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
    /// The source was declared by an item with a project-space member, or
    /// under a project-authored Surface. Dispatch carries this to the
    /// service's authorization, which then admits only data of the session's
    /// own live project.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project_authored: bool,
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
    /// Digest of the node's project-authoring source list this binding was
    /// compiled under, when its surface or any view it binds has a
    /// project-space member. Empty for a binding of bundle items only, which
    /// the list does not shape. Revalidation compares it with the list then
    /// installed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_source_ceiling_digest: String,
}

/// Digest of a project-authoring source list, independent of its order.
pub fn project_source_ceiling_digest(sources: &[String]) -> Result<String> {
    let sorted: BTreeSet<&str> = sources.iter().map(String::as_str).collect();
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "project_authoring_sources": sorted,
    }))
}

impl CompiledUiBinding {
    /// Whether the admitted composition root is an item of exactly this kind.
    /// The kind is read from the root's canonical identity, never from a
    /// separately stored discriminant.
    pub fn root_is_kind(&self, kind: &str) -> bool {
        ryeos_engine::canonical_ref::CanonicalRef::parse(&self.surface.canonical_ref)
            .is_ok_and(|parsed| parsed.kind == kind)
    }
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
    /// Node-authored capability patterns naming the read services an item
    /// authored in project space may bind as sources. The caller loads it
    /// from signed bundle-space data; nothing a project supplies reaches it.
    pub project_source_ceiling: &'a [String],
}

/// Name of the refusal for a source a project-authored item binds outside
/// the node's list of read services for project-authored UI.
pub const REFUSAL_PROJECT_SOURCE_OUTSIDE_CEILING: &str = "project_authored_source_outside_ceiling";
/// Name of the refusal for an executing affordance on a project-authored
/// view, or on any view or surface composed under a project-authored surface.
pub const REFUSAL_PROJECT_EXECUTING_AFFORDANCE: &str = "project_authored_executing_affordance";
/// Name of the refusal for an input route on a project-authored surface.
pub const REFUSAL_PROJECT_SURFACE_ROUTE: &str = "project_authored_surface_route";
/// Name of the refusal for a target outside the composing surface's lane.
pub const REFUSAL_TARGET_OUTSIDE_SURFACE_CEILING: &str = "target_outside_surface_ceiling";

/// Typed refusal of a binding target before it is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiTargetRefusal {
    /// A source or affordance names an item whose kind does not terminate in
    /// the daemon's service registry. Only services are UI targets: a tool,
    /// directive or graph is launched by naming it, never by a bound control.
    NotAService { target_ref: String, kind: String },
}

impl UiTargetRefusal {
    pub const NOT_A_SERVICE_CODE: &'static str = "ui_target_not_a_service";

    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAService { .. } => Self::NOT_A_SERVICE_CODE,
        }
    }
}

impl std::fmt::Display for UiTargetRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAService { target_ref, kind } => write!(
                f,
                "{}: UI sources and affordances may target services only; `{target_ref}` is a `{kind}`",
                Self::NOT_A_SERVICE_CODE
            ),
        }
    }
}

impl std::error::Error for UiTargetRefusal {}

/// Who authored a surface or view, as far as binding authority goes. Read
/// from the engine's weakest-link trust class over the item's extends chain,
/// so one project-space member makes the whole item project-authored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Authority {
    /// Every member is a verified bundle item.
    Node,
    /// A project-space member signed by a signer pinned for that project.
    Project,
    /// Unsigned, or signed by a signer the node does not trust.
    Untrusted,
}

impl Authority {
    fn of(identity: &EffectiveUiItemIdentity) -> Self {
        use ryeos_engine::resolution::TrustClass;
        match identity.effective_trust_class {
            TrustClass::TrustedBundle => Self::Node,
            TrustClass::TrustedProject => Self::Project,
            TrustClass::TrustedNode | TrustClass::UntrustedProject | TrustClass::Unsigned => {
                Self::Untrusted
            }
        }
    }

    /// The weaker of a surface's authority and the authority of a view it
    /// binds: a view never holds more than the surface composing it.
    fn under(self, surface: Self) -> Self {
        match (self, surface) {
            (Self::Untrusted, _) | (_, Self::Untrusted) => Self::Untrusted,
            (Self::Project, _) | (_, Self::Project) => Self::Project,
            (Self::Node, Self::Node) => Self::Node,
        }
    }
}

/// The grant one owner (the surface, or one view) compiles under.
struct OwnerGrant<'a> {
    authority: Authority,
    source_ceiling: &'a [String],
    affordance_ceiling: &'a [String],
    project_source_ceiling: &'a [String],
    caller_scopes: &'a [String],
    has_project: bool,
}

impl OwnerGrant<'_> {
    /// Why a resolved read target is not admitted as a source, or `None`.
    fn source_refusal(&self, target: &CompiledUiTarget, untrusted_reason: &str) -> Option<String> {
        if self.authority == Authority::Untrusted {
            return Some(untrusted_reason.to_string());
        }
        if !target.source_safe {
            return Some("source target is not declared read-only".to_string());
        }
        if self.authority == Authority::Project && !lane_allows(self.project_source_ceiling, target)
        {
            return Some(format!(
                "{REFUSAL_PROJECT_SOURCE_OUTSIDE_CEILING}: a project-authored item may bind only the read services the node lists for project-authored UI; `{}` is not listed",
                target.identity.canonical_ref
            ));
        }
        if !lane_allows(self.source_ceiling, target) {
            return Some(format!(
                "{REFUSAL_TARGET_OUTSIDE_SURFACE_CEILING}: `{}` is outside the surface's source lane",
                target.identity.canonical_ref
            ));
        }
        if !caller_has_all(self.caller_scopes, &target.required_caps) {
            return Some("caller lacks target capabilities".to_string());
        }
        None
    }

    /// Why a resolved target is not admitted for execution, or `None`. A
    /// project-authored owner never reaches this: it is refused before its
    /// target is resolved.
    fn execution_refusal(&self, target: &CompiledUiTarget) -> Option<String> {
        if !lane_allows(self.affordance_ceiling, target) {
            return Some(format!(
                "{REFUSAL_TARGET_OUTSIDE_SURFACE_CEILING}: `{}` is outside the surface's affordance lane",
                target.identity.canonical_ref
            ));
        }
        if !caller_has_all(self.caller_scopes, &target.required_caps) {
            return Some("caller lacks target capabilities".to_string());
        }
        None
    }
}

/// The reason recorded when a target could not be compiled. A typed refusal
/// keeps its own name; any other failure is a resolution failure.
fn target_failure_reason(error: &anyhow::Error) -> String {
    match error.downcast_ref::<UiTargetRefusal>() {
        Some(refusal) => refusal.to_string(),
        None => format!("target resolution failed: {error:#}"),
    }
}

impl SessionCompiledUiBinding {
    pub fn compile(
        context: BindingCompileContext<'_>,
        effective_surface: &Value,
        mut resolve_target: impl FnMut(&str) -> Result<CompiledUiTarget>,
    ) -> Result<Self> {
        ryeos_client_base::surface::view_sets::validate_effective_view_sets(effective_surface)
            .map_err(anyhow::Error::msg)?;
        // A surface that declares no views carries no embedded mapping; it
        // compiles to a binding with no view sources. A present mapping must
        // be an object.
        let no_views = serde_json::Map::new();
        let views = match effective_surface.get("views") {
            None => &no_views,
            Some(views) => views
                .as_object()
                .context("effective surface views mapping is not an object")?,
        };
        let surface_authority = Authority::of(&context.identities.surface);
        // A node surface must declare both lanes. A surface authored in
        // project space never sets its own ceiling: a lane it declares can
        // only narrow, and one it omits is the node's list for sources and
        // nothing for affordances.
        let (source_ceiling, affordance_ceiling) = match surface_authority {
            Authority::Node => (
                capability_lane(effective_surface, "sources")?,
                capability_lane(effective_surface, "affordances")?,
            ),
            Authority::Project | Authority::Untrusted => (
                optional_capability_lane(effective_surface, "sources")?
                    .unwrap_or_else(|| context.project_source_ceiling.to_vec()),
                optional_capability_lane(effective_surface, "affordances")?.unwrap_or_default(),
            ),
        };
        let grant_for = |authority: Authority| OwnerGrant {
            authority,
            source_ceiling: &source_ceiling,
            affordance_ceiling: &affordance_ceiling,
            project_source_ceiling: context.project_source_ceiling,
            caller_scopes: context.caller_scopes,
            has_project: context.project_root.is_some(),
        };
        let surface_grant = grant_for(surface_authority);

        let mut sources = BTreeMap::new();
        let mut affordances = BTreeMap::new();
        let mut attenuated = Vec::new();

        // Shell-wide observations are authored by the surface itself. They
        // use the surface's canonical identity as their coordinate namespace,
        // rather than living in a hard-coded browser bootstrap API.
        if let Some(source_map) = effective_surface.get("sources").and_then(Value::as_object) {
            let surface_ref = context.identities.surface.canonical_ref.clone();
            compile_sources(
                &surface_ref,
                source_map,
                &surface_grant,
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
            let view_grant = grant_for(Authority::of(identity).under(surface_authority));
            let dynamic_parameters = view
                .get("input")
                .and_then(|input| input.get("feeds"))
                .map(source_dynamic_parameters)
                .unwrap_or_default();

            if let Some(source_map) = view.get("sources").and_then(Value::as_object) {
                compile_sources(
                    &coordinate_view_ref,
                    source_map,
                    &view_grant,
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
                    let refusal = match resolve_target(item_ref) {
                        Ok(target) => match view_grant.source_refusal(
                            &target,
                            "untrusted surface/view authority excludes input sources",
                        ) {
                            Some(reason) => reason,
                            None => {
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
                                            project_authored: view_grant.authority
                                                == Authority::Project,
                                        },
                                    );
                                continue;
                            }
                        },
                        Err(error) => target_failure_reason(&error),
                    };
                    attenuated.push(AttenuatedUiBinding {
                        coordinate,
                        reason: refusal,
                    });
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
                    &view_grant,
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
                &surface_grant,
                &mut resolve_target,
                &mut affordances,
                &mut attenuated,
            )?;
        }

        let surface_route = compile_surface_route(
            effective_surface,
            &surface_grant,
            &mut resolve_target,
            &mut attenuated,
        );
        let shaped_by_project_ceiling = surface_authority != Authority::Node
            || context.identities.views.values().any(|view| {
                matches!(
                    view,
                    EmbeddedViewIdentity::Resolved { identity }
                        if Authority::of(identity) != Authority::Node
                )
            });
        let project_source_ceiling_digest = if shaped_by_project_ceiling {
            project_source_ceiling_digest(context.project_source_ceiling)?
        } else {
            String::new()
        };
        let binding = CompiledUiBinding {
            project_source_ceiling_digest,
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
        mark_authorship(
            surface_fields,
            Some(&self.binding.surface),
            self.binding
                .sources
                .get(&self.binding.surface.canonical_ref),
        );
        if self.binding.surface_route.is_none()
            && let Some(input) = surface_fields
                .get_mut("input")
                .and_then(Value::as_object_mut)
        {
            input.remove("route");
        }

        if let Some(views) = surface_fields
            .get_mut("views")
            .and_then(Value::as_object_mut)
        {
            for (view_ref, view) in views {
                let Some(fields) = view.as_object_mut() else {
                    continue;
                };
                let identity = match self.binding.views.get(view_ref) {
                    Some(EmbeddedViewIdentity::Resolved { identity }) => Some(identity),
                    Some(EmbeddedViewIdentity::Degraded { .. }) | None => None,
                };
                mark_authorship(fields, identity, self.binding.sources.get(view_ref));
                sanitize_sources(fields, self.binding.sources.get(view_ref));
                sanitize_affordances(fields, self.binding.affordances.get(view_ref));
                let retained = self.binding.affordances.get(view_ref);
                if let Some(input) = fields.get_mut("input").and_then(Value::as_object_mut)
                    && input
                        .get("submit")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !retained.is_some_and(|items| items.contains_key(id)))
                {
                    input.remove("submit");
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

fn compile_sources(
    owner_ref: &str,
    source_map: &serde_json::Map<String, Value>,
    grant: &OwnerGrant<'_>,
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
        if authored.requires_project && !grant.has_project {
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
        let refusal = match resolve_target(item_ref) {
            Ok(target) => match grant
                .source_refusal(&target, "untrusted surface/view authority excludes sources")
            {
                Some(reason) => reason,
                None => {
                    out.entry(owner_ref.to_string()).or_default().insert(
                        channel.clone(),
                        CompiledUiSource {
                            target,
                            parameters: authored.params,
                            dynamic_parameters: dynamic_parameters.into_iter().collect(),
                            activation: authored.activation,
                            requires_project: authored.requires_project,
                            project_authored: grant.authority == Authority::Project,
                        },
                    );
                    continue;
                }
            },
            Err(error) => target_failure_reason(&error),
        };
        attenuated.push(AttenuatedUiBinding {
            coordinate,
            reason: refusal,
        });
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
    grant: &OwnerGrant<'_>,
    resolve_target: &mut impl FnMut(&str) -> Result<CompiledUiTarget>,
    attenuated: &mut Vec<AttenuatedUiBinding>,
) -> Option<CompiledUiRoute> {
    let route = surface.get("input")?.get("route")?;
    if route
        .get("requires_project")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !grant.has_project
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
                reason: format!("surface route bindings are invalid: {error:#}"),
            });
            return None;
        }
    };
    let coordinate = "surface_route".to_string();
    match grant.authority {
        Authority::Node => {}
        Authority::Project => {
            attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: format!(
                    "{REFUSAL_PROJECT_SURFACE_ROUTE}: a project-authored surface may not declare an input route"
                ),
            });
            return None;
        }
        Authority::Untrusted => {
            attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: "surface authority excludes an executable input route".to_string(),
            });
            return None;
        }
    }
    match resolve_target(item_ref) {
        Ok(target) if grant.execution_refusal(&target).is_none() => {
            let parameters = route
                .get("params")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            if let Err(error) = validate_route_target_contract(&target, &bindings, &parameters) {
                attenuated.push(AttenuatedUiBinding {
                    coordinate,
                    reason: format!("surface route target contract is invalid: {error:#}"),
                });
                return None;
            }
            Some(CompiledUiRoute {
                target,
                parameters,
                bindings,
            })
        }
        Ok(target) => {
            attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: grant
                    .execution_refusal(&target)
                    .unwrap_or_else(|| "surface route is not admitted".to_string()),
            });
            None
        }
        Err(error) => {
            attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: target_failure_reason(&error),
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

fn compile_affordances(
    view_ref: &str,
    items: &[Value],
    input_submit: Option<&str>,
    grant: &OwnerGrant<'_>,
    resolve_target: &mut impl FnMut(&str) -> Result<CompiledUiTarget>,
    out: &mut BTreeMap<String, BTreeMap<String, CompiledUiAffordance>>,
    attenuated: &mut Vec<AttenuatedUiBinding>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for affordance in items {
        ryeos_client_base::ui::content::validate_affordance_eligibility(affordance)
            .map_err(anyhow::Error::msg)?;
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
            && !grant.has_project
        {
            attenuated.push(AttenuatedUiBinding {
                coordinate,
                reason: "affordance requires a project-bound session".to_string(),
            });
            continue;
        }
        if grant.authority == Authority::Untrusted {
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
            // A project-authored owner binds selection and navigation only.
            // Its executing affordance is refused by name before the target
            // is resolved, whatever the target is, and the rest of the item
            // still compiles, as for any other entry outside its grant.
            Some("rye") if grant.authority == Authority::Project => {
                attenuated.push(AttenuatedUiBinding {
                    coordinate,
                    reason: format!(
                        "{REFUSAL_PROJECT_EXECUTING_AFFORDANCE}: a project-authored item may bind UI-plane affordances only"
                    ),
                });
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
                    Ok(target) if grant.execution_refusal(&target).is_none() => {
                        out.entry(view_ref.to_string()).or_default().insert(
                            id.to_string(),
                            CompiledUiAffordance::Execution {
                                producer,
                                invoke: invoke.clone(),
                                target,
                            },
                        );
                    }
                    Ok(target) => attenuated.push(AttenuatedUiBinding {
                        coordinate,
                        reason: grant
                            .execution_refusal(&target)
                            .unwrap_or_else(|| "affordance is not admitted".to_string()),
                    }),
                    Err(error) => attenuated.push(AttenuatedUiBinding {
                        coordinate,
                        reason: target_failure_reason(&error),
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

/// A lane the surface may omit. A present lane must still be a string list.
fn optional_capability_lane(surface: &Value, lane: &str) -> Result<Option<Vec<String>>> {
    match surface
        .get("capabilities")
        .and_then(|value| value.get(lane))
    {
        None | Some(Value::Null) => Ok(None),
        Some(_) => capability_lane(surface, lane).map(Some),
    }
}

/// The capability naming execution of exactly this target, in the vocabulary
/// surface lanes are written in. `None` when the target's ref does not parse.
fn target_execute_capability(target: &CompiledUiTarget) -> Option<String> {
    let parsed =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&target.identity.canonical_ref).ok()?;
    Some(ryeos_runtime::authorizer::canonical_cap(
        &parsed.kind,
        &parsed.bare_id,
        "execute",
    ))
}

/// Whether a lane admits a target. The lane must name the target itself, by
/// its execute capability, and every capability the target declares. A
/// target that declares none therefore passes only a lane that names it;
/// declaring nothing is never a way past a ceiling.
fn lane_allows(patterns: &[String], target: &CompiledUiTarget) -> bool {
    let admits = |required: &str| {
        patterns
            .iter()
            .any(|granted| ryeos_state::capability::grant_matches(granted, required))
    };
    target_execute_capability(target).is_some_and(|capability| admits(&capability))
        && target.required_caps.iter().all(|required| admits(required))
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

/// Settle the authorship marker an item carries in the presentation. An
/// item every member of which is a verified bundle item carries none. Any
/// other item carries one whose space, trust class and ref are written here
/// from the binding's own identity record, and whose `source_refs` are the
/// sources this binding actually retained for it, so the marker says what
/// the item reads, not what it asked to read. The signer stays as the
/// composition layer stamped it from the engine's provenance.
fn mark_authorship(
    fields: &mut serde_json::Map<String, Value>,
    identity: Option<&EffectiveUiItemIdentity>,
    retained: Option<&BTreeMap<String, CompiledUiSource>>,
) {
    let key = ryeos_api::surface_views::AUTHORSHIP_KEY;
    let Some(identity) = identity.filter(|identity| Authority::of(identity) != Authority::Node)
    else {
        fields.remove(key);
        return;
    };
    let signer = fields
        .get(key)
        .and_then(|stamped| stamped.get("signer"))
        .filter(|signer| signer.get("fingerprint").is_some_and(Value::is_string))
        .cloned();
    let source_refs: BTreeSet<&str> = retained
        .into_iter()
        .flat_map(|sources| sources.values())
        .map(|source| source.target.identity.canonical_ref.as_str())
        .collect();
    let mut marker = serde_json::json!({
        "space": "project",
        "trust_class": identity.effective_trust_class,
        "ref": identity.canonical_ref,
        "source_refs": source_refs,
    });
    if let Some(signer) = signer {
        marker["signer"] = signer;
    }
    fields.insert(key.to_string(), marker);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

    #[test]
    fn result_effect_vocabulary_is_the_three_declared_effects() {
        for effect in [
            CompiledUiResultEffect::SurfaceOpening,
            CompiledUiResultEffect::ResumeParticularViewSet,
            CompiledUiResultEffect::ResumeParticularSurface,
        ] {
            let wire = serde_json::to_value(effect).unwrap();
            assert_eq!(
                serde_json::from_value::<CompiledUiResultEffect>(wire).unwrap(),
                effect
            );
        }
        for retired in ["admit_binding_attachment", "open_surface"] {
            assert!(
                serde_json::from_value::<CompiledUiResultEffect>(serde_json::json!(retired))
                    .is_err()
            );
        }
    }

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
                project_source_ceiling: &[],
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

    /// A lane names the targets it admits by their execute capability, plus
    /// whatever capability each target declares. A non-empty affordance lane
    /// therefore also names the test services.
    fn surface(affordance_caps: Value) -> Value {
        let mut affordance_caps = affordance_caps;
        if let Some(lane) = affordance_caps.as_array_mut()
            && !lane.is_empty()
        {
            lane.push(serde_json::json!("ryeos.execute.service.test/*"));
        }
        serde_json::json!({
            "capabilities": {
                "sources":["cap.read", "ryeos.execute.service.test/*"],
                "affordances":affordance_caps
            },
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
                project_source_ceiling: &[],
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
        assert!(!compiled.binding.sources.contains_key("surface:test/root"));
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
                project_source_ceiling: &[],
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

    // ── Project-authored surfaces and views ─────────────────────────────
    //
    // A surface or view whose effective trust class says a project-space
    // file is part of it binds less than a bundle item: read sources the
    // node lists, UI-plane affordances, and nothing that executes.

    const VIEW_REF: &str = "view:test/input";
    const SURFACE_REF: &str = "surface:test/root";
    const FIELD_PROJECT: &str = "service:ui/ryeos-ui/field/project";
    const FIELD_RUNS: &str = "service:ui/ryeos-ui/field/runs";
    const FIELD_EXECUTION: &str = "service:ui/ryeos-ui/field/execution";
    const FIELD_DEFINITION: &str = "service:ui/ryeos-ui/field/definition";

    fn ceiling(services: &[&str]) -> Vec<String> {
        services
            .iter()
            .map(|service| {
                let bare = service.strip_prefix("service:").expect("service ref");
                format!("ryeos.execute.service.{bare}")
            })
            .collect()
    }

    fn every_field_service() -> Vec<String> {
        ceiling(&[FIELD_PROJECT, FIELD_RUNS, FIELD_EXECUTION, FIELD_DEFINITION])
    }

    fn identity_of(item_ref: &str, byte: char, trust: TrustClass) -> EffectiveUiItemIdentity {
        EffectiveUiItemIdentity {
            effective_trust_class: trust,
            ..identity(item_ref, byte)
        }
    }

    /// Stand-in for the signed service registry. `service:test/…/read` is a
    /// read-only service needing `cap.read`; any other `service:test/…`
    /// writes and needs `cap.write`; the UI field services are read-only and
    /// declare no capability. Anything that is not a service is refused the
    /// way the daemon's target compiler refuses it.
    fn bundle_service_target(item_ref: &str) -> Result<CompiledUiTarget> {
        let Some(bare) = item_ref.strip_prefix("service:") else {
            let kind = item_ref.split(':').next().unwrap_or_default().to_string();
            return Err(UiTargetRefusal::NotAService {
                target_ref: item_ref.to_string(),
                kind,
            }
            .into());
        };
        let field = bare.starts_with("ui/ryeos-ui/field/");
        let read_only = field || item_ref.ends_with("/read");
        Ok(CompiledUiTarget {
            identity: identity(item_ref, if read_only { 'c' } else { 'd' }),
            required_caps: if field {
                Vec::new()
            } else {
                vec![if read_only { "cap.read" } else { "cap.write" }.into()]
            },
            source_safe: read_only,
            dispatch_class: CompiledUiDispatchClass::Verified,
            result_effect: None,
            parameter_schema: BTreeMap::from([
                ("input".to_string(), "string".to_string()),
                ("target".to_string(), "object".to_string()),
                ("intent".to_string(), "string?".to_string()),
            ]),
        })
    }

    fn compile_authored_by(
        surface_trust: TrustClass,
        view_trust: TrustClass,
        caller_scopes: &[&str],
        project_source_ceiling: &[String],
        surface: &Value,
        resolve_target: impl FnMut(&str) -> Result<CompiledUiTarget>,
    ) -> SessionCompiledUiBinding {
        let caller_scopes: Vec<String> = caller_scopes.iter().map(|s| s.to_string()).collect();
        let views = surface
            .get("views")
            .and_then(Value::as_object)
            .map(|views| {
                views
                    .keys()
                    .map(|view_ref| {
                        (
                            view_ref.clone(),
                            EmbeddedViewIdentity::Resolved {
                                identity: identity_of(view_ref, 'b', view_trust),
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        SessionCompiledUiBinding::compile(
            BindingCompileContext {
                contract_revision: "test.v1",
                principal_id: "principal:test",
                caller_scopes: &caller_scopes,
                project_root: Some("/project"),
                node_policy_generation_digest: "policy:test",
                project_source_ceiling,
                identities: EmbeddedSurfaceIdentity {
                    request_engine_generation_identity: "engine:test".into(),
                    surface: identity_of(SURFACE_REF, 'a', surface_trust),
                    views,
                },
            },
            surface,
            resolve_target,
        )
        .unwrap()
    }

    fn attenuation<'a>(
        compiled: &'a SessionCompiledUiBinding,
        coordinate: &str,
    ) -> Option<&'a str> {
        compiled
            .binding
            .attenuated
            .iter()
            .find(|entry| entry.coordinate == coordinate)
            .map(|entry| entry.reason.as_str())
    }

    fn refused_as(compiled: &SessionCompiledUiBinding, coordinate: &str, name: &str) {
        let reason = attenuation(compiled, coordinate)
            .unwrap_or_else(|| panic!("{coordinate} is not attenuated"));
        assert!(
            reason.starts_with(&format!("{name}: ")),
            "{coordinate} was refused as `{reason}`, expected `{name}`"
        );
    }

    /// The surface every test starts from, with one more source on the view
    /// that the project ceiling lists and one UI-plane affordance.
    fn surface_with_listed_source_and_selection() -> Value {
        let mut authored = surface(serde_json::json!(["cap.write"]));
        authored["capabilities"]["sources"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("ryeos.execute.service.ui/ryeos-ui/*"));
        authored["views"][VIEW_REF]["widget"] = serde_json::json!("field");
        authored["views"][VIEW_REF]["sources"]["project"] =
            serde_json::json!({"ref": FIELD_PROJECT, "params": {}});
        authored["views"][VIEW_REF]["affordances"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": "select",
                "invoke": {"plane": "ui", "facet": "selection"}
            }));
        authored
    }

    /// A view from project space under a pinned signer, bound by a bundle
    /// surface, keeps the read source the node lists and its UI-plane
    /// affordance. Its other source and its executing affordance are refused
    /// by name. The bundle surface's own source and input route are
    /// untouched: only the project item is restricted.
    #[test]
    fn trusted_project_view_under_a_bundle_surface_keeps_only_listed_sources_and_ui_affordances() {
        let compiled = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedProject,
            &["cap.read", "cap.write"],
            &ceiling(&[FIELD_PROJECT]),
            &surface_with_listed_source_and_selection(),
            bundle_service_target,
        );

        let sources = &compiled.binding.sources[VIEW_REF];
        assert_eq!(sources.keys().collect::<Vec<_>>(), ["project"]);
        assert_eq!(
            sources["project"].target.identity.canonical_ref,
            FIELD_PROJECT
        );
        refused_as(
            &compiled,
            "source:view:test/input:default",
            REFUSAL_PROJECT_SOURCE_OUTSIDE_CEILING,
        );

        let affordances = &compiled.binding.affordances[VIEW_REF];
        assert_eq!(affordances.keys().collect::<Vec<_>>(), ["select"]);
        assert!(matches!(
            affordances["select"],
            CompiledUiAffordance::Local { .. }
        ));
        refused_as(
            &compiled,
            "affordance:view:test/input:run",
            REFUSAL_PROJECT_EXECUTING_AFFORDANCE,
        );

        // The bundle surface keeps its own grants.
        assert!(compiled.binding.sources[SURFACE_REF].contains_key("shell"));
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
        assert_eq!(attenuation(&compiled, "surface_route"), None);
    }

    /// The same view authored by a bundle keeps everything, which is what
    /// makes the restriction above a property of the author and not of the
    /// surface, the caller or the targets.
    #[test]
    fn bundle_view_under_the_same_surface_keeps_its_sources_and_executing_affordance() {
        let compiled = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedBundle,
            &["cap.read", "cap.write"],
            &ceiling(&[FIELD_PROJECT]),
            &surface(serde_json::json!(["cap.write"])),
            bundle_service_target,
        );
        assert!(compiled.binding.sources[VIEW_REF].contains_key("default"));
        assert!(matches!(
            compiled.binding.affordances[VIEW_REF]["run"],
            CompiledUiAffordance::Execution { .. }
        ));
        assert!(compiled.binding.attenuated.is_empty());
    }

    /// A project-authored view and surface carry an authorship marker in the
    /// presentation sent to clients; bundle items carry none. The daemon
    /// writes the marker from the engine's trust result, so a view file that
    /// authors the key itself can neither claim nor hide it.
    #[test]
    fn project_authored_items_are_marked_in_the_presentation_and_bundle_items_are_not() {
        let mut authored = surface_with_listed_source_and_selection();
        authored["views"][VIEW_REF]["authorship"] = serde_json::json!({"space": "bundle"});
        authored["authorship"] = serde_json::json!({"space": "bundle"});

        let present = |surface_trust, view_trust| {
            compile_authored_by(
                surface_trust,
                view_trust,
                &["cap.read", "cap.write"],
                &ceiling(&[FIELD_PROJECT]),
                &authored,
                bundle_service_target,
            )
            .sanitize_effective_surface(&authored)
            .unwrap()
        };

        let bundle = present(TrustClass::TrustedBundle, TrustClass::TrustedBundle);
        assert!(bundle.get("authorship").is_none());
        assert!(bundle["views"][VIEW_REF].get("authorship").is_none());

        let project_view = present(TrustClass::TrustedBundle, TrustClass::TrustedProject);
        assert!(project_view.get("authorship").is_none());
        assert_eq!(
            project_view["views"][VIEW_REF]["authorship"],
            serde_json::json!({
                "space": "project",
                "trust_class": "trusted_project",
                "ref": VIEW_REF,
                "source_refs": [FIELD_PROJECT]
            }),
            "the marker lists the sources the binding kept, not the ones the view asked for"
        );

        let project_surface = present(TrustClass::TrustedProject, TrustClass::TrustedBundle);
        assert_eq!(
            project_surface["authorship"],
            serde_json::json!({
                "space": "project",
                "trust_class": "trusted_project",
                "ref": SURFACE_REF,
                "source_refs": []
            })
        );
        assert!(
            project_surface["views"][VIEW_REF]
                .get("authorship")
                .is_none()
        );

        for (trust, name) in [
            (TrustClass::UntrustedProject, "untrusted_project"),
            (TrustClass::Unsigned, "unsigned"),
        ] {
            let untrusted = present(TrustClass::TrustedBundle, trust);
            assert_eq!(
                untrusted["views"][VIEW_REF]["authorship"],
                serde_json::json!({
                    "space": "project",
                    "trust_class": name,
                    "ref": VIEW_REF,
                    "source_refs": []
                })
            );
        }

        // The signer the composition layer stamped from the engine's
        // provenance is carried through; anything else an item wrote under
        // the key is not.
        let mut stamped = authored.clone();
        stamped["views"][VIEW_REF]["authorship"] = serde_json::json!({
            "space": "bundle",
            "source_refs": ["service:test/write"],
            "signer": {"fingerprint": "ab".repeat(32), "name": "Farm author"}
        });
        let carried = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedProject,
            &["cap.read", "cap.write"],
            &ceiling(&[FIELD_PROJECT]),
            &stamped,
            bundle_service_target,
        )
        .sanitize_effective_surface(&stamped)
        .unwrap();
        assert_eq!(
            carried["views"][VIEW_REF]["authorship"],
            serde_json::json!({
                "space": "project",
                "trust_class": "trusted_project",
                "ref": VIEW_REF,
                "source_refs": [FIELD_PROJECT],
                "signer": {"fingerprint": "ab".repeat(32), "name": "Farm author"}
            })
        );
    }

    /// A project view whose signer is not pinned, or which is unsigned,
    /// still takes its place in the surface, but every source and every
    /// affordance it declares is removed, listed sources and UI-local
    /// affordances included. Bundle content around it is unaffected.
    #[test]
    fn untrusted_or_unsigned_project_view_is_attenuated_to_a_static_presentation() {
        let authored = surface_with_listed_source_and_selection();

        for trust in [TrustClass::UntrustedProject, TrustClass::Unsigned] {
            let compiled = compile_authored_by(
                TrustClass::TrustedBundle,
                trust,
                &["cap.read", "cap.write"],
                &ceiling(&[FIELD_PROJECT]),
                &authored,
                bundle_service_target,
            );

            assert!(
                !compiled.binding.sources.contains_key(VIEW_REF),
                "{trust:?}"
            );
            assert!(
                !compiled.binding.affordances.contains_key(VIEW_REF),
                "{trust:?}"
            );
            for channel in ["default", "project"] {
                assert_eq!(
                    attenuation(&compiled, &format!("source:view:test/input:{channel}")),
                    Some("untrusted surface/view authority excludes sources")
                );
            }
            for id in ["run", "select"] {
                assert_eq!(
                    attenuation(&compiled, &format!("affordance:view:test/input:{id}")),
                    Some("untrusted surface/view authority excludes affordances")
                );
            }

            // The bundle surface's own source and input route still compile.
            assert!(compiled.binding.sources[SURFACE_REF].contains_key("shell"));
            assert!(compiled.binding.surface_route.is_some());

            // The view body still reaches the renderer, without behaviour.
            let presentation = compiled.sanitize_effective_surface(&authored).unwrap();
            let view = &presentation["views"][VIEW_REF];
            assert_eq!(view["widget"], "field");
            assert_eq!(view["sources"], serde_json::json!({}));
            assert_eq!(view["affordances"], serde_json::json!([]));
        }
    }

    /// A surface from project space under a pinned signer has no input
    /// route, and neither it nor any view it binds has an executing
    /// affordance, bundle views included. Sources compile only for services
    /// the node lists; the surface's own shell source is not one of them.
    #[test]
    fn trusted_project_surface_has_no_route_no_executing_affordance_and_only_listed_sources() {
        let compiled = compile_authored_by(
            TrustClass::TrustedProject,
            TrustClass::TrustedBundle,
            &["cap.read", "cap.write"],
            &ceiling(&[FIELD_PROJECT]),
            &surface_with_listed_source_and_selection(),
            bundle_service_target,
        );

        assert!(compiled.binding.surface_route.is_none());
        refused_as(&compiled, "surface_route", REFUSAL_PROJECT_SURFACE_ROUTE);
        assert!(!compiled.binding.sources.contains_key(SURFACE_REF));
        refused_as(
            &compiled,
            "source:surface:test/root:shell",
            REFUSAL_PROJECT_SOURCE_OUTSIDE_CEILING,
        );
        assert_eq!(
            compiled.binding.sources[VIEW_REF]
                .keys()
                .collect::<Vec<_>>(),
            ["project"]
        );
        assert!(matches!(
            compiled.binding.affordances[VIEW_REF]["select"],
            CompiledUiAffordance::Local { .. }
        ));
        refused_as(
            &compiled,
            "affordance:view:test/input:run",
            REFUSAL_PROJECT_EXECUTING_AFFORDANCE,
        );
        assert_eq!(compiled.posture, EffectiveUiPosture::ObservationOnly);
    }

    /// A project surface that writes its own wide capabilities block gains
    /// nothing by it. Its declared lanes only narrow the node's ceiling: a
    /// source outside the node's list stays refused, and its affordance lane
    /// admits no executing affordance and no route.
    #[test]
    fn project_surface_with_its_own_capabilities_block_is_bounded_by_the_node_ceiling() {
        let mut authored = surface_with_listed_source_and_selection();
        authored["capabilities"] = serde_json::json!({
            "sources": ["*"],
            "affordances": ["*"]
        });
        authored["sources"]["listed"] = serde_json::json!({"ref": FIELD_PROJECT, "params": {}});
        let compiled = compile_authored_by(
            TrustClass::TrustedProject,
            TrustClass::TrustedProject,
            &["*"],
            &ceiling(&[FIELD_PROJECT]),
            &authored,
            bundle_service_target,
        );

        assert_eq!(
            compiled.binding.sources[SURFACE_REF]
                .keys()
                .collect::<Vec<_>>(),
            ["listed"]
        );
        refused_as(
            &compiled,
            "source:surface:test/root:shell",
            REFUSAL_PROJECT_SOURCE_OUTSIDE_CEILING,
        );
        refused_as(
            &compiled,
            "source:view:test/input:default",
            REFUSAL_PROJECT_SOURCE_OUTSIDE_CEILING,
        );
        refused_as(&compiled, "surface_route", REFUSAL_PROJECT_SURFACE_ROUTE);
        refused_as(
            &compiled,
            "affordance:view:test/input:run",
            REFUSAL_PROJECT_EXECUTING_AFFORDANCE,
        );
        assert!(compiled.binding.affordances.values().all(|owner| {
            owner
                .values()
                .all(|entry| matches!(entry, CompiledUiAffordance::Local { .. }))
        }));

        // A lane the project surface declares can still narrow the node's.
        authored["capabilities"]["sources"] = serde_json::json!([]);
        let narrowed = compile_authored_by(
            TrustClass::TrustedProject,
            TrustClass::TrustedProject,
            &["*"],
            &ceiling(&[FIELD_PROJECT]),
            &authored,
            bundle_service_target,
        );
        assert!(narrowed.binding.sources.is_empty());
    }

    /// A project surface that declares no capabilities block compiles: the
    /// node's list is its source lane and it has no affordance lane. A node
    /// surface with no block is still refused.
    #[test]
    fn project_surface_without_a_capabilities_block_takes_the_node_ceiling() {
        let mut authored = surface_with_listed_source_and_selection();
        authored.as_object_mut().unwrap().remove("capabilities");
        let compiled = compile_authored_by(
            TrustClass::TrustedProject,
            TrustClass::TrustedProject,
            &[],
            &ceiling(&[FIELD_PROJECT]),
            &authored,
            bundle_service_target,
        );
        assert_eq!(
            compiled.binding.sources[VIEW_REF]
                .keys()
                .collect::<Vec<_>>(),
            ["project"]
        );
        assert!(compiled.binding.surface_route.is_none());

        let node_surface = SessionCompiledUiBinding::compile(
            BindingCompileContext {
                contract_revision: "test.v1",
                principal_id: "principal:test",
                caller_scopes: &[],
                project_root: Some("/project"),
                node_policy_generation_digest: "policy:test",
                project_source_ceiling: &[],
                identities: EmbeddedSurfaceIdentity {
                    request_engine_generation_identity: "engine:test".into(),
                    surface: identity(SURFACE_REF, 'a'),
                    views: BTreeMap::new(),
                },
            },
            &authored,
            bundle_service_target,
        );
        assert!(node_surface.is_err());
    }

    /// An untrusted or unsigned project surface carries no behaviour at
    /// all: its route, its sources and every view's sources and affordances
    /// are removed, even for views that are themselves bundle-authored.
    #[test]
    fn untrusted_project_surface_attenuates_itself_and_every_view_it_binds() {
        for trust in [TrustClass::UntrustedProject, TrustClass::Unsigned] {
            let compiled = compile_authored_by(
                trust,
                TrustClass::TrustedBundle,
                &["cap.read", "cap.write"],
                &every_field_service(),
                &surface_with_listed_source_and_selection(),
                bundle_service_target,
            );
            assert!(compiled.binding.sources.is_empty(), "{trust:?}");
            assert!(compiled.binding.affordances.is_empty(), "{trust:?}");
            assert!(compiled.binding.surface_route.is_none(), "{trust:?}");
            assert_eq!(compiled.posture, EffectiveUiPosture::ObservationOnly);
        }
    }

    /// An executing affordance on a project-authored view is refused by
    /// name whatever it targets, and the target is never resolved: a
    /// service, a tool, a directive and a graph are all the same refusal.
    #[test]
    fn project_view_with_an_executing_affordance_is_refused_by_name_whatever_the_target() {
        let mut authored = surface(serde_json::json!(["*"]));
        authored["views"][VIEW_REF]["sources"] = serde_json::json!({});
        authored["views"][VIEW_REF]["affordances"] = serde_json::json!([
            {"id": "service", "invoke": {"plane": "rye", "ref": "service:test/write"}},
            {"id": "tool", "invoke": {"plane": "rye", "ref": "tool:project/anything"}},
            {"id": "directive", "invoke": {"plane": "rye", "ref": "directive:project/anything"}},
            {"id": "graph", "invoke": {"plane": "rye", "ref": "graph:project/anything"}}
        ]);
        let mut resolved = Vec::new();
        let compiled = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedProject,
            &["*"],
            &every_field_service(),
            &authored,
            |item_ref| {
                resolved.push(item_ref.to_string());
                bundle_service_target(item_ref)
            },
        );

        assert!(!compiled.binding.affordances.contains_key(VIEW_REF));
        for id in ["service", "tool", "directive", "graph"] {
            refused_as(
                &compiled,
                &format!("affordance:view:test/input:{id}"),
                REFUSAL_PROJECT_EXECUTING_AFFORDANCE,
            );
        }
        assert!(
            resolved
                .iter()
                .all(|item_ref| item_ref == "service:test/read" || item_ref == "service:test/write"),
            "no affordance target of the project view was resolved: {resolved:?}"
        );
        // The entries leave the presentation with the grant.
        let presentation = compiled.sanitize_effective_surface(&authored).unwrap();
        assert_eq!(
            presentation["views"][VIEW_REF]["affordances"],
            serde_json::json!([])
        );
    }

    /// A source or affordance of a bundle item that names something other
    /// than a service is refused by the target compiler's own name, and the
    /// rest of the item compiles.
    #[test]
    fn non_service_target_on_a_bundle_item_is_refused_by_name() {
        let mut authored = surface(serde_json::json!(["*"]));
        authored["capabilities"]["sources"] = serde_json::json!(["*"]);
        authored["views"][VIEW_REF]["sources"]["tool"] =
            serde_json::json!({"ref": "tool:project/anything", "params": {}});
        authored["views"][VIEW_REF]["affordances"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": "directive",
                "invoke": {"plane": "rye", "ref": "directive:project/anything"}
            }));
        let compiled = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedBundle,
            &["*"],
            &[],
            &authored,
            bundle_service_target,
        );
        refused_as(
            &compiled,
            "source:view:test/input:tool",
            UiTargetRefusal::NOT_A_SERVICE_CODE,
        );
        refused_as(
            &compiled,
            "affordance:view:test/input:directive",
            UiTargetRefusal::NOT_A_SERVICE_CODE,
        );
        assert!(compiled.binding.sources[VIEW_REF].contains_key("default"));
        assert!(compiled.binding.affordances[VIEW_REF].contains_key("run"));
    }

    /// The two bounds on a bundle view's executing affordance: the surface's
    /// affordance lane and the session's own scopes. Each failure has its
    /// own reason.
    #[test]
    fn bundle_view_affordance_is_bounded_by_the_surface_ceiling_and_the_caller_scopes() {
        let outside_caller = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedBundle,
            &["cap.read"],
            &[],
            &surface(serde_json::json!(["cap.write"])),
            bundle_service_target,
        );
        assert!(!outside_caller.binding.affordances.contains_key(VIEW_REF));
        assert_eq!(
            attenuation(&outside_caller, "affordance:view:test/input:run"),
            Some("caller lacks target capabilities")
        );
        assert!(outside_caller.binding.sources[VIEW_REF].contains_key("default"));

        let outside_ceiling = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedBundle,
            &["cap.read", "cap.write"],
            &[],
            &surface(serde_json::json!(["cap.unrelated"])),
            bundle_service_target,
        );
        assert!(!outside_ceiling.binding.affordances.contains_key(VIEW_REF));
        refused_as(
            &outside_ceiling,
            "affordance:view:test/input:run",
            REFUSAL_TARGET_OUTSIDE_SURFACE_CEILING,
        );
    }

    /// A lane admits a target only by naming it. A target that declares no
    /// capability no longer passes a lane that names something else, for any
    /// caller; it passes once the lane names the target's own execute
    /// capability, exactly or by pattern.
    #[test]
    fn a_target_declaring_no_required_caps_passes_only_a_lane_that_names_it() {
        fn no_caps(item_ref: &str) -> Result<CompiledUiTarget> {
            Ok(CompiledUiTarget {
                required_caps: Vec::new(),
                ..bundle_service_target(item_ref)?
            })
        }
        let mut authored = surface(serde_json::json!([]));
        authored["capabilities"] = serde_json::json!({
            "sources": ["cap.unrelated"],
            "affordances": ["cap.unrelated"]
        });
        let unnamed = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedBundle,
            &[],
            &[],
            &authored,
            no_caps,
        );
        assert!(unnamed.binding.sources.is_empty());
        assert!(unnamed.binding.affordances.is_empty());
        assert!(unnamed.binding.surface_route.is_none());
        for coordinate in [
            "source:surface:test/root:shell",
            "source:view:test/input:default",
            "affordance:view:test/input:run",
            "surface_route",
        ] {
            refused_as(&unnamed, coordinate, REFUSAL_TARGET_OUTSIDE_SURFACE_CEILING);
        }

        authored["capabilities"] = serde_json::json!({
            "sources": ["ryeos.execute.service.test/read"],
            "affordances": ["ryeos.execute.service.test/*"]
        });
        let named = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedBundle,
            &[],
            &[],
            &authored,
            no_caps,
        );
        assert!(named.binding.sources[VIEW_REF].contains_key("default"));
        assert!(matches!(
            named.binding.affordances[VIEW_REF]["run"],
            CompiledUiAffordance::Execution { .. }
        ));
        assert!(named.binding.surface_route.is_some());
    }

    /// A view a refused shadow left unresolved stays in the presentation as
    /// a degraded entry carrying the refusal's own words: it is neither
    /// dropped nor replaced by the bundle view, and it has no behaviour.
    #[test]
    fn a_refused_view_reaches_the_presentation_as_a_degraded_entry_with_its_reason() {
        let reason = "project item `view:test/input` at /project/.ai/views/test/input.yaml shadows the item of the same ref supplied by installed bundle `shipper`";
        let mut authored = surface(serde_json::json!(["cap.write"]));
        authored["views"][VIEW_REF] = serde_json::json!({"degraded": reason});
        let compiled = SessionCompiledUiBinding::compile(
            BindingCompileContext {
                contract_revision: "test.v1",
                principal_id: "principal:test",
                caller_scopes: &["cap.read".into(), "cap.write".into()],
                project_root: Some("/project"),
                node_policy_generation_digest: "policy:test",
                project_source_ceiling: &[],
                identities: EmbeddedSurfaceIdentity {
                    request_engine_generation_identity: "engine:test".into(),
                    surface: identity(SURFACE_REF, 'a'),
                    views: BTreeMap::from([(
                        VIEW_REF.to_string(),
                        EmbeddedViewIdentity::Degraded {
                            reason: reason.to_string(),
                        },
                    )]),
                },
            },
            &authored,
            bundle_service_target,
        )
        .unwrap();
        assert!(!compiled.binding.sources.contains_key(VIEW_REF));
        assert!(!compiled.binding.affordances.contains_key(VIEW_REF));
        let presentation = compiled.sanitize_effective_surface(&authored).unwrap();
        assert_eq!(
            presentation["views"][VIEW_REF],
            serde_json::json!({"degraded": reason})
        );
    }

    // ── Real project files ──────────────────────────────────────────────
    //
    // Byte copies of views and surfaces two projects ship, compiled as a
    // project-authored surface binding project-authored views.

    fn fixture(relative: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/project-authored")
            .join(relative);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        serde_yaml::from_str(&text).unwrap_or_else(|error| panic!("parse {relative}: {error}"))
    }

    /// The surface fixture with each view its tiles name embedded from the
    /// same project's fixtures, as composition would embed them.
    fn project_surface_fixture(project: &str, surface: &str) -> Value {
        let mut body = fixture(&format!("{project}/surfaces/{surface}.yaml"));
        let tiles: Vec<String> = body["tiles"]
            .as_array()
            .expect("surface tiles")
            .iter()
            .map(|tile| tile.as_str().expect("tile view ref").to_string())
            .collect();
        for view_ref in tiles {
            let bare = view_ref.strip_prefix("view:").expect("view ref");
            body["views"][&view_ref] = fixture(&format!("{project}/views/{bare}.yaml"));
        }
        body
    }

    fn compile_project_fixture(body: &Value, ceiling: &[String]) -> SessionCompiledUiBinding {
        compile_authored_by(
            TrustClass::TrustedProject,
            TrustClass::TrustedProject,
            &[],
            ceiling,
            body,
            bundle_service_target,
        )
    }

    /// A project view shaped like the real ones compiles and keeps working
    /// when the node lists the field services it reads: every source keeps
    /// its authored parameters, the UI-plane selection affordance is a local
    /// affordance, and both survive into the presentation with the marker.
    #[test]
    fn project_view_with_field_sources_and_a_selection_affordance_compiles_and_works() {
        let view_ref = "view:farm/project/field";
        let body = project_surface_fixture("kaggriculture", "farm/ui/campaign-terminal");
        let compiled = compile_project_fixture(&body, &every_field_service());

        assert!(
            compiled.binding.attenuated.is_empty(),
            "{:?}",
            compiled.binding.attenuated
        );
        let sources = &compiled.binding.sources[view_ref];
        assert_eq!(
            sources.keys().collect::<Vec<_>>(),
            ["execution", "project", "runs"]
        );
        assert_eq!(
            sources["project"].target.identity.canonical_ref,
            FIELD_PROJECT
        );
        assert_eq!(sources["runs"].target.identity.canonical_ref, FIELD_RUNS);
        assert_eq!(
            sources["runs"].parameters,
            serde_json::json!({"item_ref": "graph:farm/match", "limit": 500})
        );
        assert_eq!(
            sources["execution"].parameters["thread_id"],
            "@facet:selection.thread_id"
        );
        assert!(matches!(
            compiled.binding.affordances[view_ref]["select"],
            CompiledUiAffordance::Local { .. }
        ));
        assert!(compiled.binding.surface_route.is_none());
        assert_eq!(compiled.posture, EffectiveUiPosture::ObservationOnly);

        let presentation = compiled.sanitize_effective_surface(&body).unwrap();
        let view = &presentation["views"][view_ref];
        assert_eq!(view["sources"].as_object().unwrap().len(), 3);
        assert_eq!(view["affordances"].as_array().unwrap().len(), 1);
        assert_eq!(view["authorship"]["space"], "project");
        assert_eq!(presentation["authorship"]["trust_class"], "trusted_project");
    }

    fn shipped_project_source_ceiling() -> Vec<String> {
        let path = ryeos_engine::test_support::workspace_root()
            .join("bundles/ryeos-ui/.ai/config/ryeos/ui/project-authoring.yaml");
        let text = std::fs::read_to_string(path).unwrap();
        crate::handlers::ui_launch_mint::ProjectAuthoringConfig::from_value(
            serde_yaml::from_str(&text).unwrap(),
        )
        .unwrap()
        .sources
    }

    /// The list the UI bundle ships names services exactly, and a list with
    /// a wildcard is refused when it is loaded.
    #[test]
    fn shipped_project_source_ceiling_names_each_service_exactly() {
        let shipped = shipped_project_source_ceiling();
        assert_eq!(
            shipped,
            ceiling(&[FIELD_PROJECT, FIELD_DEFINITION, FIELD_RUNS, FIELD_EXECUTION])
        );
        for widened in [
            "ryeos.execute.service.*",
            "ryeos.execute.service.ui/ryeos-ui/field/*",
            "",
        ] {
            let refusal = crate::handlers::ui_launch_mint::ProjectAuthoringConfig::from_value(
                serde_json::json!({"sources": [widened]}),
            )
            .unwrap_err();
            assert!(
                refusal.contains("must name one service exactly"),
                "{refusal}"
            );
        }
    }

    /// Every real project file compiles whole under the shipped list: each
    /// view keeps every source it declares and its selection affordance,
    /// nothing is refused, and every kept source is marked as declared by a
    /// project-authored item, which is what holds its reads to the session's
    /// own project.
    #[test]
    fn real_project_files_compile_whole_against_the_shipped_ceiling() {
        let shipped = shipped_project_source_ceiling();
        for (project, surface) in [
            ("kaggriculture", "farm/ui/campaign-terminal"),
            ("arc-agi-3", "arc3/solve/ui/solve-terminal"),
            ("arc-agi-3", "arc3/solve/ui/solve-web"),
        ] {
            let body = project_surface_fixture(project, surface);
            let compiled = compile_project_fixture(&body, &shipped);
            assert!(
                compiled.binding.attenuated.is_empty(),
                "{surface}: {:?}",
                compiled.binding.attenuated
            );
            let views = body["views"].as_object().expect("embedded views");
            assert!(!views.is_empty(), "{surface} binds views");
            for (view_ref, view) in views {
                for channel in view["sources"].as_object().expect("view sources").keys() {
                    let source = &compiled.binding.sources[view_ref][channel];
                    assert!(source.project_authored, "{view_ref}:{channel}");
                }
                assert!(
                    matches!(
                        compiled.binding.affordances[view_ref]["select"],
                        CompiledUiAffordance::Local { .. }
                    ),
                    "{view_ref}"
                );
            }
        }
    }

    /// The provenance of a source follows its author. A source a
    /// project-authored view declares is marked; the bundle surface's own
    /// source beside it is not, and neither is the same view's source when a
    /// bundle authored the view. A bundle view's source under a
    /// project-authored surface is marked, because that surface composed it.
    #[test]
    fn a_source_is_marked_project_authored_by_its_author_and_its_surface() {
        let authored = surface_with_listed_source_and_selection();
        let project_view = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedProject,
            &["cap.read", "cap.write"],
            &ceiling(&[FIELD_PROJECT]),
            &authored,
            bundle_service_target,
        );
        assert!(project_view.binding.sources[VIEW_REF]["project"].project_authored);
        assert!(!project_view.binding.sources[SURFACE_REF]["shell"].project_authored);

        let bundle_view = compile_authored_by(
            TrustClass::TrustedBundle,
            TrustClass::TrustedBundle,
            &["cap.read", "cap.write"],
            &ceiling(&[FIELD_PROJECT]),
            &authored,
            bundle_service_target,
        );
        assert!(!bundle_view.binding.sources[VIEW_REF]["project"].project_authored);
        assert!(!bundle_view.binding.sources[VIEW_REF]["default"].project_authored);
        // A node-authored source serializes without the field.
        assert!(
            serde_json::to_value(&bundle_view.binding.sources[VIEW_REF]["project"])
                .unwrap()
                .get("project_authored")
                .is_none()
        );

        let project_surface = compile_authored_by(
            TrustClass::TrustedProject,
            TrustClass::TrustedBundle,
            &["cap.read", "cap.write"],
            &ceiling(&[FIELD_PROJECT]),
            &authored,
            bundle_service_target,
        );
        assert!(project_surface.binding.sources[VIEW_REF]["project"].project_authored);
    }

    /// A binding with a project-authored member records the digest of the
    /// list it was compiled under, so a changed or removed list is detected
    /// at revalidation. The digest ignores order, changes with the list, and
    /// is absent from a binding of bundle items, which the list never shapes.
    #[test]
    fn binding_with_a_project_member_records_the_project_ceiling_digest() {
        let authored = surface_with_listed_source_and_selection();
        let compile_under = |view_trust, list: &[String]| {
            compile_authored_by(
                TrustClass::TrustedBundle,
                view_trust,
                &["cap.read", "cap.write"],
                list,
                &authored,
                bundle_service_target,
            )
            .binding
            .project_source_ceiling_digest
        };
        let one = ceiling(&[FIELD_PROJECT]);
        let two = ceiling(&[FIELD_PROJECT, FIELD_RUNS]);
        let two_reordered = ceiling(&[FIELD_RUNS, FIELD_PROJECT]);

        let recorded = compile_under(TrustClass::TrustedProject, &one);
        assert_eq!(recorded, project_source_ceiling_digest(&one).unwrap());
        assert_ne!(recorded, compile_under(TrustClass::TrustedProject, &two));
        assert_ne!(recorded, compile_under(TrustClass::TrustedProject, &[]));
        assert_eq!(
            compile_under(TrustClass::TrustedProject, &two),
            compile_under(TrustClass::TrustedProject, &two_reordered)
        );
        assert_eq!(compile_under(TrustClass::UntrustedProject, &one), recorded);
        assert_eq!(compile_under(TrustClass::TrustedBundle, &one), "");
    }
}
