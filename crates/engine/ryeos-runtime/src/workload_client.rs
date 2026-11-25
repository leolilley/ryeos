//! Closed wire contract for a restricted workload-local RyeOS client.
//!
//! The local endpoint is transport only. It carries no daemon address,
//! signing key, callback bearer, thread-auth bearer, project path, placement,
//! or boot identity. The trusted bridge and daemon recover those from the
//! already-admitted worker boot and reject every request outside its retained
//! grant.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::callback::MethodCall;

pub const WORKLOAD_CLIENT_PROTOCOL: &str = "ryeos.workload-client/v2";
pub const WORKLOAD_CLIENT_CHANNEL_ENV: &str = "RYEOS_WORKLOAD_CLIENT_FD";
/// Fixed sandbox descriptor for the protected daemon-to-bridge boot channel.
///
/// This is protocol vocabulary, not daemon-local placement policy. Keeping it
/// high avoids the compact inherited-authority range normally used by the
/// isolation adapter, while the generic launch validator remains responsible
/// for rejecting any exact descriptor collision.
pub const WORKLOAD_CLIENT_CHANNEL_TARGET_FD: u32 = 198;
pub const WORKLOAD_CLIENT_BROKER_DIRECTORY_NAME: &str = ".ryeos-wc";
pub const WORKLOAD_CLIENT_ENDPOINT_ENV: &str =
    ryeos_engine::protocol_vocabulary::WORKLOAD_CLIENT_ENDPOINT_ENV;
pub const MAX_WORKLOAD_CLIENT_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Boot/ready frames carry bounded public presentation, never execution results.
pub const MAX_WORKLOAD_CLIENT_CONTROL_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_WORKLOAD_CLIENT_PRESENTATION_BYTES: usize = 48 * 1024;
// Closed protocol ceilings, not node defaults. Signed policy may select less.
pub const MAX_WORKLOAD_CLIENT_EXECUTIONS: u16 = 256;
pub const MAX_WORKLOAD_CLIENT_REF_BINDINGS: u16 = 32;
pub const MAX_WORKLOAD_CLIENT_BINDING_VALUES: usize = 32;
pub const MAX_WORKLOAD_CLIENT_CALLS: u16 = 32;
pub const MAX_WORKLOAD_CLIENT_DELEGATION_CAPS: usize = 256;
pub const MAX_WORKLOAD_CLIENT_IN_FLIGHT: u16 = 64;
pub const MAX_WORKLOAD_CLIENT_INVOCATIONS_PER_BOOT: u32 = 1_000_000;
pub const MAX_WORKLOAD_CLIENT_LIFETIME_SECONDS: u64 = 604_800;
pub const MAX_WORKLOAD_CLIENT_ERROR_MESSAGE_BYTES: usize = 2_048;
/// Most bytes of a collected child's result and error together. A larger
/// result is refused with a typed code, never cut to fit.
pub const MAX_WORKLOAD_CLIENT_COLLECT_RESULT_BYTES: usize = 256 * 1024;
/// The one refusal for every collect request outside the grant: an ungranted
/// operation, a child that was never given to this session, a child of another
/// session, a descendant of a child, a child launched by another key, and a
/// thread that does not exist. The message never tells these apart.
pub const WORKLOAD_COLLECT_NOT_PERMITTED_CODE: &str = "workload_collect_not_permitted";
pub const WORKLOAD_COLLECT_NOT_PERMITTED_MESSAGE: &str =
    "the grant does not permit collecting this thread";
/// A permitted collect whose child result exceeds its byte cap.
pub const WORKLOAD_COLLECT_RESULT_TOO_LARGE_CODE: &str = "workload_collect_result_too_large";
/// A permitted collect whose child output contains a secret value the node
/// holds. The output is withheld whole; it is never cut or masked.
pub const WORKLOAD_COLLECT_RESULT_WITHHELD_CODE: &str = "workload_collect_result_withheld";
pub const WORKLOAD_COLLECT_RESULT_WITHHELD_MESSAGE: &str =
    "the child's output matches a secret value the node holds and is withheld";
/// Shortest secret the exact-match floor looks for. A shorter value would
/// match ordinary text by chance, so it is not checked.
pub const MIN_COLLECT_SECRET_FLOOR_BYTES: usize = 8;
const MAX_WORKLOAD_CLIENT_ERROR_CODE_BYTES: usize = 128;
const MAX_WORKLOAD_CLIENT_IDENTIFIER_BYTES: usize = 256;
const MAX_WORKLOAD_CLIENT_BINDING_NAME_BYTES: usize = 128;
// This declaration is retained as one ordinary launch fact, not a transport
// frame. Keep its producer bound within the registry's existing fact ceiling.
pub const MAX_WORKLOAD_CLIENT_REQUEST_CONTRACT_BYTES: u32 =
    ryeos_engine::runtime_registry::MAX_LAUNCH_FACT_BYTES;
pub const MAX_WORKLOAD_CLIENT_REQUEST_ID_BYTES: usize = 128;
pub const WORKLOAD_CLIENT_REQUEST_FACT: &str = "workload_client_request";

/// Project-authored request retained from one exact worker-environment item.
///
/// This is never the live grant. The target intersects these finite routes
/// and bounds with the initiating principal, root delegation ceiling, current
/// node policy, and each resolved child program before minting boot-local
/// callback authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientRequestContract {
    pub protocol: String,
    pub bindings: Vec<WorkloadClientBinding>,
    pub executions: Vec<WorkloadClientExecutionCeiling>,
    pub max_in_flight: u16,
    pub max_invocations_per_boot: u32,
    pub max_lifetime_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientProgram {
    pub realization_id: String,
    pub relative_path: String,
}

/// Selected request ingress, not an execution kind or an authority grant.
/// Both ingress implementations feed the same daemon dispatch owner. In
/// particular, protocol ingress must not fabricate an executable dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkloadClientBinding {
    Cli { program: WorkloadClientProgram },
    StructuredSession {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadClientIngress {
    Cli,
    StructuredSession,
}

impl WorkloadClientBinding {
    pub fn ingress(&self) -> WorkloadClientIngress {
        match self {
            Self::Cli { .. } => WorkloadClientIngress::Cli,
            Self::StructuredSession {} => WorkloadClientIngress::StructuredSession,
        }
    }
}

pub fn validate_workload_client_ingresses(
    ingresses: &[WorkloadClientIngress],
) -> anyhow::Result<()> {
    if ingresses.is_empty()
        || ingresses.len() > 2
        || ingresses.windows(2).any(|pair| pair[0] >= pair[1])
    {
        anyhow::bail!("workload-client ingresses must be nonempty, sorted and unique");
    }
    Ok(())
}

/// Only a trusted ingress may assign this namespace. An untrusted request ID
/// (including a string shaped like another ingress's ID) is always input to
/// this projection, never an already-normalized identity. Behavior is excluded:
/// existing dispatch admission detects changed behavior under one occurrence.
pub fn ingress_request_id(
    ingress: WorkloadClientIngress,
    external_request_id: &str,
) -> anyhow::Result<String> {
    validate_request_id(external_request_id)?;
    let coordinate = lillux::canonical_json(&serde_json::json!({
        "ingress": ingress,
        "request_id": external_request_id,
    }))?;
    Ok(format!("wc-{}", lillux::sha256_hex(coordinate.as_bytes())))
}

/// One exact child route requested by the project environment.
///
/// Ref-binding values are finite allow-lists rather than wildcard patterns.
/// The selected child's own kind schema remains authoritative for whether a
/// binding is legal and for its parameter/method schemas, effects, program,
/// content, and isolation requirements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientExecutionCeiling {
    pub item_ref: String,
    pub ref_bindings: BTreeMap<String, Vec<String>>,
    pub calls: Vec<WorkloadClientCallCeiling>,
    pub effect_classes: Vec<String>,
    /// Exact workspace relationship expected from the resolved child's signed
    /// kind projection. This is an assertion and narrowing boundary, never an
    /// alternative grant authored by the worker environment.
    pub workspace_access: ryeos_engine::kind_registry::WorkspaceAccess,
    /// Whether the session may launch this item as a detached child. Absent
    /// means false. The effect classes and workspace access above are the
    /// ceiling a detached child's own calls are bound by, sealed with it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    /// Whether the session may collect the result of a detached child it
    /// launched from this item. Absent means false, and it is meaningful only
    /// with `detached`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collect: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkloadClientCallCeiling {
    Default,
    Method { name: String },
}

