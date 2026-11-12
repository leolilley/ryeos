//! Terminal-envelope, final-cost and follow-envelope validation.
use super::*;

/// The external transcript is the input to the same ordinary result interpreter
/// used by the runner, never permission to sign an unrelated caller result.
pub(super) fn validate_external_direct_result(
    update: &FinalizeThreadRecord,
    output: &runtime_db::external_execution::ExternalDirectOutput,
) -> Result<()> {
    let expected = ryeos_engine::dispatch::interpret_external_terminal(
        &output.termination,
        &output.stdout,
        &output.stderr,
    );
    anyhow::ensure!(
        update.status == expected.status.as_str()
            && update.outcome_code == expected.outcome_code
            && update.result_json == expected.result
            && update.error_json == expected.error,
        "external direct final result differs from its authenticated target output"
    );
    anyhow::ensure!(
        update.artifacts.is_empty()
            && update.final_cost.is_none()
            && update.managed_envelope.is_none()
            && update.result_project_snapshot_hash.is_none()
            && update.result_workspace_output_capture_hash.is_none(),
        "external direct result cannot add unadmitted artifacts, cost or project publication"
    );
    Ok(())
}

pub(super) fn runtime_status_for_thread_status(
    status: ThreadStatus,
) -> Result<ryeos_runtime::envelope::RuntimeResultStatus> {
    use ryeos_runtime::envelope::RuntimeResultStatus;

    match status {
        ThreadStatus::Completed => Ok(RuntimeResultStatus::Completed),
        ThreadStatus::Failed => Ok(RuntimeResultStatus::Failed),
        ThreadStatus::Cancelled => Ok(RuntimeResultStatus::Cancelled),
        ThreadStatus::Killed => Ok(RuntimeResultStatus::Killed),
        ThreadStatus::TimedOut => Ok(RuntimeResultStatus::TimedOut),
        ThreadStatus::Continued => Ok(RuntimeResultStatus::Continued),
        ThreadStatus::Created | ThreadStatus::Running => {
            bail!("managed runtime envelope requires a terminal thread status")
        }
    }
}

pub(super) fn validate_managed_terminal_envelope(
    raw: &Value,
    thread_id: &str,
    status: ThreadStatus,
    result: Option<&Value>,
    error: Option<&Value>,
    final_cost: Option<&ryeos_engine::contracts::FinalCost>,
) -> Result<()> {
    validate_checkpoint_shape(raw, "managed runtime terminal envelope")
        .context("validate managed runtime terminal envelope")?;
    let envelope = ryeos_runtime::envelope::decode_managed_runtime_terminal_envelope(raw)
        .map_err(anyhow::Error::msg)
        .context("decode managed runtime terminal envelope")?;
    if envelope.child_thread_id != thread_id {
        bail!(
            "managed runtime envelope child_thread_id `{}` contradicts settlement thread `{thread_id}`",
            envelope.child_thread_id
        );
    }
    let expected_runtime_status = runtime_status_for_thread_status(status)?;
    if envelope.status != expected_runtime_status {
        bail!(
            "managed runtime envelope status `{}` contradicts settlement status `{}`",
            envelope.status.as_str(),
            status
        );
    }
    if envelope.success != expected_runtime_status.is_success() {
        bail!(
            "managed runtime envelope success contradicts settlement status `{}`",
            status
        );
    }
    if status == ThreadStatus::Completed && error.is_some() {
        bail!("completed settlement must not carry a terminal error");
    }
    if status == ThreadStatus::Continued && error.is_some() {
        bail!("continued settlement must not carry a terminal error");
    }
    let expected_payload = result.or(error).cloned().unwrap_or(Value::Null);
    if envelope.result != expected_payload {
        bail!("managed runtime envelope result contradicts settlement result/error payload");
    }

    match (final_cost, envelope.cost.as_ref()) {
        (None, None) => {}
        (Some(final_cost), Some(runtime_cost))
            if final_cost.input_tokens == runtime_cost.input_tokens
                && final_cost.output_tokens == runtime_cost.output_tokens
                && final_cost.spend == runtime_cost.total_usd
                && final_cost.basis == runtime_cost.basis => {}
        (Some(_), Some(_)) => {
            bail!("managed runtime envelope cost contradicts settlement final cost")
        }
        (Some(_), None) | (None, Some(_)) => {
            bail!("managed runtime envelope cost presence contradicts settlement final cost")
        }
    }

    // Deserializing these required fields is itself the contract check. Keep the
    // reads explicit so future removal does not accidentally make them optional.
    let _ = (&envelope.outputs, &envelope.warnings);
    Ok(())
}

