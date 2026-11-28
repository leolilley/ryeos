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
                && final_cost.basis == runtime_cost.basis
                && final_cost.usage_status == runtime_cost.usage_status => {}
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
        if !cost.usage_status.is_complete() {
            facets.insert(
                "cost.usage_status".to_string(),
                cost.usage_status.as_str().to_string(),
            );
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
        .map(|cost| serde_json::to_value(cost.to_runtime()))
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
        usage_status: Default::default(),
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
    pub(super) usage_status: ryeos_engine::launch_envelope_types::UsageStatus,
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
        usage_status: cost.usage_status,
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
    let is_worker_execution = ryeos_engine::canonical_ref::CanonicalRef::parse(&child.item_ref)
        .is_ok_and(|canonical| {
            canonical.kind == ryeos_runtime::envelope::WORKER_EXECUTION_ITEM_KIND
        });
    let result = if is_worker_execution {
        // A worker child's followed value is its outcome record for every
        // terminal status, never the raw session projection.
        ryeos_runtime::envelope::worker_follow_value(
            child_terminal_thread_id,
            envelope.status,
            &envelope.result,
            envelope.cost.as_ref(),
        )
    } else if envelope.status.is_success() {
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
    admit_follow_terminal_envelope_with_reads(
        waiter,
        child_chain_root_id,
        child_terminal_thread_id,
        candidate,
        None,
    )
}

/// Add the reads of a followed directive child to a compact envelope, cut to
/// fit the parent's resume bounds: the list is halved and marked `truncated`
/// until it fits, and left off only when not even an empty list fits.
fn attach_reads_fitting(
    waiter: &runtime_db::FollowWaiter,
    child_chain_root_id: &str,
    compact: Value,
    reads: &ryeos_runtime::envelope::FollowReads,
) -> Result<Value> {
    let mut shrunk = reads.clone();
    loop {
        let candidate = ryeos_runtime::envelope::with_follow_reads(compact.clone(), &shrunk)
            .map_err(anyhow::Error::msg)?;
        if validate_prospective_follow_resume_payload(waiter, child_chain_root_id, &candidate)
            .is_ok()
        {
            return Ok(candidate);
        }
        if shrunk.refs.is_empty() {
            return Ok(compact);
        }
        let keep = shrunk.refs.len() / 2;
        shrunk.refs.truncate(keep);
        shrunk.truncated = true;
    }
}

pub(super) fn admit_follow_terminal_envelope_with_reads(
    waiter: &runtime_db::FollowWaiter,
    child_chain_root_id: &str,
    child_terminal_thread_id: &str,
    candidate: &Value,
    reads: Option<&ryeos_runtime::envelope::FollowReads>,
) -> Result<(Value, bool)> {
    let compact = compact_follow_terminal_envelope(
        waiter,
        child_chain_root_id,
        child_terminal_thread_id,
        candidate,
    )?;
    let compact = match reads {
        Some(reads) => attach_reads_fitting(waiter, child_chain_root_id, compact, reads)?,
        None => compact,
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn worker_waiter() -> runtime_db::FollowWaiter {
        let mut waiter = super::super::tests::follow_waiter_for_admission();
        waiter.children[0].item_ref = "worker_execution:test/bounded".to_string();
        waiter
    }

    fn candidate(status: &str, result: Value, cost: Value) -> Value {
        json!({
            "success": status == "completed",
            "child_thread_id": "T-child",
            "status": status,
            "result": result,
            "outputs": {},
            "warnings": [],
            "cost": cost,
        })
    }

    fn compact(waiter: &runtime_db::FollowWaiter, candidate: &Value) -> Value {
        compact_follow_terminal_envelope(waiter, "T-child", "T-child", candidate).unwrap()
    }

    #[test]
    fn a_completed_worker_child_follows_as_its_outcome_record() {
        let worker_result = json!({"turn_id": "t1", "result": {"text": "ok", "truncated": false}});
        let envelope = compact(
            &worker_waiter(),
            &candidate(
                "completed",
                json!({"session": {
                    "state": "terminal", "outcome_kind": "completed",
                    "worker_result": worker_result,
                }}),
                json!({"input_tokens": 1, "output_tokens": 2, "total_usd": "0.000003"}),
            ),
        );
        let decoded = ryeos_runtime::envelope::decode_follow_terminal_envelope(&envelope).unwrap();
        assert!(decoded.success);
        assert_eq!(
            decoded.result,
            json!({
                "child_thread_id": "T-child",
                "status": "completed",
                "outcome_kind": "completed",
                "result": worker_result,
                "cost": {"input_tokens": 1, "output_tokens": 2, "total_usd": "0.000003"},
            })
        );
        assert!(decoded.cost.is_some());
    }

    #[test]
    fn a_failed_worker_child_follows_as_its_outcome_record_not_the_raw_session() {
        let envelope = compact(
            &worker_waiter(),
            &candidate(
                "cancelled",
                json!({"session": {
                    "state": "terminal", "outcome_kind": "deadline",
                    "worker_result": null, "bounded_execution": {"large": true},
                }}),
                Value::Null,
            ),
        );
        let decoded = ryeos_runtime::envelope::decode_follow_terminal_envelope(&envelope).unwrap();
        assert!(!decoded.success);
        assert_eq!(
            decoded.result,
            json!({
                "child_thread_id": "T-child",
                "status": "cancelled",
                "outcome_kind": "deadline",
                "result": null,
                "cost": null,
            })
        );
    }

    #[test]
    fn a_worker_failure_child_follows_as_a_failure_with_its_partial_result() {
        let worker_result =
            json!({"turn_id": "t1", "status": "crashed", "result": {"text": "partial"}});
        let envelope = compact(
            &worker_waiter(),
            &candidate(
                "failed",
                json!({"session": {
                    "state": "terminal", "outcome_kind": "worker_failure",
                    "worker_result": worker_result,
                }}),
                Value::Null,
            ),
        );
        let decoded = ryeos_runtime::envelope::decode_follow_terminal_envelope(&envelope).unwrap();
        assert!(!decoded.success);
        assert_eq!(
            decoded.result,
            json!({
                "child_thread_id": "T-child",
                "status": "failed",
                "outcome_kind": "worker_failure",
                "result": worker_result,
                "cost": null,
            })
        );
    }

    #[test]
    fn a_non_worker_child_keeps_its_kind_defined_projection() {
        let mut waiter = worker_waiter();
        waiter.children[0].item_ref = "tool:test/one".to_string();
        let envelope = compact(
            &waiter,
            &candidate("completed", json!({"answer": 42}), Value::Null),
        );
        let decoded = ryeos_runtime::envelope::decode_follow_terminal_envelope(&envelope).unwrap();
        assert_eq!(decoded.result, json!({"answer": 42}));
    }

    #[test]
    fn a_usage_status_mismatch_between_envelope_and_settlement_is_refused() {
        use ryeos_engine::launch_envelope_types::UsageStatus;

        let cost = |usage_status| ryeos_runtime::envelope::RuntimeCost {
            input_tokens: 3,
            output_tokens: 5,
            total_usd: ryeos_runtime::envelope::UsdNanos::parse_canonical("0.02").unwrap(),
            basis: None,
            usage_status,
        };
        let envelope = |usage_status| {
            json!({
                "success": true,
                "child_thread_id": "T-child",
                "status": "completed",
                "result": null,
                "outputs": {},
                "warnings": [],
                "cost": serde_json::to_value(cost(usage_status)).unwrap(),
            })
        };
        let validate = |raw: &Value, settled: UsageStatus| {
            let final_cost =
                ryeos_engine::contracts::FinalCost::from_runtime(&cost(settled), 0, None, None);
            validate_managed_terminal_envelope(
                raw,
                "T-child",
                ThreadStatus::Completed,
                None,
                None,
                Some(&final_cost),
            )
        };

        assert!(validate(&envelope(UsageStatus::Partial), UsageStatus::Partial).is_ok());
        assert!(validate(&envelope(UsageStatus::Complete), UsageStatus::Complete).is_ok());
        assert!(validate(&envelope(UsageStatus::Partial), UsageStatus::Complete).is_err());
        assert!(validate(&envelope(UsageStatus::Complete), UsageStatus::Partial).is_err());
        // Only a non-complete status is written to the wire.
        assert!(
            envelope(UsageStatus::Complete)["cost"]
                .get("usage_status")
                .is_none()
        );
        assert_eq!(
            envelope(UsageStatus::Partial)["cost"]["usage_status"],
            "partial"
        );
    }
}
