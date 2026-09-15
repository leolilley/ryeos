//! Pure Graph execution-evidence interpretation.
//!
//! This handler owns only Graph semantics: the exact static unary call shape
//! and the projection of one successful terminal/event history into a result
//! plus candidate runtime coordinates. The daemon must independently
//! authenticate the supplied effective program, complete signed history,
//! runtime action intent, child snapshot/capsule, result and realization.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::resolution::KindComposedView;
use ryeos_graph_definition::{EdgeSpec, EffectClass, ErrorMode, GraphNode, NodeType};
use ryeos_handler_protocol::{
    ExecutionEvidenceCandidateCallWire, ExecutionEvidenceDescribeRequest,
    ExecutionEvidenceDescribeResponse, ExecutionEvidenceEventWire, ExecutionEvidenceProgramWire,
    ExecutionEvidenceProjectRequest, ExecutionEvidenceProjectResponse,
    ExecutionEvidenceRequiredCallWire, HandlerResponse,
};
use ryeos_runtime::callback::ActionPayload;
use ryeos_runtime::callback_contract::{
    RuntimeDispatchEffectClass, RuntimeDispatchEvidence, RuntimeDispatchPublication,
    RuntimeDispatchSource,
};
use ryeos_runtime::{EvaluationContext, EvaluationLimits, EvaluationSession};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
struct ExpectedUnaryCall {
    action_node: String,
    action_step: u32,
    target_ref: String,
    parameters: Value,
    ref_bindings: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallStart {
    operation_id: String,
    tool: String,
    call_id: String,
    graph_run_id: String,
    definition_ref: String,
    effective_definition_digest: String,
    node: String,
    node_ref: String,
    step: u32,
    item_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallResult {
    operation_id: String,
    tool: String,
    call_id: String,
    graph_run_id: String,
    definition_ref: String,
    effective_definition_digest: String,
    node: String,
    node_ref: String,
    step: u32,
    item_id: String,
    status: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildSpawned {
    child_thread_id: String,
    node: String,
    step: u32,
    item_id: String,
    spawn_reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptArtifact {
    artifact_type: String,
    uri: String,
    content_hash: Value,
    metadata: ReceiptMetadata,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptMetadata {
    node: String,
    step: u32,
    definition_ref: String,
    effective_definition_digest: String,
    graph_run_id: String,
    node_result_hash: String,
    cache_hit: bool,
    #[serde(rename = "elapsed_ms")]
    _elapsed_ms: u64,
    timestamp: String,
    error: Value,
    cost: Value,
    dispatch: RuntimeDispatchEvidence,
}

pub fn describe(request: ExecutionEvidenceDescribeRequest) -> HandlerResponse {
    let response = match describe_inner(request) {
        Ok(required_calls) => ExecutionEvidenceDescribeResponse::Described { required_calls },
        Err(error) => ExecutionEvidenceDescribeResponse::Refused {
            message: format!("{error:#}"),
        },
    };
    HandlerResponse::ExecutionEvidenceDescribe { response }
}

pub fn project(request: ExecutionEvidenceProjectRequest) -> HandlerResponse {
    let response = match project_inner(request) {
        Ok((result, calls)) => ExecutionEvidenceProjectResponse::Projected { result, calls },
        Err(error) => ExecutionEvidenceProjectResponse::Refused {
            message: format!("{error:#}"),
        },
    };
    HandlerResponse::ExecutionEvidenceProject { response }
}

pub fn wrong_request() -> HandlerResponse {
    HandlerResponse::ExecutionEvidenceProject {
        response: ExecutionEvidenceProjectResponse::Refused {
            message: "Graph execution-evidence handler accepts only evidence requests".into(),
        },
    }
}

fn describe_inner(
    request: ExecutionEvidenceDescribeRequest,
) -> anyhow::Result<Vec<ExecutionEvidenceRequiredCallWire>> {
    require_empty_config(&request.config)?;
    let expected = prove_static_unary_call(&request.effective_program)?;
    Ok(vec![ExecutionEvidenceRequiredCallWire {
        call_id: evidence_call_id(&expected),
        request: serde_json::to_value(expected_action(&expected)?)?,
    }])
}

fn project_inner(
    request: ExecutionEvidenceProjectRequest,
) -> anyhow::Result<(Value, Vec<ExecutionEvidenceCandidateCallWire>)> {
    require_empty_config(&request.config)?;
    let expected = prove_static_unary_call(&request.effective_program)?;
    if request.terminal.status != "completed" || !request.terminal.error.is_null() {
        bail!("Graph execution evidence requires one successful terminal");
    }
    let graph: ryeos_graph_definition::GraphResult =
        serde_json::from_value(request.terminal.result).context("decode Graph terminal result")?;
    if !graph.success
        || graph.status != ryeos_graph_definition::GraphRunStatus::Completed
        || graph.definition_ref != request.effective_program.canonical_ref
        || graph.effective_definition_digest
            != request.effective_program.effective_definition_digest
        || graph.steps != 2
        || graph.errors_suppressed.is_some()
        || graph.errors.is_some()
        || graph.error.is_some()
    {
        bail!("Graph terminal contradicts the described unary execution");
    }
    let result = graph
        .result
        .context("Graph terminal did not return an evidence result")?;
    let candidate = project_events(
        &expected,
        &request.effective_program,
        &graph.graph_run_id,
        &result,
        &request.events,
    )?;
    Ok((result, vec![candidate]))
}

fn require_empty_config(config: &Value) -> anyhow::Result<()> {
    if config != &serde_json::json!({}) {
        bail!("Graph execution-evidence handler takes no configuration");
    }
    Ok(())
}

fn prove_static_unary_call(
    program: &ExecutionEvidenceProgramWire,
) -> anyhow::Result<ExpectedUnaryCall> {
    let view = KindComposedView {
        composed: program.composed.composed.clone(),
        derived: program.composed.derived.clone().into_iter().collect(),
        policy_facts: program.composed.policy_facts.clone().into_iter().collect(),
    };
    let prepared = ryeos_graph_definition::prepare_effective_graph(
        &program.canonical_ref,
        &view,
        &program.ancestor_requested_ids,
    )?;
    let config = &prepared.file.config;
    if prepared.file.effects != EffectClass::Live
        || prepared.file.product_recipe.is_some()
        || config.nodes.len() != 2
        || config.max_steps < 2
        || config.segment_steps.is_some()
        || config.on_error != ErrorMode::Fail
        || !config.env_requires.is_empty()
        || !prepared.compiled.hooks().is_empty()
    {
        bail!("Graph evidence requires a live two-node hookless unary call");
    }
    let action = config
        .nodes
        .get(&config.start)
        .context("Graph evidence start node is missing")?;
    require_plain_node(action)?;
    if action.node_type != NodeType::Action || action.output.is_some() {
        bail!("Graph evidence start must be a unary action");
    }
    let Some(EdgeSpec::Unconditional { to }) = &action.next else {
        bail!("Graph evidence requires one unconditional action-to-return edge");
    };
    if to == &config.start {
        bail!("Graph evidence cannot loop");
    }
    let terminal = config
        .nodes
        .get(to)
        .context("Graph evidence return node is missing")?;
    require_plain_node(terminal)?;
    if terminal.node_type != NodeType::Return
        || terminal.action.is_some()
        || terminal.assign.is_some()
        || terminal.next.is_some()
        || terminal.output.is_none()
    {
        bail!("Graph evidence requires a terminal return with no further action");
    }
    let compiled_action = prepared
        .compiled
        .node(&config.start)
        .action
        .as_ref()
        .context("Graph evidence action is not compiled")?;
    if compiled_action.references().iter().next().is_some() {
        bail!("Graph evidence action must be entirely static");
    }
    let context = EvaluationContext::new();
    let limits = EvaluationLimits::default();
    let mut session = EvaluationSession::with_context(&context, &limits);
    let rendered = compiled_action
        .render(&mut session)
        .map_err(anyhow::Error::msg)?;
    let action = rendered
        .as_object()
        .context("compiled Graph evidence action is not an object")?;
    if action.keys().any(|key| {
        !matches!(
            key.as_str(),
            "item_id" | "params" | "ref_bindings" | "thread"
        )
    }) {
        bail!("Graph evidence action has unsupported launch controls");
    }
    let target_ref = action
        .get("item_id")
        .and_then(Value::as_str)
        .context("Graph evidence action has no exact item ref")?;
    let target = CanonicalRef::parse(target_ref)?;
    if target.suffix.is_some() || target.to_string() != target_ref {
        bail!("Graph evidence action must name an unsuffixed canonical item");
    }
    if action
        .get("thread")
        .is_some_and(|value| value.as_str() != Some("inline"))
    {
        bail!("Graph evidence action must be inline");
    }
    let ref_bindings: BTreeMap<String, String> = serde_json::from_value(
        action
            .get("ref_bindings")
            .cloned()
            .context("Graph evidence action has no ref_bindings")?,
    )?;
    if !ref_bindings.is_empty() {
        bail!("Graph evidence action cannot have reference bindings");
    }
    let parameters = action
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if !parameters.is_object() {
        bail!("Graph evidence action parameters must be a static object");
    }
    Ok(ExpectedUnaryCall {
        action_node: config.start.clone(),
        action_step: 0,
        target_ref: target_ref.to_owned(),
        parameters,
        ref_bindings,
    })
}

fn require_plain_node(node: &GraphNode) -> anyhow::Result<()> {
    if node.effects != EffectClass::Live
        || node.cache_result
        || node.follow
        || node.detach
        || node.retry.is_some()
        || node.on_error.is_some()
        || node.project_observations.is_some()
        || node.facets.is_some()
        || node.over.is_some()
        || node.r#as.is_some()
        || node.collect.is_some()
        || node.collect_threads.is_some()
        || node.parallel
        || node.max_concurrency.is_some()
        || !node.env_requires.is_empty()
    {
        bail!("Graph evidence node has unproven replay, iteration or side-effect controls");
    }
    Ok(())
}

fn evidence_call_id(expected: &ExpectedUnaryCall) -> String {
    format!("{}:{}", expected.action_step, expected.action_node)
}

fn expected_action(expected: &ExpectedUnaryCall) -> anyhow::Result<ActionPayload> {
    Ok(ActionPayload {
        operation_id: None,
        item_id: expected.target_ref.clone(),
        ref_bindings: expected.ref_bindings.clone(),
        product_selections: Vec::new(),
        params: expected.parameters.clone(),
        thread: "inline".to_owned(),
        call: None,
        facets: None,
        launch_window: None,
    })
}

fn project_events(
    expected: &ExpectedUnaryCall,
    program: &ExecutionEvidenceProgramWire,
    graph_run_id: &str,
    terminal_result: &Value,
    events: &[ExecutionEvidenceEventWire],
) -> anyhow::Result<ExecutionEvidenceCandidateCallWire> {
    validate_complete_event_sequence(events)?;
    let mut starts = Vec::new();
    let mut results = Vec::new();
    let mut children = Vec::new();
    let mut receipts = Vec::new();
    for event in events {
        match event.event_type.as_str() {
            ryeos_state::event_types::TOOL_CALL_START => starts.push((
                event.thread_seq,
                serde_json::from_value::<ToolCallStart>(event.payload.clone())
                    .context("decode Graph evidence tool_call_start")?,
            )),
            ryeos_state::event_types::TOOL_CALL_RESULT => results.push((
                event.thread_seq,
                serde_json::from_value::<ToolCallResult>(event.payload.clone())
                    .context("decode Graph evidence tool_call_result")?,
            )),
            ryeos_state::event_types::CHILD_THREAD_SPAWNED => children.push((
                event.thread_seq,
                serde_json::from_value::<ChildSpawned>(event.payload.clone())
                    .context("decode Graph evidence child_thread_spawned")?,
            )),
            ryeos_state::event_types::ARTIFACT_PUBLISHED
                if event.payload.get("artifact_type").and_then(Value::as_str)
                    == Some("graph_node_receipt") =>
            {
                receipts.push((
                    event.thread_seq,
                    serde_json::from_value::<ReceiptArtifact>(event.payload.clone())
                        .context("decode Graph evidence node receipt")?,
                ))
            }
            ryeos_state::event_types::GRAPH_NODE_RETRY
            | ryeos_state::event_types::GRAPH_FOLLOW_SUSPENDED
            | ryeos_state::event_types::GRAPH_FOREACH_STARTED
            | ryeos_state::event_types::GRAPH_FOREACH_ITERATION
            | ryeos_state::event_types::THREAD_CONTINUED
            | ryeos_state::event_types::CONTINUATION_REQUESTED
            | ryeos_state::event_types::CONTINUATION_ACCEPTED => {
                bail!("Graph evidence history contains an unsupported execution mode")
            }
            _ => {}
        }
    }
    let [(start_seq, start)] = starts.as_slice() else {
        bail!("Graph evidence history must contain exactly one Tool start")
    };
    let [(result_seq, action_result)] = results.as_slice() else {
        bail!("Graph evidence history must contain exactly one Tool result")
    };
    let [(child_seq, child)] = children.as_slice() else {
        bail!("Graph evidence history must contain exactly one child spawn")
    };
    let [(receipt_seq, artifact)] = receipts.as_slice() else {
        bail!("Graph evidence history must contain exactly one node receipt")
    };
    let runtime_call_id = format!(
        "{graph_run_id}:{}:{}",
        expected.action_step, expected.action_node
    );
    let node_ref = format!("{}#node:{}", program.canonical_ref, expected.action_node);
    if start.operation_id != action_result.operation_id
        || start.tool != expected.target_ref
        || start.item_id != expected.target_ref
        || action_result.tool != expected.target_ref
        || action_result.item_id != expected.target_ref
        || start.call_id != runtime_call_id
        || action_result.call_id != runtime_call_id
        || start.graph_run_id != graph_run_id
        || action_result.graph_run_id != graph_run_id
        || start.definition_ref != program.canonical_ref
        || action_result.definition_ref != program.canonical_ref
        || start.effective_definition_digest != program.effective_definition_digest
        || action_result.effective_definition_digest != program.effective_definition_digest
        || start.node != expected.action_node
        || action_result.node != expected.action_node
        || start.node_ref != node_ref
        || action_result.node_ref != node_ref
        || start.step != expected.action_step
        || action_result.step != expected.action_step
        || action_result.status != "ok"
    {
        bail!("Graph Tool events contradict the described unary call");
    }
    if child.node != expected.action_node
        || child.step != expected.action_step
        || child.item_id != expected.target_ref
        || child.spawn_reason != "dispatch"
        || child.child_thread_id.is_empty()
    {
        bail!("Graph child event contradicts the described unary call");
    }
    let receipt = &artifact.metadata;
    if artifact.artifact_type != "graph_node_receipt"
        || artifact.uri
            != format!(
                "graph://runs/{graph_run_id}/node-receipts/{}",
                expected.action_step
            )
        || !artifact.content_hash.is_null()
        || receipt.node != expected.action_node
        || receipt.step != expected.action_step
        || receipt.definition_ref != program.canonical_ref
        || receipt.effective_definition_digest != program.effective_definition_digest
        || receipt.graph_run_id != graph_run_id
        || receipt.cache_hit
        || receipt.timestamp.is_empty()
        || !receipt.error.is_null()
        || !receipt.cost.is_null()
    {
        bail!("Graph node receipt contradicts the described unary call");
    }
    receipt.dispatch.validate()?;
    let action_digest =
        ryeos_runtime::callback::dispatch_action_digest(&expected_action(expected)?)?;
    if receipt.dispatch.source != RuntimeDispatchSource::Executed
        || receipt.dispatch.effect_class != RuntimeDispatchEffectClass::Live
        || receipt.dispatch.publication != RuntimeDispatchPublication::NotApplicable
        || receipt.dispatch.action_digest != action_digest
        || receipt.dispatch.effect_identity.is_some()
        || receipt.dispatch.record_hash.is_some()
        || receipt.dispatch.replayed_from.is_some()
        || receipt.dispatch.result_projection
            != ryeos_effect_contract::DispatchResultProjection::DispatchedSubject
    {
        bail!("Graph node receipt is cached, replayed, or names the wrong action");
    }
    validate_digest("Graph operation", &start.operation_id)?;
    validate_digest("Graph action", &action_digest)?;
    validate_digest("Graph result", &receipt.node_result_hash)?;
    if canonical_digest(terminal_result)? != receipt.node_result_hash {
        bail!("Graph terminal result differs from the committed Tool receipt");
    }
    if !(*start_seq < *result_seq && *result_seq < *child_seq && *child_seq < *receipt_seq) {
        bail!("Graph occurrence events are not in commit order");
    }
    Ok(ExecutionEvidenceCandidateCallWire {
        call_id: evidence_call_id(expected),
        operation_id: start.operation_id.clone(),
        action_digest,
        child_thread_id: child.child_thread_id.clone(),
        result_digest: receipt.node_result_hash.clone(),
    })
}

fn validate_complete_event_sequence(events: &[ExecutionEvidenceEventWire]) -> anyhow::Result<()> {
    if events.is_empty() {
        bail!("Graph evidence history is empty");
    }
    let mut sequences = BTreeSet::new();
    for event in events {
        if event.thread_seq == 0 || !sequences.insert(event.thread_seq) {
            bail!("Graph evidence history has an invalid thread sequence");
        }
    }
    if sequences
        .iter()
        .copied()
        .enumerate()
        .any(|(index, sequence)| sequence != index as u64 + 1)
    {
        bail!("Graph evidence history is not a contiguous thread sequence");
    }
    Ok(())
}

fn canonical_digest(value: &Value) -> anyhow::Result<String> {
    ryeos_state::objects::canonical_value_digest(value)
}

fn validate_digest(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("{label} is not a canonical SHA-256 digest");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ryeos_engine::hooks::{
        EFFECTIVE_HOOK_PLAN_SCHEMA, EffectiveHookLayer, EffectiveHookPlan, HOOK_CONTEXT_SCHEMA,
        HookContextContract, HookEventContract, HookResultMode,
    };
    use ryeos_handler_protocol::{ExecutionEvidenceTerminalWire, LaunchComposedViewWire};
    use serde_json::json;

    use super::*;

    fn program() -> ExecutionEvidenceProgramWire {
        let composed: Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/e2e/environment-products/unknown-output/bundle-overlay/.ai/graphs/test/verify-dynamic-products.yaml"
        )))
        .unwrap();
        let caps = composed["requires"]["capabilities"]["declared"].clone();
        let plan = EffectiveHookPlan {
            schema: EFFECTIVE_HOOK_PLAN_SCHEMA.to_owned(),
            owner_kind: "graph".to_owned(),
            event_contracts: BTreeMap::from([(
                ryeos_state::event_types::GRAPH_STARTED.to_owned(),
                HookEventContract {
                    context_contract: HookContextContract {
                        schema: HOOK_CONTEXT_SCHEMA.to_owned(),
                        allowed_roots: BTreeSet::from(["event".to_owned()]),
                    },
                    allowed_results: BTreeSet::from([HookResultMode::Discard]),
                },
            )]),
            authored: EffectiveHookLayer::empty(),
            builtin: EffectiveHookLayer::empty(),
            infrastructure: EffectiveHookLayer::empty(),
            context: EffectiveHookLayer::empty(),
            operator: EffectiveHookLayer::empty(),
            project: EffectiveHookLayer::empty(),
            sources: Vec::new(),
        };
        ExecutionEvidenceProgramWire {
            canonical_ref: "graph:test/verify-dynamic-products".into(),
            effective_definition_digest: "d".repeat(64),
            composed: LaunchComposedViewWire {
                composed,
                derived: BTreeMap::from([(
                    ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY.to_owned(),
                    plan.to_value().unwrap(),
                )]),
                policy_facts: BTreeMap::from([("effective_caps".into(), caps)]),
            },
            ancestor_requested_ids: Vec::new(),
        }
    }

    fn terminal(result: Value) -> ExecutionEvidenceTerminalWire {
        ExecutionEvidenceTerminalWire {
            status: "completed".into(),
            result,
            error: Value::Null,
            artifacts: Vec::new(),
        }
    }

    fn event(thread_seq: u64, event_type: &str, payload: Value) -> ExecutionEvidenceEventWire {
        ExecutionEvidenceEventWire {
            thread_seq,
            event_type: event_type.to_owned(),
            payload,
        }
    }

    fn successful_project_request() -> ExecutionEvidenceProjectRequest {
        let effective_program = program();
        let expected = prove_static_unary_call(&effective_program).unwrap();
        let graph_run_id = "run";
        let operation_id = "a".repeat(64);
        let mut dispatched_action = expected_action(&expected).unwrap();
        dispatched_action.operation_id = Some(operation_id.clone());
        let action_digest =
            ryeos_runtime::callback::dispatch_action_digest(&dispatched_action).unwrap();
        assert_eq!(
            action_digest,
            ryeos_runtime::callback::dispatch_action_digest(&expected_action(&expected).unwrap())
                .unwrap(),
            "the occurrence coordinate must remain separate from behavior identity"
        );
        let result = json!({
            "schema": "ryeos.product_qualification_result.v1",
            "subject_manifest_hash": "b".repeat(64),
            "claims": ["bounded_payload_pair"],
            "probe_evidence": {
                "auxiliary_manifest_hash": "c".repeat(64),
                "network_contacted": false
            }
        });
        let result_digest = canonical_digest(&result).unwrap();
        let runtime_call_id = format!("{graph_run_id}:0:probe");
        let node_ref = "graph:test/verify-dynamic-products#node:probe";
        let dispatch = RuntimeDispatchEvidence {
            source: RuntimeDispatchSource::Executed,
            effect_class: RuntimeDispatchEffectClass::Live,
            action_digest: action_digest.clone(),
            effect_identity: None,
            publication: RuntimeDispatchPublication::NotApplicable,
            record_hash: None,
            replayed_from: None,
            result_projection: ryeos_effect_contract::DispatchResultProjection::DispatchedSubject,
        };
        let events = vec![
            event(1, ryeos_state::event_types::GRAPH_STARTED, json!({})),
            event(2, ryeos_state::event_types::GRAPH_STEP_STARTED, json!({})),
            event(
                3,
                ryeos_state::event_types::TOOL_CALL_START,
                json!({
                    "operation_id": operation_id,
                    "tool": expected.target_ref,
                    "call_id": runtime_call_id,
                    "graph_run_id": graph_run_id,
                    "definition_ref": effective_program.canonical_ref,
                    "effective_definition_digest": effective_program.effective_definition_digest,
                    "node": expected.action_node,
                    "node_ref": node_ref,
                    "step": expected.action_step,
                    "item_id": expected.target_ref,
                }),
            ),
            event(
                4,
                ryeos_state::event_types::TOOL_CALL_RESULT,
                json!({
                    "operation_id": operation_id,
                    "tool": expected.target_ref,
                    "call_id": runtime_call_id,
                    "graph_run_id": graph_run_id,
                    "definition_ref": effective_program.canonical_ref,
                    "effective_definition_digest": effective_program.effective_definition_digest,
                    "node": expected.action_node,
                    "node_ref": node_ref,
                    "step": expected.action_step,
                    "item_id": expected.target_ref,
                    "status": "ok",
                }),
            ),
            event(
                5,
                ryeos_state::event_types::CHILD_THREAD_SPAWNED,
                json!({
                    "child_thread_id": "child",
                    "node": expected.action_node,
                    "step": expected.action_step,
                    "item_id": expected.target_ref,
                    "spawn_reason": "dispatch",
                }),
            ),
            event(
                6,
                ryeos_state::event_types::ARTIFACT_PUBLISHED,
                json!({
                    "artifact_type": "graph_node_receipt",
                    "uri": "graph://runs/run/node-receipts/0",
                    "content_hash": null,
                    "metadata": {
                        "node": expected.action_node,
                        "step": expected.action_step,
                        "definition_ref": effective_program.canonical_ref,
                        "effective_definition_digest": effective_program.effective_definition_digest,
                        "graph_run_id": graph_run_id,
                        "node_result_hash": result_digest,
                        "cache_hit": false,
                        "elapsed_ms": 1,
                        "timestamp": "2026-09-08T00:00:00Z",
                        "error": null,
                        "cost": null,
                        "dispatch": dispatch,
                    },
                }),
            ),
            event(7, ryeos_state::event_types::GRAPH_STEP_COMPLETED, json!({})),
            event(8, ryeos_state::event_types::GRAPH_STEP_STARTED, json!({})),
            event(9, ryeos_state::event_types::GRAPH_STEP_COMPLETED, json!({})),
            event(10, ryeos_state::event_types::GRAPH_COMPLETED, json!({})),
        ];
        let graph = ryeos_graph_definition::GraphResult {
            success: true,
            graph_id: "test/verify-dynamic-products".into(),
            definition_ref: effective_program.canonical_ref.clone(),
            effective_definition_digest: effective_program.effective_definition_digest.clone(),
            graph_run_id: graph_run_id.into(),
            status: ryeos_graph_definition::GraphRunStatus::Completed,
            steps: 2,
            state: json!({"qualification_result": result}),
            result: Some(result),
            errors_suppressed: None,
            errors: None,
            error: None,
            cost: None,
            node_costs: Vec::new(),
            hook_costs: Vec::new(),
        };
        ExecutionEvidenceProjectRequest {
            config: json!({}),
            effective_program,
            terminal: terminal(serde_json::to_value(graph).unwrap()),
            events,
        }
    }

    #[test]
    fn describe_returns_one_exact_normalized_static_call() {
        let HandlerResponse::ExecutionEvidenceDescribe {
            response: ExecutionEvidenceDescribeResponse::Described { required_calls },
        } = describe(ExecutionEvidenceDescribeRequest {
            config: json!({}),
            effective_program: program(),
        })
        else {
            panic!("Graph description refused")
        };
        assert_eq!(required_calls.len(), 1);
        assert_eq!(required_calls[0].call_id, "0:probe");
        let action: ActionPayload =
            serde_json::from_value(required_calls[0].request.clone()).unwrap();
        assert_eq!(action.item_id, "tool:test/probe-dynamic-products");
        assert_eq!(action.thread, "inline");
        assert!(action.ref_bindings.is_empty());
        assert!(action.product_selections.is_empty());
        assert_eq!(action.params, json!({}));
    }

    #[test]
    fn hidden_hooks_and_dynamic_or_replayed_calls_refuse_description() {
        let mut hooked = program();
        let mut plan = EffectiveHookPlan::from_value(
            &hooked.composed.derived[ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY],
        )
        .unwrap();
        plan.operator.hooks.push(ryeos_engine::hooks::HookDefinition {
            id: "hidden".into(),
            event: "graph_started".into(),
            result: ryeos_engine::hooks::HookResultMode::Discard,
            condition: ryeos_runtime::ExpressionCondition::Absent,
            action: json!({"item_id":"tool:test/probe-dynamic-products","ref_bindings":{},"params":{}}),
        });
        plan.operator.dispatch_caps = vec!["ryeos.execute.tool.test/probe-dynamic-products".into()];
        plan.sources.push(ryeos_engine::hooks::HookSourceEvidence {
            layer: ryeos_engine::hooks::HookLayer::Operator,
            canonical_ref: "config:test/hooks".into(),
            source_space: ryeos_engine::contracts::ItemSpace::Node,
            trust_class: ryeos_engine::resolution::TrustClass::TrustedNode,
            signer_fingerprint: "1".repeat(64),
            source_raw_content_digest: "2".repeat(64),
        });
        hooked.composed.derived.insert(
            ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY.into(),
            plan.to_value().unwrap(),
        );
        assert!(matches!(
            describe(ExecutionEvidenceDescribeRequest {
                config: json!({}),
                effective_program: hooked,
            }),
            HandlerResponse::ExecutionEvidenceDescribe {
                response: ExecutionEvidenceDescribeResponse::Refused { .. }
            }
        ));

        for (key, value) in [
            ("cache_result", json!(true)),
            ("effects", json!("recorded")),
            ("follow", json!(true)),
            ("retry", json!({"attempts":1,"backoff_ms":1})),
        ] {
            let mut invalid = program();
            invalid.composed.composed["config"]["nodes"]["probe"][key] = value;
            assert!(matches!(
                describe(ExecutionEvidenceDescribeRequest {
                    config: json!({}),
                    effective_program: invalid,
                }),
                HandlerResponse::ExecutionEvidenceDescribe {
                    response: ExecutionEvidenceDescribeResponse::Refused { .. }
                }
            ));
        }
    }

    #[test]
    fn static_shape_does_not_assign_meaning_to_the_callee_kind_name() {
        let mut renamed = program();
        renamed.composed.composed["config"]["nodes"]["probe"]["action"]["item_id"] =
            json!("custom:test/probe-dynamic-products");
        let HandlerResponse::ExecutionEvidenceDescribe {
            response: ExecutionEvidenceDescribeResponse::Described { required_calls },
        } = describe(ExecutionEvidenceDescribeRequest {
            config: json!({}),
            effective_program: renamed,
        })
        else {
            panic!("static canonical callee kind was assigned handler-specific meaning")
        };
        assert_eq!(
            required_calls[0].request["item_id"],
            "custom:test/probe-dynamic-products"
        );
    }

    #[test]
    fn project_returns_the_exact_committed_unary_occurrence() {
        let request = successful_project_request();
        let expected_result = request.terminal.result["result"].clone();
        let HandlerResponse::ExecutionEvidenceProject {
            response: ExecutionEvidenceProjectResponse::Projected { result, calls },
        } = project(request)
        else {
            panic!("exact committed unary history was refused")
        };
        assert_eq!(result, expected_result);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "0:probe");
        assert_eq!(calls[0].operation_id, "a".repeat(64));
        assert_eq!(calls[0].child_thread_id, "child");
        assert_eq!(calls[0].result_digest, canonical_digest(&result).unwrap());
    }

    #[test]
    fn project_refuses_a_terminal_without_exact_history() {
        let graph = ryeos_graph_definition::GraphResult {
            success: true,
            graph_id: "test/verify-dynamic-products".into(),
            definition_ref: "graph:test/verify-dynamic-products".into(),
            effective_definition_digest: "d".repeat(64),
            graph_run_id: "run".into(),
            status: ryeos_graph_definition::GraphRunStatus::Completed,
            steps: 2,
            state: json!({}),
            result: Some(json!({"accepted":true})),
            errors_suppressed: None,
            errors: None,
            error: None,
            cost: None,
            node_costs: Vec::new(),
            hook_costs: Vec::new(),
        };
        assert!(matches!(
            project(ExecutionEvidenceProjectRequest {
                config: json!({}),
                effective_program: program(),
                terminal: terminal(serde_json::to_value(graph).unwrap()),
                events: Vec::new(),
            }),
            HandlerResponse::ExecutionEvidenceProject {
                response: ExecutionEvidenceProjectResponse::Refused { .. }
            }
        ));
    }
}