pub(crate) fn validate_final_cost_for_settlement(
    cost: &ryeos_engine::contracts::FinalCost,
) -> Result<()> {
    // `spend` is `UsdNanos`: finite and non-negative by construction.
    if cost.input_tokens > i64::MAX as u64 {
        bail!("final cost input_tokens exceeds the settlement storage maximum");
    }
    if cost.output_tokens > i64::MAX as u64 {
        bail!("final cost output_tokens exceeds the settlement storage maximum");
    }
    match cost.basis.as_deref() {
        None | Some(ryeos_engine::launch_envelope_types::COST_BASIS_ROLLUP) => {}
        Some(basis) => {
            bail!("final cost basis `{basis}` is invalid; expected `rollup` or null");
        }
    }
    Ok(())
}

pub(super) fn terminal_facets(
    final_cost: Option<&ryeos_engine::contracts::FinalCost>,
) -> Result<BTreeMap<String, String>> {
    let mut facets = BTreeMap::new();
    if let Some(cost) = final_cost {
        facets.insert("cost.turns".to_string(), cost.turns.to_string());
        facets.insert(
            "cost.input_tokens".to_string(),
            cost.input_tokens.to_string(),
        );
        facets.insert(
            "cost.output_tokens".to_string(),
            cost.output_tokens.to_string(),
        );
        facets.insert("cost.spend".to_string(), cost.spend.to_canonical_string());
        if let Some(provider) = cost.provider.as_ref() {
            facets.insert("cost.provider".to_string(), provider.clone());
        }
        if let Some(basis) = cost.basis.as_ref() {
            facets.insert("cost.basis".to_string(), basis.clone());
        }
        if let Some(metadata) = cost.metadata.as_ref() {
            facets.insert(
                "cost.metadata_json".to_string(),
                serde_json::to_string(metadata).context("encode final cost metadata")?,
            );
        }
    }
    Ok(facets)
}

pub(super) fn managed_runtime_terminal_supplement(
    envelope: Option<&Value>,
) -> Result<Option<ryeos_state::objects::ManagedRuntimeTerminalSupplement>> {
    envelope
        .map(|envelope| {
            let envelope =
                ryeos_runtime::envelope::decode_managed_runtime_terminal_envelope(envelope)
                    .map_err(anyhow::Error::msg)
                    .context("decode validated managed runtime terminal envelope")?;
            Ok::<_, anyhow::Error>(ryeos_state::objects::ManagedRuntimeTerminalSupplement {
                outputs: envelope.outputs,
                warnings: envelope.warnings,
            })
        })
        .transpose()
}

pub(super) fn terminal_value_digest(value: Option<&Value>, field: &str) -> Result<Option<String>> {
    value
        .map(|value| {
            let canonical = lillux::canonical_json(value)
                .with_context(|| format!("canonicalize terminal {field}"))?;
            Ok::<_, anyhow::Error>(lillux::sha256_hex(canonical.as_bytes()))
        })
        .transpose()
}