/// Public, non-authorizing projection of one admitted execution ceiling.
///
/// The retained ceiling uses a tagged enum because it is authority data.  A
/// worker must never be shown that internal representation as if it were an
/// invocation example: `{ "kind": "default" }` is not a valid
/// [`MethodCall`].  Keep the model/client-facing description in the actual
/// request grammar while the daemon retains and enforces the full ceiling.
pub fn execution_ceiling_presentation(ceiling: &WorkloadClientExecutionCeiling) -> Value {
    let calls = ceiling
        .calls
        .iter()
        .map(|call| match call {
            WorkloadClientCallCeiling::Default => Value::Null,
            WorkloadClientCallCeiling::Method { name } => {
                serde_json::json!({"method": name})
            }
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "item_ref": ceiling.item_ref,
        "allowed_ref_bindings": ceiling.ref_bindings,
        "calls": calls,
        "effect_classes": ceiling.effect_classes,
        "workspace_access": ceiling.workspace_access,
    })
}

/// Compile root-launch selectors for the finite child operations exposed by a
/// workload-client request. This is structural routing only: ordinary product
/// admission still verifies every witness, relationship, binding and manifest
/// against the destination node when the child is prepared.
pub fn admitted_execution_product_selections(
    inputs: &[ryeos_state::external_content::products::composition::ProductSelectionInput],
    request: Option<&WorkloadClientRequestContract>,
) -> anyhow::Result<
    BTreeMap<String, ryeos_state::external_content::products::composition::ProductSelectionInputs>,
> {
    use ryeos_state::external_content::products::composition::{
        ProductSelectionInput, ProductSelectionTarget, canonicalize_product_selection_inputs,
    };

    let mut admitted = BTreeMap::<String, Vec<ProductSelectionInput>>::new();
    for input in inputs {
        let ProductSelectionTarget::WorkloadExecution { item_ref } = &input.target else {
            continue;
        };
        let request = request.ok_or_else(|| {
            anyhow::anyhow!("product selector targets a disabled workload-client surface")
        })?;
        if request
            .executions
            .binary_search_by(|entry| entry.item_ref.as_str().cmp(item_ref))
            .is_err()
        {
            anyhow::bail!(
                "product selector targets an execution outside the workload-client request"
            );
        }
        admitted
            .entry(item_ref.clone())
            .or_default()
            .push(ProductSelectionInput {
                target: ProductSelectionTarget::Root {},
                selection: input.selection.clone(),
            });
    }
    admitted
        .into_iter()
        .map(|(item_ref, selections)| {
            Ok((item_ref, canonicalize_product_selection_inputs(selections)?))
        })
        .collect()
}

/// Target-local execution-policy ceiling. A null policy disables the feature.
/// Values here bound project requests mechanically; they never add item,
/// principal, project, effect, or child-program authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientNodePolicy {
    pub protocol: String,
    pub delegation_cap_ceiling: Vec<String>,
    pub allowed_effect_classes: Vec<String>,
    pub allowed_workspace_access: Vec<ryeos_engine::kind_registry::WorkspaceAccess>,
    pub max_executions: u16,
    pub max_ref_bindings_per_execution: u16,
    pub max_calls_per_execution: u16,
    pub max_in_flight: u16,
    pub max_invocations_per_boot: u32,
    pub max_lifetime_seconds: u64,
    pub max_request_bytes: u32,
}

impl WorkloadClientRequestContract {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.protocol != WORKLOAD_CLIENT_PROTOCOL {
            anyhow::bail!("workload-client request protocol is not current");
        }
        if self.bindings.is_empty() || self.bindings.len() > 2 {
            anyhow::bail!("workload-client request must select a finite ingress set");
        }
        let mut previous = None;
        for binding in &self.bindings {
            let ingress = binding.ingress();
            if previous.is_some_and(|prior| prior >= ingress) {
                anyhow::bail!("workload-client bindings must be sorted and unique");
            }
            previous = Some(ingress);
            if let WorkloadClientBinding::Cli { program } = binding {
                validate_identifier("workload-client realization id", &program.realization_id)?;
                ryeos_state::objects::validate_session_process_environment_relative_path(
                    &program.relative_path,
                )?;
                if std::path::Path::new(&program.relative_path)
                    .file_name()
                    .is_none()
                {
                    anyhow::bail!("workload-client program path has no executable member");
                }
            }
        }
        validate_execution_ceilings(&self.executions)?;
        validate_workload_client_limits(
            self.max_in_flight,
            self.max_invocations_per_boot,
            self.max_lifetime_seconds,
        )?;
        if serde_json::to_vec(self)?.len() > MAX_WORKLOAD_CLIENT_REQUEST_CONTRACT_BYTES as usize {
            anyhow::bail!("workload-client project request exceeds its retained fact bound");
        }
        Ok(())
    }

    pub fn cli_program(&self) -> Option<&WorkloadClientProgram> {
        self.bindings.iter().find_map(|binding| match binding {
            WorkloadClientBinding::Cli { program } => Some(program),
            WorkloadClientBinding::StructuredSession {} => None,
        })
    }

    pub fn has_ingress(&self, ingress: WorkloadClientIngress) -> bool {
        self.bindings
            .iter()
            .any(|binding| binding.ingress() == ingress)
    }
}

/// Validate the exact finite execution projection independently from client
/// program selection. Live admitted grants retain this projection after the
/// program has been resolved into an inherited executable authority; they
/// must not fabricate a placeholder program merely to reuse validation.
pub fn validate_execution_ceilings(
    executions: &[WorkloadClientExecutionCeiling],
) -> anyhow::Result<()> {
    if executions.is_empty() || executions.len() > usize::from(MAX_WORKLOAD_CLIENT_EXECUTIONS) {
        anyhow::bail!("workload-client request has no finite execution surface");
    }
    let mut previous_item: Option<&str> = None;
    for execution in executions {
        ryeos_engine::canonical_ref::CanonicalRef::parse(&execution.item_ref).map_err(|error| {
            anyhow::anyhow!("workload-client execution ref is invalid: {error}")
        })?;
        if previous_item.is_some_and(|previous| previous >= execution.item_ref.as_str()) {
            anyhow::bail!("workload-client execution refs must be sorted and unique");
        }
        previous_item = Some(&execution.item_ref);
        if execution.ref_bindings.len() > usize::from(MAX_WORKLOAD_CLIENT_REF_BINDINGS) {
            anyhow::bail!("workload-client execution exceeds its ref-binding bound");
        }
        for (name, refs) in &execution.ref_bindings {
            validate_binding_name(name)?;
            if refs.is_empty() || refs.len() > MAX_WORKLOAD_CLIENT_BINDING_VALUES {
                anyhow::bail!("workload-client ref binding `{name}` has no finite value surface");
            }
            let mut previous: Option<&str> = None;
            for item_ref in refs {
                ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref).map_err(|error| {
                    anyhow::anyhow!("workload-client ref binding `{name}` is invalid: {error}")
                })?;
                if previous.is_some_and(|prior| prior >= item_ref.as_str()) {
                    anyhow::bail!(
                        "workload-client ref binding `{name}` values must be sorted and unique"
                    );
                }
                previous = Some(item_ref);
            }
        }
        if execution.calls.is_empty()
            || execution.calls.len() > usize::from(MAX_WORKLOAD_CLIENT_CALLS)
        {
            anyhow::bail!("workload-client execution has no finite call surface");
        }
        let mut previous_call: Option<&WorkloadClientCallCeiling> = None;
        for call in &execution.calls {
            if previous_call.is_some_and(|previous| previous >= call) {
                anyhow::bail!("workload-client calls must be sorted and unique");
            }
            if let WorkloadClientCallCeiling::Method { name } = call {
                validate_identifier("workload-client method", name)?;
            }
            previous_call = Some(call);
        }
        validate_effect_classes(&execution.effect_classes, false)?;
        if execution.collect && !execution.detached {
            anyhow::bail!(
                "workload-client execution `{}` allows collecting a detached child it cannot launch",
                execution.item_ref
            );
        }
    }
    Ok(())
}

