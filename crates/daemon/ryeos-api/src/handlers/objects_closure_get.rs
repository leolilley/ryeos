//! `objects/closure/get` — retrieve every present CAS entry in a schema-defined closure.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde_json::Value;

use crate::handlers::objects_closure_describe::{
    Request, admit_request, closure_summary_json, collect_limited_closure_with_cas,
    enforce_serialized_response_limit,
};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let req = admit_request(req, &state)?;
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let _cas_guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let report = collect_limited_closure_with_cas(&req, &cas)?;
    if !req.allow_incomplete && !report.is_complete() {
        bail!(
            "object closure is incomplete: missing_objects={} first_missing_object={:?}; \
             missing_blobs={} first_missing_blob={:?}; malformed_objects={} \
             first_malformed_object={:?}; unsupported_objects={} first_unsupported_object={:?}",
            report.missing_objects.len(),
            report.missing_objects.first(),
            report.missing_blobs.len(),
            report.missing_blobs.first(),
            report.malformed_objects.len(),
            report.malformed_objects.first(),
            report.unsupported_objects.len(),
            report.unsupported_objects.first(),
        );
    }
    if !report.large_object_hashes.is_empty() && !req.allow_untransported_large_objects {
        bail!(
            "objects/closure/get cannot transport {} referenced large objects; use a large-object-aware transport",
            report.large_object_hashes.len()
        );
    }

    let mut entries = Vec::new();
    let mut total_object_bytes = 0_u64;
    let closure_summary = closure_summary_json(&report, true);
    let response_envelope = serde_json::json!({
        "closure": closure_summary.clone(),
        "object_bytes": u64::MAX,
        "blob_bytes": u64::MAX,
        "entries": [],
    });
    let mut estimated_response_bytes =
        u64::try_from(serde_json::to_vec(&response_envelope)?.len())?;
    let mut entry_count = 0usize;

    for hash in &report.object_hashes {
        if let Some(raw) = cas.get_object_raw_bytes_bounded_with_capsule_ceiling(
            hash,
            req.max_object_bytes,
            ryeos_state::objects::MAX_ADMITTED_LAUNCH_CAPSULE_BYTES as u64,
        )? {
            let cap = usize::try_from(req.max_object_bytes)
                .context("admitted per-object byte limit exceeds this platform")?;
            let value: Value =
                serde_json::from_slice(&raw).context("failed to parse closure object")?;
            if lillux::canonical_json_bounded(&value, cap)?.as_slice() != raw {
                bail!("closure object {hash} is not canonical JSON");
            }
            let object_bytes =
                u64::try_from(raw.len()).context("closure object size exceeds u64")?;
            total_object_bytes = total_object_bytes
                .checked_add(object_bytes)
                .context("object closure byte count overflow")?;
            if total_object_bytes > req.max_total_object_bytes {
                bail!(
                    "object closure exceeds max_total_object_bytes: {} > {}",
                    total_object_bytes,
                    req.max_total_object_bytes
                );
            }
            let entry_shell = serde_json::to_vec(&serde_json::json!({
                "hash": hash,
                "kind": "object",
                "value": Value::Null,
            }))?;
            let entry_separator = if entry_count > 0 { 1_u64 } else { 0 };
            let entry_overhead = u64::try_from(entry_shell.len().saturating_sub(4))?
                .checked_add(entry_separator)
                .context("object closure response size overflow")?;
            estimated_response_bytes = estimated_response_bytes
                .checked_add(object_bytes)
                .and_then(|size| size.checked_add(entry_overhead))
                .context("object closure response size overflow")?;
            if estimated_response_bytes > req.max_response_bytes {
                bail!(
                    "object closure response exceeds max_response_bytes: {} > {}",
                    estimated_response_bytes,
                    req.max_response_bytes
                );
            }
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "object",
                "value": value,
            }));
            entry_count += 1;
        } else {
            let entry_shell = serde_json::to_vec(&serde_json::json!({
                "hash": hash,
                "kind": "missing",
            }))?;
            let entry_separator = if entry_count > 0 { 1_u64 } else { 0 };
            estimated_response_bytes = estimated_response_bytes
                .checked_add(u64::try_from(entry_shell.len())?)
                .and_then(|size| size.checked_add(entry_separator))
                .context("object closure response size overflow")?;
            if estimated_response_bytes > req.max_response_bytes {
                bail!("object closure response exceeds max_response_bytes");
            }
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "missing",
            }));
            entry_count += 1;
        }
    }

    let mut total_blob_bytes = 0_u64;
    for hash in &report.blob_hashes {
        let remaining_blob_bytes = req
            .max_total_blob_bytes
            .checked_sub(total_blob_bytes)
            .context("object closure blob byte accounting exceeded its admitted total")?;
        let response_budget = req
            .max_response_bytes
            .checked_sub(estimated_response_bytes)
            .context("object closure response exceeds max_response_bytes")?;
        let (read_ceiling, entry_overhead) = admitted_blob_entry_read_bound(
            hash,
            req.max_blob_bytes,
            remaining_blob_bytes,
            response_budget,
            entry_count > 0,
        )?;
        if let Some(data) = cas.get_blob_bounded(hash, read_ceiling)? {
            let blob_bytes = u64::try_from(data.len()).unwrap_or(u64::MAX);
            total_blob_bytes = total_blob_bytes
                .checked_add(blob_bytes)
                .context("object closure blob byte count overflow")?;
            if total_blob_bytes > req.max_total_blob_bytes {
                bail!(
                    "object closure exceeds max_total_blob_bytes: {} > {}",
                    total_blob_bytes,
                    req.max_total_blob_bytes
                );
            }
            let encoded_bytes = encoded_len(blob_bytes);
            estimated_response_bytes = estimated_response_bytes
                .checked_add(entry_overhead)
                .and_then(|size| size.checked_add(encoded_bytes))
                .context("object closure response size overflow")?;
            if estimated_response_bytes > req.max_response_bytes {
                bail!(
                    "object closure response exceeds max_response_bytes: {} > {}",
                    estimated_response_bytes,
                    req.max_response_bytes
                );
            }
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "blob",
                "data": base64::engine::general_purpose::STANDARD.encode(&data),
            }));
            entry_count += 1;
        } else {
            let missing_shell = serde_json::to_vec(&serde_json::json!({
                "hash": hash,
                "kind": "missing",
            }))?;
            let entry_separator = if entry_count > 0 { 1_u64 } else { 0 };
            let missing_overhead = u64::try_from(missing_shell.len())?
                .checked_add(entry_separator)
                .context("object closure response size overflow")?;
            estimated_response_bytes = estimated_response_bytes
                .checked_add(missing_overhead)
                .context("object closure response size overflow")?;
            if estimated_response_bytes > req.max_response_bytes {
                bail!("object closure response exceeds max_response_bytes");
            }
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "missing",
            }));
            entry_count += 1;
        }
    }

    let response = serde_json::json!({
        "closure": closure_summary,
        "object_bytes": total_object_bytes,
        "blob_bytes": total_blob_bytes,
        "entries": entries,
    });
    enforce_serialized_response_limit(&response, req.max_response_bytes)?;
    Ok(response)
}