pub(super) fn managed_terminal_envelope_from_supplement(
    thread_id: &str,
    status: ThreadStatus,
    result: Option<&Value>,
    error: Option<&Value>,
    final_cost: Option<&ryeos_engine::contracts::FinalCost>,
    supplement: ryeos_state::objects::ManagedRuntimeTerminalSupplement,
) -> Result<Value> {
    let runtime_status = runtime_status_for_thread_status(status)?;
    let raw_cost = final_cost
        .map(|cost| {
            serde_json::to_value(ryeos_runtime::envelope::RuntimeCost {
                input_tokens: cost.input_tokens,
                output_tokens: cost.output_tokens,
                total_usd: cost.spend,
                basis: cost.basis.clone(),
            })
        })
        .transpose()
        .context("encode authoritative managed runtime terminal cost")?;
    let envelope = json!({
        "success": runtime_status.is_success(),
        "child_thread_id": thread_id,
        "status": runtime_status,
        "result": result.or(error).cloned().unwrap_or(Value::Null),
        "outputs": supplement.outputs,
        "warnings": supplement.warnings,
        "cost": raw_cost,
    });
    validate_managed_terminal_envelope(&envelope, thread_id, status, result, error, final_cost)?;
    Ok(envelope)
}

pub(super) const FOLLOW_ENVELOPE_LIMIT_CODE: &str = "follow_terminal_envelope_limit_exceeded";

pub(super) fn follow_envelope_limit_failure(
    child_thread_id: &str,
    cost: Option<&ryeos_runtime::envelope::RuntimeCost>,
) -> Value {
    let status = ryeos_runtime::envelope::RuntimeResultStatus::Failed;
    ryeos_runtime::envelope::encode_follow_terminal_envelope(
        child_thread_id,
        status,
        json!({
            "code": FOLLOW_ENVELOPE_LIMIT_CODE,
            "message": "follow child terminal envelope exceeded the bounded parent resume payload",
        }),
        cost,
    )
    .expect("bounded follow failure envelope is statically valid")
}

pub(super) fn follow_envelope_limit_reservation() -> Value {
    let maximum_cost = ryeos_runtime::envelope::RuntimeCost {
        input_tokens: i64::MAX as u64,
        output_tokens: i64::MAX as u64,
        total_usd: ryeos_engine::launch_envelope_types::UsdNanos::MAX,
        basis: Some(ryeos_engine::launch_envelope_types::COST_BASIS_ROLLUP.to_string()),
    };
    let maximum_thread_id = format!("T-{}", "x".repeat(126));
    follow_envelope_limit_failure(&maximum_thread_id, Some(&maximum_cost))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ValidatedFinalCost {
    pub(super) completed_turns: u32,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) spend_usd: ryeos_engine::launch_envelope_types::UsdNanos,
}

pub(super) fn validate_final_cost(
    cost: &ryeos_engine::contracts::FinalCost,
) -> Result<ValidatedFinalCost> {
    validate_final_cost_for_settlement(cost)?;
    Ok(ValidatedFinalCost {
        completed_turns: cost.turns,
        input_tokens: cost.input_tokens,
        output_tokens: cost.output_tokens,
        spend_usd: cost.spend,
    })
}

pub(super) fn validated_follow_candidate_cost(
    candidate: &Value,
) -> Result<Option<ryeos_runtime::envelope::RuntimeCost>> {
    let Some(raw_cost) = candidate.get("cost") else {
        return Ok(None);
    };
    if raw_cost.is_null() {
        return Ok(None);
    }
    let cost: ryeos_runtime::envelope::RuntimeCost =
        serde_json::from_value(raw_cost.clone()).context("decode follow terminal cost")?;
    cost.validate().context("validate follow terminal cost")?;
    Ok(Some(cost))
}

pub(super) fn project_follow_action_result(
    item_ref: &str,
    result: Value,
    outputs: Value,
) -> Result<Value> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)
        .with_context(|| format!("parse followed item reference `{item_ref}`"))?;
    if canonical.kind == "graph" {
        return ryeos_runtime::graph_result::project_graph_action_result(result, outputs)
            .map_err(anyhow::Error::msg)
            .context("project followed graph return");
    }
    Ok(ryeos_runtime::envelope::project_kind_defined_action_result(
        result, outputs,
    ))
}