impl WorkloadClientNodePolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.protocol != WORKLOAD_CLIENT_PROTOCOL {
            anyhow::bail!("node workload-client protocol is not current");
        }
        validate_sorted_strings(
            "node workload-client delegation capability ceiling",
            &self.delegation_cap_ceiling,
            MAX_WORKLOAD_CLIENT_DELEGATION_CAPS,
        )?;
        for capability in &self.delegation_cap_ceiling {
            crate::authorizer::validate_scope_pattern(capability).map_err(anyhow::Error::msg)?;
            if !capability.starts_with("ryeos.execute.") {
                anyhow::bail!(
                    "node workload-client ceiling may contain only execution capabilities"
                );
            }
        }
        validate_effect_classes(&self.allowed_effect_classes, true)?;
        if self.allowed_workspace_access.is_empty()
            || self
                .allowed_workspace_access
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            anyhow::bail!(
                "node workload-client workspace-access ceiling must be sorted and unique"
            );
        }
        if self.max_executions == 0
            || self.max_executions > MAX_WORKLOAD_CLIENT_EXECUTIONS
            || self.max_ref_bindings_per_execution > MAX_WORKLOAD_CLIENT_REF_BINDINGS
            || self.max_calls_per_execution == 0
            || self.max_calls_per_execution > MAX_WORKLOAD_CLIENT_CALLS
            || self.max_request_bytes == 0
            || self.max_request_bytes as usize > MAX_WORKLOAD_CLIENT_FRAME_BYTES
        {
            anyhow::bail!("node workload-client structural bounds are not canonical");
        }
        validate_workload_client_limits(
            self.max_in_flight,
            self.max_invocations_per_boot,
            self.max_lifetime_seconds,
        )
    }

    pub fn admit_request(&self, request: &WorkloadClientRequestContract) -> anyhow::Result<()> {
        self.validate()?;
        request.validate()?;
        if request.protocol != self.protocol
            || request.executions.len() > usize::from(self.max_executions)
            || request.max_in_flight > self.max_in_flight
            || request.max_invocations_per_boot > self.max_invocations_per_boot
            || request.max_lifetime_seconds > self.max_lifetime_seconds
        {
            anyhow::bail!("workload-client request exceeds target node policy");
        }
        for execution in &request.executions {
            if execution.detached {
                anyhow::bail!(
                    "workload-client execution `{}` asks to be detached, which this node does not admit",
                    execution.item_ref
                );
            }
            if execution.ref_bindings.len() > usize::from(self.max_ref_bindings_per_execution)
                || execution.calls.len() > usize::from(self.max_calls_per_execution)
                || execution
                    .effect_classes
                    .iter()
                    .any(|class| self.allowed_effect_classes.binary_search(class).is_err())
                || self
                    .allowed_workspace_access
                    .binary_search(&execution.workspace_access)
                    .is_err()
            {
                anyhow::bail!("workload-client execution exceeds target node policy");
            }
        }
        Ok(())
    }
}

pub fn validate_workload_client_limits(
    max_in_flight: u16,
    max_invocations_per_boot: u32,
    max_lifetime_seconds: u64,
) -> anyhow::Result<()> {
    if max_in_flight == 0
        || max_in_flight > MAX_WORKLOAD_CLIENT_IN_FLIGHT
        || max_invocations_per_boot == 0
        || max_invocations_per_boot > MAX_WORKLOAD_CLIENT_INVOCATIONS_PER_BOOT
        || max_lifetime_seconds == 0
        || max_lifetime_seconds > MAX_WORKLOAD_CLIENT_LIFETIME_SECONDS
    {
        anyhow::bail!("workload-client lifetime or invocation bounds are not canonical");
    }
    Ok(())
}

fn validate_identifier(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > MAX_WORKLOAD_CLIENT_IDENTIFIER_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
        })
    {
        anyhow::bail!("{label} is not canonical");
    }
    Ok(())
}

fn validate_binding_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name.len() > MAX_WORKLOAD_CLIENT_BINDING_NAME_BYTES
        || name.bytes().any(|byte| {
            byte.is_ascii_control() || byte.is_ascii_whitespace() || matches!(byte, b'/' | b'=')
        })
    {
        anyhow::bail!("workload-client ref-binding name is not canonical");
    }
    Ok(())
}

fn validate_sorted_strings(label: &str, values: &[String], max: usize) -> anyhow::Result<()> {
    if values.len() > max {
        anyhow::bail!("{label} exceeds its bound");
    }
    let mut previous: Option<&str> = None;
    for value in values {
        if value.is_empty()
            || value.len() > MAX_WORKLOAD_CLIENT_IDENTIFIER_BYTES
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            || previous.is_some_and(|prior| prior >= value.as_str())
        {
            anyhow::bail!("{label} must be bounded, sorted, and unique");
        }
        previous = Some(value);
    }
    Ok(())
}

fn validate_effect_classes(values: &[String], allow_empty: bool) -> anyhow::Result<()> {
    if (!allow_empty && values.is_empty()) || values.len() > 3 {
        anyhow::bail!("workload-client effect-class ceiling is not finite");
    }
    validate_sorted_strings("workload-client effect-class ceiling", values, 3)?;
    if values
        .iter()
        .any(|value| !matches!(value.as_str(), "live" | "recorded" | "sealed"))
    {
        anyhow::bail!("workload-client effect-class ceiling is not canonical");
    }
    Ok(())
}

/// Secret-free daemon-to-bridge boot contract carried only on the protected
/// target channel. The grant digest names retained daemon authority; it does
/// not delegate that authority to the bridge-local endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientBootFrame {
    pub protocol: String,
    pub grant_digest: String,
    pub ingresses: Vec<WorkloadClientIngress>,
    pub execution_presentation: Value,
    pub max_in_flight: u16,
    pub max_request_bytes: u32,
    pub max_lifetime_seconds: u64,
}

impl WorkloadClientBootFrame {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_workload_client_ingresses(&self.ingresses)?;
        validate_execution_presentation(&self.execution_presentation)?;
        if self.protocol != WORKLOAD_CLIENT_PROTOCOL
            || !lillux::valid_hash(&self.grant_digest)
            || self.max_in_flight == 0
            || self.max_in_flight > MAX_WORKLOAD_CLIENT_IN_FLIGHT
            || self.max_request_bytes == 0
            || self.max_request_bytes as usize > MAX_WORKLOAD_CLIENT_FRAME_BYTES
            || self.max_lifetime_seconds == 0
            || self.max_lifetime_seconds > MAX_WORKLOAD_CLIENT_LIFETIME_SECONDS
        {
            anyhow::bail!("workload-client boot frame is not canonical");
        }
        Ok(())
    }
}