fn admitted_blob_entry_read_bound(
    hash: &str,
    max_blob_bytes: u64,
    remaining_total_blob_bytes: u64,
    remaining_response_bytes: u64,
    has_previous_entry: bool,
) -> Result<(u64, u64)> {
    let entry_shell = serde_json::to_vec(&serde_json::json!({
        "hash": hash,
        "kind": "blob",
        "data": "",
    }))?;
    let entry_separator = if has_previous_entry { 1_u64 } else { 0 };
    let entry_overhead = u64::try_from(entry_shell.len().saturating_sub(2))?
        .checked_add(entry_separator)
        .context("object closure response size overflow")?;
    let response_data_bytes = remaining_response_bytes
        .checked_sub(entry_overhead)
        .context("object closure response has no remaining blob budget")?;
    let response_blob_bytes = (response_data_bytes / 4).saturating_mul(3);
    Ok((
        max_blob_bytes
            .min(remaining_total_blob_bytes)
            .min(response_blob_bytes),
        entry_overhead,
    ))
}

fn encoded_len(raw_len: u64) -> u64 {
    raw_len.saturating_add(2) / 3 * 4
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:objects/closure/get",
    endpoint: "objects.closure.get",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects/closure/get"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_blob_read_limit_includes_json_entry_and_base64_expansion() {
        let hash = "ab".repeat(32);
        let (limit, overhead) =
            admitted_blob_entry_read_bound(&hash, 1024, 1024, 1_000_000, false).unwrap();
        assert_eq!(limit, 1024);
        let exact_response_budget = overhead + encoded_len(limit);
        let (exact_limit, exact_overhead) =
            admitted_blob_entry_read_bound(&hash, 1024, 1024, exact_response_budget, false)
                .unwrap();
        assert_eq!(exact_overhead, overhead);
        assert_eq!(exact_limit, limit);

        let (below_limit, _) =
            admitted_blob_entry_read_bound(&hash, 1024, 1024, exact_response_budget - 1, false)
                .unwrap();
        assert!(encoded_len(below_limit) <= exact_response_budget - 1 - overhead);
        assert!(encoded_len(below_limit + 1) > exact_response_budget - 1 - overhead);

        let (aggregate_limited, _) =
            admitted_blob_entry_read_bound(&hash, 1024, 17, 1_000_000, true).unwrap();
        assert_eq!(aggregate_limited, 17);
    }

    #[test]
    fn closure_blob_read_limit_rejects_budget_without_entry_overhead() {
        let hash = "cd".repeat(32);
        assert!(admitted_blob_entry_read_bound(&hash, 1024, 1024, 1, false).is_err());
    }
}