pub(super) fn compact_follow_terminal_envelope(
    waiter: &runtime_db::FollowWaiter,
    child_chain_root_id: &str,
    child_terminal_thread_id: &str,
    candidate: &Value,
) -> Result<Value> {
    let envelope = ryeos_runtime::envelope::decode_managed_runtime_terminal_envelope(candidate)
        .map_err(anyhow::Error::msg)
        .context("decode managed follow terminal candidate")?;
    if envelope.child_thread_id != child_terminal_thread_id {
        bail!(
            "follow terminal envelope child_thread_id `{}` does not match terminal child `{child_terminal_thread_id}`",
            envelope.child_thread_id
        );
    }
    if envelope.status == ryeos_runtime::envelope::RuntimeResultStatus::Continued {
        bail!("continued is not a terminal follow status");
    }
    if envelope.success != envelope.status.is_success() {
        bail!("managed follow terminal success contradicts its status");
    }
    let child = waiter
        .children
        .iter()
        .find(|child| child.child_chain_root_id == child_chain_root_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "follow waiter {} does not contain child chain {child_chain_root_id}",
                waiter.follow_key
            )
        })?;
    let result = if envelope.status.is_success() {
        project_follow_action_result(&child.item_ref, envelope.result, envelope.outputs)?
    } else {
        envelope.result
    };
    let compact = ryeos_runtime::envelope::encode_follow_terminal_envelope(
        child_terminal_thread_id,
        envelope.status,
        result,
        envelope.cost.as_ref(),
    )
    .map_err(anyhow::Error::msg)?;
    ryeos_runtime::envelope::decode_follow_terminal_envelope(&compact)
        .map_err(anyhow::Error::msg)
        .context("validate compact follow terminal envelope")?;
    Ok(compact)
}

pub(super) fn validate_follow_reservation_shape(seed: &runtime_db::NewFollowWaiter) -> Result<()> {
    if let Some(authority) = &seed.child_project_authority {
        authority.validate()?;
    }
    let expected = usize::try_from(seed.expected_children)
        .context("follow expected_children does not fit usize")?;
    if expected == 0 {
        bail!("follow waiter {} expects no children", seed.follow_key);
    }
    if !seed.fanout && expected != 1 {
        bail!(
            "non-fanout waiter {} must expect exactly one child",
            seed.follow_key
        );
    }
    let limits = checkpoint_shape_limits();
    if expected > limits.max_container_elements {
        bail!(
            "follow waiter {} expects {expected} children; maximum is {}",
            seed.follow_key,
            limits.max_container_elements
        );
    }
    if !seed.fanout {
        validate_checkpoint_shape(
            &follow_envelope_limit_reservation(),
            "reserved follow parent resume payload",
        )
        .context("validate reserved follow parent resume payload")?;
        return Ok(());
    }

    let pending = follow_envelope_limit_reservation();
    let mut budget = follow_fanout_items_budget(expected)?;
    for _ in 0..expected {
        budget
            .append(&pending)
            .context("validate reserved follow fanout parent resume payload")?;
    }
    Ok(())
}

pub(super) fn follow_fanout_items_budget(expected: usize) -> Result<RuntimeJsonArrayBudget> {
    let mut limits = checkpoint_shape_limits();
    // `"completed"` is the longest closed fanout status (11 serialized
    // bytes); reserve one comma per entry as well, with the leading `[` taking
    // the remaining byte in `1 + 12*n` (the closing `]` replaces the final
    // comma reservation).
    let status_bytes = 1usize
        .checked_add(
            12usize
                .checked_mul(expected)
                .context("follow status JSON byte count overflow")?,
        )
        .context("follow status JSON byte count overflow")?;
    let fixed_payload = json!({
        "fanout": true,
        "items": [],
        "statuses": [],
        "failed": expected,
        "expected": expected,
    });
    let fixed_bytes = serde_json::to_vec(&fixed_payload)
        .context("encode follow fanout fixed payload")?
        .len()
        .checked_sub(4)
        .expect("two empty JSON arrays contain four bytes");
    limits.max_result_bytes = limits
        .max_result_bytes
        .checked_sub(fixed_bytes)
        .and_then(|remaining| remaining.checked_sub(status_bytes))
        .context("follow fanout fixed payload exceeds runtime JSON byte limit")?;
    // Final nodes are the item-array nodes plus one status scalar per child,
    // the status array, root object, fanout boolean, failed count, and expected
    // count. The incremental budget owns the item array and its children.
    limits.max_result_nodes = limits
        .max_result_nodes
        .checked_sub(expected)
        .and_then(|remaining| remaining.checked_sub(5))
        .context("follow fanout fixed payload exceeds runtime JSON node limit")?;
    // The item budget treats its array as depth one; in the final payload that
    // array is nested under the root object and therefore starts at depth two.
    limits.max_result_depth = limits
        .max_result_depth
        .checked_sub(1)
        .context("follow fanout root exceeds runtime JSON depth limit")?;
    Ok(RuntimeJsonArrayBudget::with_limits(
        "follow fanout terminal-envelope cohort",
        limits,
    ))
}