/// Mechanical transport ceiling for a projection derived by the existing
/// inventory/resolution owner. It cannot authorize an operation.
pub fn validate_execution_presentation(value: &Value) -> anyhow::Result<()> {
    let items = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("workload presentation must be an array"))?;
    if items.is_empty()
        || items.len() > usize::from(MAX_WORKLOAD_CLIENT_EXECUTIONS)
        || items.iter().any(|item| !item.is_object())
        || serde_json::to_vec(value)?.len() > MAX_WORKLOAD_CLIENT_PRESENTATION_BYTES
    {
        anyhow::bail!("workload presentation exceeds its closed count/byte bounds");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientReadyFrame {
    pub protocol: String,
    pub grant_digest: String,
}

impl WorkloadClientReadyFrame {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.protocol != WORKLOAD_CLIENT_PROTOCOL || !lillux::valid_hash(&self.grant_digest) {
            anyhow::bail!("workload-client ready frame is not canonical");
        }
        Ok(())
    }
}

/// One workload request. Only ordinary item execution is in the v1 grammar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientExecuteRequest {
    pub item_ref: String,
    pub ref_bindings: BTreeMap<String, String>,
    #[serde(default)]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<MethodCall>,
}

/// Read the result of one detached child this session launched. The id is
/// the only input; which children a session may collect is decided by the
/// daemon from its own records, never from this frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientCollectRequest {
    pub child_thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "method",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkloadClientOperation {
    Execute(WorkloadClientExecuteRequest),
    Collect(WorkloadClientCollectRequest),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientRequestFrame {
    pub protocol: String,
    pub request_id: String,
    pub operation: WorkloadClientOperation,
}

/// Trusted-ingress provenance on the protected channel, never accepted by
/// the workload-local CLI wire grammar. This is retained with the existing
/// action intent, not in a transport-owned replay database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkloadInvocationSource {
    Cli {
        external_request_id: String,
    },
    StructuredSession {
        upstream_session_id: String,
        operation_id: String,
        call_id: String,
    },
}

impl WorkloadInvocationSource {
    /// The single operation-coordinate derivation shared by live admission
    /// and durable intent decoding. Behavior is checked separately by the
    /// action request digest; it must not create another occurrence.
    pub fn runtime_operation_id(&self, grant_digest: &str) -> anyhow::Result<String> {
        if !lillux::valid_hash(grant_digest) {
            anyhow::bail!("workload occurrence grant digest is not canonical");
        }
        let value =
            serde_json::json!({"grant_digest":grant_digest,"request_id":self.request_id()?});
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&value)?.as_bytes(),
        ))
    }

    pub fn ingress(&self) -> WorkloadClientIngress {
        match self {
            Self::Cli { .. } => WorkloadClientIngress::Cli,
            Self::StructuredSession { .. } => WorkloadClientIngress::StructuredSession,
        }
    }

    pub fn request_id(&self) -> anyhow::Result<String> {
        let external = match self {
            Self::Cli {
                external_request_id,
            } => {
                validate_request_id(external_request_id)?;
                external_request_id.clone()
            }
            Self::StructuredSession {
                upstream_session_id,
                operation_id,
                call_id,
            } => {
                for value in [upstream_session_id, operation_id, call_id] {
                    if value.is_empty()
                        || value.len() > MAX_WORKLOAD_CLIENT_IDENTIFIER_BYTES
                        || value.chars().any(char::is_control)
                    {
                        anyhow::bail!("workload invocation correlation is not bounded text");
                    }
                }
                let canonical = lillux::canonical_json(&serde_json::to_value(self)?)?;
                lillux::sha256_hex(canonical.as_bytes())
            }
        };
        ingress_request_id(self.ingress(), &external)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientDispatchFrame {
    pub source: WorkloadInvocationSource,
    pub request: WorkloadClientRequestFrame,
}

impl WorkloadClientDispatchFrame {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.request.validate()?;
        if self.request.request_id != self.source.request_id()? {
            anyhow::bail!("workload invocation contradicts its trusted ingress identity");
        }
        if serde_json::to_vec(self)?.len() > MAX_WORKLOAD_CLIENT_FRAME_BYTES {
            anyhow::bail!("workload invocation exceeds protected frame limit");
        }
        Ok(())
    }
}

impl WorkloadClientRequestFrame {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.protocol != WORKLOAD_CLIENT_PROTOCOL {
            anyhow::bail!("workload-client protocol is not current");
        }
        validate_request_id(&self.request_id)?;
        let request = match &self.operation {
            WorkloadClientOperation::Execute(request) => request,
            WorkloadClientOperation::Collect(collect) => {
                crate::callback::validate_runtime_thread_id(&collect.child_thread_id)
                    .map_err(|error| anyhow::anyhow!("workload-client collect: {error}"))?;
                let encoded = serde_json::to_vec(self)?;
                if encoded.is_empty() || encoded.len() > MAX_WORKLOAD_CLIENT_FRAME_BYTES {
                    anyhow::bail!("workload-client request exceeds its byte bound");
                }
                return Ok(());
            }
        };
        ryeos_engine::canonical_ref::CanonicalRef::parse(&request.item_ref)
            .map_err(|error| anyhow::anyhow!("workload-client item ref is invalid: {error}"))?;
        if request.ref_bindings.len() > usize::from(MAX_WORKLOAD_CLIENT_REF_BINDINGS) {
            anyhow::bail!("workload-client request exceeds its ref-binding bound");
        }
        for (name, item_ref) in &request.ref_bindings {
            validate_binding_name(name)?;
            ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref).map_err(|error| {
                anyhow::anyhow!("workload-client ref binding `{name}` is invalid: {error}")
            })?;
        }
        let encoded = serde_json::to_vec(self)?;
        if encoded.is_empty() || encoded.len() > MAX_WORKLOAD_CLIENT_FRAME_BYTES {
            anyhow::bail!("workload-client request exceeds its byte bound");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkloadClientOutcome {
    Dispatched {
        response: crate::callback_contract::CallbackDispatchResponse,
    },
    Collected {
        child: WorkloadClientCollectedChild,
    },
    Failed {
        code: String,
        message: String,
        retryable: bool,
    },
}

/// What a session may read of a detached child it launched: its status and,
/// once terminal, its result. This is a fixed allow-list of the thread's own
/// status and result record. It carries no project path, authority, capsule,
/// lineage, metadata or artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientCollectedChild {
    pub thread_id: String,
    pub status: String,
    pub kind: String,
    pub item_ref: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// Present only once the child is terminal and has a recorded result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<WorkloadClientCollectedResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientCollectedResult {
    pub outcome_code: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

impl WorkloadClientCollectedChild {
    /// Whether the child's result and error fit the per-call byte cap.
    pub fn result_within_cap(&self) -> anyhow::Result<bool> {
        let Some(result) = &self.result else {
            return Ok(true);
        };
        let size =
            serde_json::to_vec(&result.result)?.len() + serde_json::to_vec(&result.error)?.len();
        Ok(size <= MAX_WORKLOAD_CLIENT_COLLECT_RESULT_BYTES)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        crate::callback::validate_runtime_thread_id(&self.thread_id)
            .map_err(|error| anyhow::anyhow!("workload-client collected child: {error}"))?;
        if !self.result_within_cap()? {
            anyhow::bail!("workload-client collected result exceeds its byte cap");
        }
        Ok(())
    }
}

impl WorkloadClientOutcome {
    /// Dispatch returning JSON is not proof that a child test succeeded.
    /// Reuse the canonical envelope classifier and daemon-owned thread status;
    /// never maintain a second list of kind-specific result conventions here.
    pub fn succeeded(&self) -> bool {
        match self {
            Self::Dispatched { response } => response.execution_succeeded(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadClientResponseFrame {
    pub protocol: String,
    pub request_id: String,
    pub outcome: WorkloadClientOutcome,
}

impl WorkloadClientResponseFrame {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.protocol != WORKLOAD_CLIENT_PROTOCOL {
            anyhow::bail!("workload-client response protocol is not current");
        }
        validate_request_id(&self.request_id)?;
        if let WorkloadClientOutcome::Dispatched { response } = &self.outcome {
            response.dispatch.validate()?;
        }
        if let WorkloadClientOutcome::Collected { child } = &self.outcome {
            child.validate()?;
        }
        if let WorkloadClientOutcome::Failed { code, message, .. } = &self.outcome
            && (code.is_empty()
                || code.len() > MAX_WORKLOAD_CLIENT_ERROR_CODE_BYTES
                || !code.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
                || message.len() > MAX_WORKLOAD_CLIENT_ERROR_MESSAGE_BYTES
                || message.bytes().any(|byte| byte == 0))
        {
            anyhow::bail!("workload-client error is not canonical");
        }
        let encoded = serde_json::to_vec(self)?;
        if encoded.is_empty() || encoded.len() > MAX_WORKLOAD_CLIENT_FRAME_BYTES {
            anyhow::bail!("workload-client response exceeds its byte bound");
        }
        Ok(())
    }
}

pub fn validate_request_id(request_id: &str) -> anyhow::Result<()> {
    if request_id.is_empty()
        || request_id.len() > MAX_WORKLOAD_CLIENT_REQUEST_ID_BYTES
        || !request_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        anyhow::bail!("workload-client request id is not canonical");
    }
    Ok(())
}

/// Produce error text accepted by the wire validator. The ceiling counts
/// UTF-8 bytes, not characters; never split a code point or retain NUL.
/// The exact-match secret floor on a child's output.
///
/// The child's output is the disclosure surface of a collect: whatever the
/// child printed, the session can read. This is a floor under that surface,
/// not a guarantee. It refuses output that contains, byte for byte, a secret
/// value of at least [`MIN_COLLECT_SECRET_FLOOR_BYTES`] bytes: in the raw
/// text of any string or key, in the compact serialized JSON (which catches a
/// number and the JSON-escaped form of the value), and in the ASCII-escaped
/// `\uXXXX` form of the value. It does not catch a secret that was encoded,
/// hashed, split across fields or shortened, nor one that was never in
/// `secrets`.
pub fn value_contains_secret<S: AsRef<str>>(value: &Value, secrets: &[S]) -> bool {
    let needles: Vec<String> = secrets
        .iter()
        .map(AsRef::as_ref)
        .filter(|secret| secret.len() >= MIN_COLLECT_SECRET_FLOOR_BYTES)
        .flat_map(secret_text_forms)
        .collect();
    if needles.is_empty() {
        return false;
    }
    let contains_any = |text: &str| needles.iter().any(|needle| text.contains(needle.as_str()));
    contains_any(&value.to_string()) || any_text(value, &contains_any)
}

/// The raw secret plus the JSON-escaped and ASCII-escaped forms of it.
fn secret_text_forms(secret: &str) -> Vec<String> {
    let mut forms = vec![secret.to_owned()];
    if let Ok(quoted) = serde_json::to_string(secret) {
        forms.push(quoted[1..quoted.len() - 1].to_owned());
    }
    if !secret.is_ascii() {
        let mut lower = String::new();
        let mut upper = String::new();
        for character in secret.chars() {
            if character.is_ascii() {
                lower.push(character);
                upper.push(character);
            } else {
                let mut units = [0u16; 2];
                for unit in character.encode_utf16(&mut units).iter() {
                    lower.push_str(&format!("\\u{unit:04x}"));
                    upper.push_str(&format!("\\u{unit:04X}"));
                }
            }
        }
        forms.push(lower);
        forms.push(upper);
    }
    forms.sort();
    forms.dedup();
    forms
}

fn any_text(value: &Value, test: &dyn Fn(&str) -> bool) -> bool {
    match value {
        Value::String(text) => test(text),
        Value::Array(items) => items.iter().any(|item| any_text(item, test)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, field)| test(key) || any_text(field, test)),
        _ => false,
    }
}

pub fn bounded_error_message(message: &str) -> String {
    let mut result = String::new();
    for character in message.chars().filter(|character| *character != '\0') {
        if result.len() + character.len_utf8() > MAX_WORKLOAD_CLIENT_ERROR_MESSAGE_BYTES {
            break;
        }
        result.push(character);
    }
    result
}

pub fn read_frame<R: std::io::Read, T: serde::de::DeserializeOwned>(
    reader: &mut R,
) -> anyhow::Result<T> {
    read_frame_bounded(reader, MAX_WORKLOAD_CLIENT_FRAME_BYTES)
}

/// Read one length-prefixed frame without allocating beyond the caller's
/// already-admitted byte ceiling.
pub fn read_frame_bounded<R: std::io::Read, T: serde::de::DeserializeOwned>(
    reader: &mut R,
    max_bytes: usize,
) -> anyhow::Result<T> {
    if max_bytes == 0 || max_bytes > MAX_WORKLOAD_CLIENT_FRAME_BYTES {
        anyhow::bail!("workload-client frame ceiling is outside the protocol bound");
    }
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > max_bytes {
        anyhow::bail!("workload-client frame exceeds its byte bound");
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(anyhow::Error::from)
}

pub fn write_frame<W: std::io::Write, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> anyhow::Result<()> {
    write_frame_bounded(writer, value, MAX_WORKLOAD_CLIENT_FRAME_BYTES)
}

pub fn write_frame_bounded<W: std::io::Write, T: Serialize>(
    writer: &mut W,
    value: &T,
    max_bytes: usize,
) -> anyhow::Result<()> {
    if max_bytes == 0 || max_bytes > MAX_WORKLOAD_CLIENT_FRAME_BYTES {
        anyhow::bail!("workload-client frame ceiling is outside the protocol bound");
    }
    let encoded = serde_json::to_vec(value)?;
    if encoded.is_empty() || encoded.len() > max_bytes {
        anyhow::bail!("workload-client frame exceeds its byte bound");
    }
    writer.write_all(&(encoded.len() as u32).to_be_bytes())?;
    writer.write_all(&encoded)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution(item_ref: &str) -> WorkloadClientExecutionCeiling {
        WorkloadClientExecutionCeiling {
            item_ref: item_ref.to_owned(),
            ref_bindings: BTreeMap::new(),
            calls: vec![WorkloadClientCallCeiling::Default],
            effect_classes: vec!["live".to_owned()],
            workspace_access:
                ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
            detached: false,
            collect: false,
        }
    }

    fn request(executions: Vec<WorkloadClientExecutionCeiling>) -> WorkloadClientRequestContract {
        WorkloadClientRequestContract {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            bindings: vec![WorkloadClientBinding::Cli {
                program: WorkloadClientProgram {
                    realization_id: "ryeos-workload-client".to_owned(),
                    relative_path: "bin/ryeos".to_owned(),
                },
            }],
            executions,
            max_in_flight: 2,
            max_invocations_per_boot: 8,
            max_lifetime_seconds: 300,
        }
    }

    #[test]
    fn execution_presentation_uses_the_callable_wire_shape() {
        let mut ceiling = execution("tool:project/check");
        ceiling.calls.push(WorkloadClientCallCeiling::Method {
            name: "inspect".to_owned(),
        });
        let presented = execution_ceiling_presentation(&ceiling);
        assert_eq!(
            presented["calls"],
            serde_json::json!([null, {"method":"inspect"}])
        );
        assert!(
            !lillux::canonical_json(&presented)
                .unwrap()
                .contains("\"kind\":\"default\""),
            "internal authority variants must never be advertised as invocation input"
        );
    }

    #[test]
    fn workload_product_selectors_require_and_normalize_an_exact_child() {
        use ryeos_state::external_content::products::composition::{
            ProductSelection, ProductSelectionInput, ProductSelectionTarget,
        };
        let request = request(vec![execution("tool:project/check")]);
        let selection = ProductSelection {
            declaration_id: "platform".to_owned(),
            witness_hash: "a".repeat(64),
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            qualification_hash: Some("b".repeat(64)),
            remote_testimony: None,
        };
        let input = ProductSelectionInput {
            target: ProductSelectionTarget::WorkloadExecution {
                item_ref: "tool:project/check".to_owned(),
            },
            selection: selection.clone(),
        };
        let admitted =
            admitted_execution_product_selections(std::slice::from_ref(&input), Some(&request))
                .unwrap();
        assert_eq!(
            admitted["tool:project/check"],
            vec![ProductSelectionInput {
                target: ProductSelectionTarget::Root {},
                selection,
            }]
        );
        assert!(admitted_execution_product_selections(std::slice::from_ref(&input), None).is_err());

        let mut outside = input;
        outside.target = ProductSelectionTarget::WorkloadExecution {
            item_ref: "tool:project/other".to_owned(),
        };
        assert!(admitted_execution_product_selections(&[outside], Some(&request)).is_err());
    }

    #[test]
    fn project_request_is_generic_over_canonical_item_kinds() {
        request(vec![
            execution("directive:project/check"),
            execution("tool:project/format"),
        ])
        .validate()
        .unwrap();
    }

    #[test]
    fn ingress_selection_is_explicit_and_protocol_needs_no_program() {
        let mut selected = request(vec![execution("tool:project/check")]);
        selected.bindings = vec![WorkloadClientBinding::StructuredSession {}];
        selected.validate().unwrap();
        assert!(selected.cli_program().is_none());
        assert!(selected.has_ingress(WorkloadClientIngress::StructuredSession));
        selected
            .bindings
            .insert(0, request(selected.executions.clone()).bindings.remove(0));
        selected.validate().unwrap();
        assert!(selected.cli_program().is_some());
        selected.bindings.reverse();
        assert!(selected.validate().is_err());
        selected.bindings = vec![WorkloadClientBinding::StructuredSession {}; 2];
        assert!(selected.validate().is_err());
        selected.bindings.clear();
        assert!(selected.validate().is_err());
    }

    #[test]
    fn binding_decode_refuses_implicit_or_competing_program_authority() {
        let selected = request(vec![execution("tool:project/check")]);
        let mut value = serde_json::to_value(&selected).unwrap();
        value.as_object_mut().unwrap().remove("bindings");
        assert!(serde_json::from_value::<WorkloadClientRequestContract>(value.clone()).is_err());
        value["client"] =
            serde_json::json!({"realization_id":"unused", "relative_path":"bin/ryeos"});
        assert!(serde_json::from_value::<WorkloadClientRequestContract>(value).is_err());
        assert!(
            serde_json::from_value::<WorkloadClientBinding>(serde_json::json!({
                "kind":"structured_session", "program":{"realization_id":"unused"}
            }))
            .is_err()
        );
    }

    #[test]
    fn trusted_ingress_projection_separates_adversarial_external_ids() {
        let cli = ingress_request_id(WorkloadClientIngress::Cli, "same-id").unwrap();
        let protocol =
            ingress_request_id(WorkloadClientIngress::StructuredSession, "same-id").unwrap();
        assert_ne!(cli, protocol);
        assert_eq!(
            cli,
            ingress_request_id(WorkloadClientIngress::Cli, "same-id").unwrap()
        );
        assert_ne!(
            protocol,
            ingress_request_id(WorkloadClientIngress::Cli, &protocol).unwrap()
        );
        assert!(ingress_request_id(WorkloadClientIngress::Cli, "").is_err());
    }

    #[test]
    fn workload_wire_cannot_assert_protected_ingress_provenance() {
        let source = WorkloadInvocationSource::StructuredSession {
            upstream_session_id: "session".to_owned(),
            operation_id: "turn".to_owned(),
            call_id: "call".to_owned(),
        };
        let request = WorkloadClientRequestFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            request_id: source.request_id().unwrap(),
            operation: WorkloadClientOperation::Execute(WorkloadClientExecuteRequest {
                item_ref: "tool:fixture/check".to_owned(),
                ref_bindings: BTreeMap::new(),
                params: serde_json::json!({}),
                call: None,
            }),
        };
        let protected = WorkloadClientDispatchFrame {
            source: source.clone(),
            request: request.clone(),
        };
        protected.validate().unwrap();
        assert!(
            serde_json::from_value::<WorkloadClientRequestFrame>(
                serde_json::to_value(&protected).unwrap()
            )
            .is_err()
        );
        let mut forged = serde_json::to_value(&request).unwrap();
        forged["source"] = serde_json::to_value(&source).unwrap();
        assert!(serde_json::from_value::<WorkloadClientRequestFrame>(forged).is_err());
        let cli = WorkloadInvocationSource::Cli {
            external_request_id: request.request_id.clone(),
        };
        assert_ne!(cli.request_id().unwrap(), request.request_id);
        assert!(
            WorkloadClientDispatchFrame {
                source: cli,
                request
            }
            .validate()
            .is_err()
        );
        let mut malformed = serde_json::to_value(source).unwrap();
        malformed["worker_boot_epoch"] = serde_json::json!(1);
        assert!(serde_json::from_value::<WorkloadInvocationSource>(malformed).is_err());
    }

    #[test]
    fn bundled_workload_fact_matches_producer_and_registry_ceiling() {
        let descriptor: ryeos_engine::runtime_registry::RuntimeYaml =
            serde_yaml::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../bundles/core/.ai/runtimes/worker-execution-runtime.yaml"
            )))
            .unwrap();
        ryeos_engine::runtime_registry::validate_admitted_runtime_descriptor(
            &ryeos_engine::canonical_ref::CanonicalRef::parse(
                "runtime:core/worker-execution-runtime",
            )
            .unwrap(),
            &descriptor,
        )
        .unwrap();
        assert_eq!(
            descriptor.launch_contract.runtime_facts[WORKLOAD_CLIENT_REQUEST_FACT].max_bytes,
            MAX_WORKLOAD_CLIENT_REQUEST_CONTRACT_BYTES,
        );
    }

    #[test]
    fn request_and_boot_share_the_in_flight_ceiling() {
        let mut requested = request(vec![execution("tool:project/check")]);
        let mut boot = WorkloadClientBootFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            grant_digest: "a".repeat(64),
            ingresses: vec![WorkloadClientIngress::Cli],
            execution_presentation: serde_json::json!([{}]),
            max_lifetime_seconds: 300,
            max_in_flight: MAX_WORKLOAD_CLIENT_IN_FLIGHT,
            max_request_bytes: MAX_WORKLOAD_CLIENT_FRAME_BYTES as u32,
        };
        for maximum in [
            0,
            MAX_WORKLOAD_CLIENT_IN_FLIGHT,
            MAX_WORKLOAD_CLIENT_IN_FLIGHT + 1,
        ] {
            requested.max_in_flight = maximum;
            boot.max_in_flight = maximum;
            assert_eq!(
                requested.validate().is_ok(),
                maximum == MAX_WORKLOAD_CLIENT_IN_FLIGHT
            );
            assert_eq!(
                boot.validate().is_ok(),
                maximum == MAX_WORKLOAD_CLIENT_IN_FLIGHT
            );
        }
        validate_workload_client_limits(
            MAX_WORKLOAD_CLIENT_IN_FLIGHT,
            MAX_WORKLOAD_CLIENT_INVOCATIONS_PER_BOOT,
            MAX_WORKLOAD_CLIENT_LIFETIME_SECONDS,
        )
        .unwrap();
        assert!(
            validate_workload_client_limits(1, MAX_WORKLOAD_CLIENT_INVOCATIONS_PER_BOOT + 1, 1)
                .is_err()
        );
        assert!(
            validate_workload_client_limits(1, 1, MAX_WORKLOAD_CLIENT_LIFETIME_SECONDS + 1)
                .is_err()
        );
    }

    #[test]
    fn binding_names_have_one_contract_for_declaration_and_invocation() {
        for name in ["valid", "bad=name", "bad/name", "bad name", ""] {
            let mut route = execution("tool:project/check");
            route
                .ref_bindings
                .insert(name.to_owned(), vec!["tool:project/format".to_owned()]);
            let frame = WorkloadClientRequestFrame {
                protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
                request_id: "test".to_owned(),
                operation: WorkloadClientOperation::Execute(WorkloadClientExecuteRequest {
                    item_ref: route.item_ref.clone(),
                    ref_bindings: BTreeMap::from([(
                        name.to_owned(),
                        "tool:project/format".to_owned(),
                    )]),
                    params: Value::Null,
                    call: None,
                }),
            };
            assert_eq!(
                validate_execution_ceilings(&[route]).is_ok(),
                name == "valid"
            );
            assert_eq!(frame.validate().is_ok(), name == "valid");
        }
    }

    #[test]
    fn control_frame_bounds_are_symmetric_and_reject_before_write() {
        let value = "x".repeat(MAX_WORKLOAD_CLIENT_CONTROL_FRAME_BYTES - 2);
        let mut bytes = Vec::new();
        write_frame_bounded(&mut bytes, &value, MAX_WORKLOAD_CLIENT_CONTROL_FRAME_BYTES).unwrap();
        let decoded: String = read_frame_bounded(
            &mut bytes.as_slice(),
            MAX_WORKLOAD_CLIENT_CONTROL_FRAME_BYTES,
        )
        .unwrap();
        assert_eq!(decoded, value);
        bytes.clear();
        assert!(
            write_frame_bounded(
                &mut bytes,
                &(value + "x"),
                MAX_WORKLOAD_CLIENT_CONTROL_FRAME_BYTES
            )
            .is_err()
        );
        assert!(bytes.is_empty());
    }

    #[test]
    fn error_producers_bound_utf8_bytes_not_character_count() {
        let message = bounded_error_message(&format!(
            "\0{}",
            "界".repeat(MAX_WORKLOAD_CLIENT_ERROR_MESSAGE_BYTES)
        ));
        assert!(message.len() <= MAX_WORKLOAD_CLIENT_ERROR_MESSAGE_BYTES);
        assert!(message.len() + '界'.len_utf8() > MAX_WORKLOAD_CLIENT_ERROR_MESSAGE_BYTES);
        WorkloadClientResponseFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            request_id: "test".to_owned(),
            outcome: WorkloadClientOutcome::Failed {
                code: "refused".to_owned(),
                message,
                retryable: false,
            },
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn project_request_refuses_noncanonical_execution_order() {
        let error = request(vec![
            execution("tool:project/format"),
            execution("directive:project/check"),
        ])
        .validate()
        .unwrap_err();
        assert!(error.to_string().contains("sorted and unique"));
    }

    #[test]
    fn node_policy_can_only_narrow_the_project_request() {
        let policy = WorkloadClientNodePolicy {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            delegation_cap_ceiling: vec!["ryeos.execute.*".to_owned()],
            allowed_effect_classes: vec!["live".to_owned()],
            allowed_workspace_access: vec![
                ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
            ],
            max_executions: 1,
            max_ref_bindings_per_execution: 0,
            max_calls_per_execution: 1,
            max_in_flight: 1,
            max_invocations_per_boot: 8,
            max_lifetime_seconds: 300,
            max_request_bytes: 4096,
        };
        let requested = request(vec![execution("tool:project/check")]);
        assert!(policy.admit_request(&requested).is_err());
    }

    #[test]
    fn node_policy_does_not_admit_a_detached_opt_in() {
        let policy = WorkloadClientNodePolicy {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            delegation_cap_ceiling: vec!["ryeos.execute.*".to_owned()],
            allowed_effect_classes: vec!["live".to_owned()],
            allowed_workspace_access: vec![
                ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
            ],
            max_executions: 1,
            max_ref_bindings_per_execution: 0,
            max_calls_per_execution: 1,
            max_in_flight: 2,
            max_invocations_per_boot: 8,
            max_lifetime_seconds: 300,
            max_request_bytes: 4096,
        };
        let plain = request(vec![execution("tool:project/check")]);
        policy.admit_request(&plain).unwrap();
        let mut opted_in = execution("tool:project/check");
        opted_in.detached = true;
        let error = policy.admit_request(&request(vec![opted_in])).unwrap_err();
        assert!(error.to_string().contains("detached"), "{error:#}");
    }

    #[test]
    fn detached_opt_in_is_absent_from_the_wire_unless_set() {
        let plain = serde_json::to_value(execution("tool:project/check")).unwrap();
        assert!(plain.get("detached").is_none());
        let decoded: WorkloadClientExecutionCeiling = serde_json::from_value(plain).unwrap();
        assert!(!decoded.detached);
        let mut opted_in = execution("tool:project/check");
        opted_in.detached = true;
        let value = serde_json::to_value(&opted_in).unwrap();
        assert_eq!(value["detached"], serde_json::json!(true));
        let decoded: WorkloadClientExecutionCeiling = serde_json::from_value(value).unwrap();
        assert!(decoded.detached);
    }

    const CHILD: &str = "T-0f3a9c1e-0000-4000-8000-000000000001";

    fn collect_frame(child_thread_id: &str) -> WorkloadClientRequestFrame {
        WorkloadClientRequestFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            request_id: "collect-1".to_owned(),
            operation: WorkloadClientOperation::Collect(WorkloadClientCollectRequest {
                child_thread_id: child_thread_id.to_owned(),
            }),
        }
    }

    #[test]
    fn collect_request_has_one_strict_wire_shape() {
        let frame = collect_frame(CHILD);
        frame.validate().unwrap();
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            value["operation"],
            serde_json::json!({"method":"collect","request":{"child_thread_id":CHILD}})
        );
        let decoded: WorkloadClientRequestFrame = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded, frame);

        // An unknown field is refused at the request, the operation and the
        // frame.
        let mut extra_request = value.clone();
        extra_request["operation"]["request"]["parent_thread_id"] = serde_json::json!("T-x");
        assert!(serde_json::from_value::<WorkloadClientRequestFrame>(extra_request).is_err());
        let mut extra_operation = value.clone();
        extra_operation["operation"]["session"] = serde_json::json!("T-x");
        assert!(serde_json::from_value::<WorkloadClientRequestFrame>(extra_operation).is_err());
        let mut extra_frame = value.clone();
        extra_frame["source"] = serde_json::json!("cli");
        assert!(serde_json::from_value::<WorkloadClientRequestFrame>(extra_frame).is_err());
        // The id is required.
        let mut missing = value;
        missing["operation"]["request"] = serde_json::json!({});
        assert!(serde_json::from_value::<WorkloadClientRequestFrame>(missing).is_err());
    }

    #[test]
    fn collect_request_refuses_a_non_canonical_thread_id() {
        let too_long = format!("T-{}", "a".repeat(200));
        for bad in [
            "",
            "T-",
            "child",
            "T-has space",
            "T-../escape",
            "t-lowercase-prefix",
            too_long.as_str(),
        ] {
            assert!(collect_frame(bad).validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn collect_result_is_bounded_and_never_cut_to_fit() {
        let child = |result: Option<WorkloadClientCollectedResult>| WorkloadClientCollectedChild {
            thread_id: CHILD.to_owned(),
            status: "completed".to_owned(),
            kind: "tool".to_owned(),
            item_ref: "tool:project/check".to_owned(),
            created_at: "2026-11-25T00:00:00Z".to_owned(),
            started_at: None,
            finished_at: None,
            result,
        };
        // No result (a running child) always fits.
        child(None).validate().unwrap();
        let within = child(Some(WorkloadClientCollectedResult {
            outcome_code: Some("ok".to_owned()),
            result: Some(serde_json::json!("x".repeat(1024))),
            error: None,
        }));
        within.validate().unwrap();
        let oversize = child(Some(WorkloadClientCollectedResult {
            outcome_code: None,
            result: Some(serde_json::json!(
                "x".repeat(MAX_WORKLOAD_CLIENT_COLLECT_RESULT_BYTES)
            )),
            error: None,
        }));
        assert!(!oversize.result_within_cap().unwrap());
        assert!(oversize.validate().is_err());
        // The cap covers the result and the error together.
        let split = child(Some(WorkloadClientCollectedResult {
            outcome_code: None,
            result: Some(serde_json::json!(
                "x".repeat(MAX_WORKLOAD_CLIENT_COLLECT_RESULT_BYTES / 2)
            )),
            error: Some(serde_json::json!(
                "y".repeat(MAX_WORKLOAD_CLIENT_COLLECT_RESULT_BYTES / 2)
            )),
        }));
        assert!(!split.result_within_cap().unwrap());
        let frame = WorkloadClientResponseFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            request_id: "collect-1".to_owned(),
            outcome: WorkloadClientOutcome::Collected { child: oversize },
        };
        assert!(frame.validate().is_err());
        let ok = WorkloadClientResponseFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            request_id: "collect-1".to_owned(),
            outcome: WorkloadClientOutcome::Collected { child: within },
        };
        ok.validate().unwrap();
        assert!(!ok.outcome.succeeded());
        // The response decoder is as strict as the request decoder.
        let mut value = serde_json::to_value(&ok).unwrap();
        value["outcome"]["child"]["project_root"] = serde_json::json!("/home/x");
        assert!(serde_json::from_value::<WorkloadClientResponseFrame>(value).is_err());
    }

    #[test]
    fn collect_opt_in_is_absent_from_the_wire_unless_set_and_needs_detach() {
        let plain = serde_json::to_value(execution("tool:project/check")).unwrap();
        assert!(plain.get("collect").is_none());
        let mut collecting = execution("tool:project/check");
        collecting.detached = true;
        collecting.collect = true;
        let value = serde_json::to_value(&collecting).unwrap();
        assert_eq!(value["collect"], serde_json::json!(true));
        let decoded: WorkloadClientExecutionCeiling = serde_json::from_value(value).unwrap();
        assert!(decoded.collect);
        validate_execution_ceilings(&[collecting.clone()]).unwrap();
        // Collecting children an item cannot launch is not a grant.
        collecting.detached = false;
        assert!(validate_execution_ceilings(&[collecting]).is_err());
    }

    #[test]
    fn node_policy_still_refuses_a_detached_collecting_execution() {
        let policy = WorkloadClientNodePolicy {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            delegation_cap_ceiling: vec!["ryeos.execute.*".to_owned()],
            allowed_effect_classes: vec!["live".to_owned()],
            allowed_workspace_access: vec![
                ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
            ],
            max_executions: 1,
            max_ref_bindings_per_execution: 0,
            max_calls_per_execution: 1,
            max_in_flight: 2,
            max_invocations_per_boot: 8,
            max_lifetime_seconds: 300,
            max_request_bytes: 4096,
        };
        let mut collecting = execution("tool:project/check");
        collecting.detached = true;
        collecting.collect = true;
        let error = policy
            .admit_request(&request(vec![collecting]))
            .unwrap_err();
        assert!(error.to_string().contains("detached"), "{error:#}");
    }

    #[test]
    fn bounded_frame_refuses_declared_size_before_payload_read_or_allocation() {
        let mut input = std::io::Cursor::new(4097_u32.to_be_bytes());
        let error = read_frame_bounded::<_, serde_json::Value>(&mut input, 4096).unwrap_err();
        assert!(error.to_string().contains("exceeds its byte bound"));
        assert_eq!(input.position(), 4);
    }

    #[test]
    fn project_request_cannot_exceed_the_declared_runtime_fact_bound() {
        let values = (0..32)
            .map(|index| format!("tool:project/{index:02}-{}", "x".repeat(180)))
            .collect::<Vec<_>>();
        let mut oversized = execution("tool:project/check");
        oversized.ref_bindings = (0..32)
            .map(|index| (format!("binding-{index:02}"), values.clone()))
            .collect();
        let error = request(vec![oversized]).validate().unwrap_err();
        assert!(error.to_string().contains("retained fact bound"));
    }

    #[test]
    fn the_secret_floor_matches_raw_escaped_and_numeric_forms() {
        let secret = "sk-live-0123456789";
        let secrets = [secret.to_owned()];
        // Raw inside a string, inside a nested key, and inside a number.
        assert!(value_contains_secret(
            &serde_json::json!({"a": [format!("prefix {secret} suffix")]}),
            &secrets
        ));
        let mut keyed = serde_json::Map::new();
        keyed.insert(secret.to_owned(), serde_json::json!(1));
        assert!(value_contains_secret(&Value::Object(keyed), &secrets));
        assert!(value_contains_secret(
            &serde_json::json!({"n": 1234567890123_u64}),
            &["1234567890123".to_owned()]
        ));
        // A result without it passes.
        assert!(!value_contains_secret(
            &serde_json::json!({"answer": 42, "text": "nothing here"}),
            &secrets
        ));
        assert!(!value_contains_secret(&serde_json::json!(null), &secrets));
        // The JSON-escaped form: the secret holds a quote and a backslash, and
        // the child printed an already-serialized document that holds it.
        let tricky = "pa\"ss\\word-1234".to_owned();
        let doc = serde_json::json!({"echo": serde_json::to_string(&tricky).unwrap()});
        assert!(value_contains_secret(&doc, std::slice::from_ref(&tricky)));
    }

    #[test]
    fn the_secret_floor_matches_the_ascii_escaped_unicode_form() {
        let secret = "clé-secrète-été".to_owned();
        assert!(value_contains_secret(
            &serde_json::json!({"x": secret.clone()}),
            std::slice::from_ref(&secret)
        ));
        let escaped = "cl\\u00e9-secr\\u00e8te-\\u00e9t\\u00e9";
        assert!(value_contains_secret(
            &serde_json::json!({"x": escaped}),
            std::slice::from_ref(&secret)
        ));
        let upper = escaped.replace("00e", "00E");
        assert!(value_contains_secret(
            &serde_json::json!({"x": upper}),
            std::slice::from_ref(&secret)
        ));
        // A supplementary-plane character is escaped as a surrogate pair.
        let astral = "key-\u{1F511}-material".to_owned();
        assert!(value_contains_secret(
            &serde_json::json!({"x": "key-\\ud83d\\udd11-material"}),
            std::slice::from_ref(&astral)
        ));
    }

    #[test]
    fn the_secret_floor_skips_values_below_its_minimum_length() {
        let short = "abc1234".to_owned();
        assert_eq!(short.len(), MIN_COLLECT_SECRET_FLOOR_BYTES - 1);
        assert!(!value_contains_secret(
            &serde_json::json!({"x": "contains abc1234 text"}),
            std::slice::from_ref(&short)
        ));
        let exact = "abc12345".to_owned();
        assert_eq!(exact.len(), MIN_COLLECT_SECRET_FLOOR_BYTES);
        assert!(value_contains_secret(
            &serde_json::json!({"x": "contains abc12345 text"}),
            std::slice::from_ref(&exact)
        ));
        // No secrets, or only short ones, never withholds anything.
        assert!(!value_contains_secret::<String>(
            &serde_json::json!("anything"),
            &[]
        ));
    }
}