pub(super) fn validate_prospective_follow_resume_payload(
    waiter: &runtime_db::FollowWaiter,
    child_chain_root_id: &str,
    candidate: &Value,
) -> Result<()> {
    let limits = checkpoint_shape_limits();
    let expected = usize::try_from(waiter.expected_children)
        .context("follow expected_children does not fit usize")?;
    if expected == 0 {
        bail!("follow waiter {} expects no children", waiter.follow_key);
    }
    if expected > limits.max_container_elements {
        bail!(
            "follow waiter {} expects {expected} children; maximum is {}",
            waiter.follow_key,
            limits.max_container_elements
        );
    }
    if !waiter.fanout && expected != 1 {
        bail!(
            "non-fanout waiter {} must expect exactly one child",
            waiter.follow_key
        );
    }

    let mut children = HashMap::with_capacity(waiter.children.len());
    for child in &waiter.children {
        if children.insert(child.item_index, child).is_some() {
            bail!(
                "follow waiter {} has duplicate child index {}",
                waiter.follow_key,
                child.item_index
            );
        }
    }
    let pending = follow_envelope_limit_reservation();
    let mut found_candidate = false;
    let mut budget = waiter
        .fanout
        .then(|| follow_fanout_items_budget(expected))
        .transpose()?;
    for item_index in 0..waiter.expected_children {
        let envelope = match children.get(&item_index) {
            Some(child) if child.child_chain_root_id == child_chain_root_id => {
                found_candidate = true;
                candidate
            }
            Some(child) => child.terminal_envelope.as_ref().unwrap_or(&pending),
            None => &pending,
        };
        if let Some(budget) = budget.as_mut() {
            budget.append(envelope)?;
        } else {
            validate_checkpoint_shape(envelope, "follow parent resume payload")?;
        }
    }
    if !found_candidate {
        bail!(
            "follow waiter {} does not contain child chain {child_chain_root_id}",
            waiter.follow_key
        );
    }

    Ok(())
}

pub(super) fn admit_follow_terminal_envelope(
    waiter: &runtime_db::FollowWaiter,
    child_chain_root_id: &str,
    child_terminal_thread_id: &str,
    candidate: &Value,
) -> Result<(Value, bool)> {
    let compact = compact_follow_terminal_envelope(
        waiter,
        child_chain_root_id,
        child_terminal_thread_id,
        candidate,
    )?;
    match validate_prospective_follow_resume_payload(waiter, child_chain_root_id, &compact) {
        Ok(()) => Ok((compact, false)),
        Err(candidate_error) => {
            let cost = validated_follow_candidate_cost(&compact)?;
            let degraded = follow_envelope_limit_failure(child_terminal_thread_id, cost.as_ref());
            validate_prospective_follow_resume_payload(waiter, child_chain_root_id, &degraded)
                .with_context(|| {
                    format!(
                        "follow terminal envelope exceeded bounds ({candidate_error}); bounded failure envelope also did not fit"
                    )
                })?;
            Ok((degraded, true))
        }
    }
}
