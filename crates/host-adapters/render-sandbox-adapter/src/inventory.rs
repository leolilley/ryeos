//! Read-only provider inventory: the sandboxes and snapshots one Render owner
//! scope holds. Every request is a GET; nothing is created, changed or
//! deleted, and the node decides what each row means.
//!
//! The list response shapes follow the terminated-sandbox list the bootstrap
//! cleanup already reads: a JSON array of `{<kind>: {...}, "cursor": "..."}`
//! entries paged with `ownerId`, `limit` and `cursor`. The snapshot list path
//! (`/v1/sandbox-snapshots`) is not in the signed provider spec; it sits under
//! the same fixed API origin. Neither list has been called against the live
//! API from this adapter yet.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::provider_inventory::{
    MAX_PROVIDER_INVENTORY_REQUEST_BYTES, MAX_PROVIDER_INVENTORY_RESOURCES,
    PROVIDER_INVENTORY_PROTOCOL, ProviderInventoryAdapterRequest, ProviderInventoryAdapterResponse,
    ProviderResource, ProviderResourceKind,
};
use ryeos_external_execution_contract::{
    LIFECYCLE_PROVIDER_SPEC_FD_ENV, LIFECYCLE_PROVIDER_SPEC_SHA256_ENV, LIFECYCLE_REQUEST_FD_ENV,
    LIFECYCLE_SETTINGS_FD_ENV, MAX_LIFECYCLE_PROVIDER_SPEC_BYTES, canonical_json,
    from_json_slice_strict,
};
use serde::Deserialize;
use serde_json::Value;

use crate::provider_spec::{ProviderSpec, RouteName};

pub(crate) const PAGE_LIMIT: usize = 100;
pub(crate) const MAX_PAGES: usize = 16;

/// Only the fields a listing needs; placement and production settings carry
/// others, which an inventory neither reads nor trusts.
#[derive(Debug, Deserialize)]
struct InventorySettings {
    owner_id: String,
    tls_roots_der_base64: Vec<String>,
}

impl crate::RenderApiSettings for InventorySettings {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }
    fn tls_roots_der_base64(&self) -> &[String] {
        &self.tls_roots_der_base64
    }
}

pub(crate) fn list_provider_resources(
    adapter: &lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    let deadline = crate::operation_deadline()?;
    ensure!(
        [
            crate::LIFECYCLE_BOOTSTRAP_FD_ENV,
            crate::LIFECYCLE_SIGNED_IMPORT_FD_ENV,
            crate::LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV,
        ]
        .iter()
        .all(|name| std::env::var_os(name).is_none()),
        "provider inventory received Worker activation authority"
    );
    let bytes = crate::read_sealed_env(
        LIFECYCLE_REQUEST_FD_ENV,
        MAX_PROVIDER_INVENTORY_REQUEST_BYTES,
    )?;
    let request: ProviderInventoryAdapterRequest =
        from_json_slice_strict(&bytes, MAX_PROVIDER_INVENTORY_REQUEST_BYTES)?;
    request.validate()?;
    ensure!(
        canonical_json(&request)? == bytes && request.provider_id == crate::ADAPTER_ID,
        "provider inventory request is noncanonical or selects another provider"
    );
    crate::verify_artifact(adapter, &request.adapter_artifact_hash, None)?;
    let spec_bytes = crate::read_sealed_env(
        LIFECYCLE_PROVIDER_SPEC_FD_ENV,
        usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )?;
    let spec_digest = std::env::var(LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("provider inventory lacks the captured provider spec digest")?;
    ensure!(
        spec_digest == request.provider_spec_digest
            && lillux::sha256_hex(&spec_bytes) == request.provider_spec_digest,
        "provider inventory spec changed its signed handoff"
    );
    let spec = ProviderSpec::parse(
        &spec_bytes,
        &lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json")),
    )?;
    let settings_bytes =
        crate::read_sealed_env(LIFECYCLE_SETTINGS_FD_ENV, crate::MAX_SETTINGS_BYTES)?;
    ensure!(
        lillux::sha256_hex(&settings_bytes) == request.settings_digest,
        "provider inventory settings differ from the signed binding"
    );
    let settings: InventorySettings = serde_json::from_slice(&settings_bytes)?;
    ensure!(
        !settings.owner_id.is_empty()
            && settings.owner_id.len() <= 256
            && settings
                .owner_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
        "invalid Render owner id"
    );
    crate::validate_tls_roots(&settings.tls_roots_der_base64)?;

    let (sandboxes, target) = crate::api_url(&spec, RouteName::Collection, None, &settings, None)?;
    crate::validate_api_url(
        &sandboxes,
        &target.path_segments,
        target.owner_id_query.as_deref(),
        None,
    )?;
    let mut snapshots = url::Url::parse(spec.api_base())?;
    snapshots
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Render API URL cannot accept path segments"))?
        .pop_if_empty()
        .push("v1")
        .push("sandbox-snapshots");

    let network = crate::network_context_from_captured_inputs()?;
    let cancellation = lillux::network::NetworkCancellation::default();
    let _signal = crate::SignalCancellation::install(cancellation.clone())?;
    let credential = crate::read_credential()?;
    let mut fetch = |base: &url::Url, cursor: Option<&str>| -> Result<Vec<u8>> {
        let url = page_url(base, &settings.owner_id, cursor);
        let (status, body) = crate::read_response(crate::send_api_request(
            &network,
            &url,
            "GET",
            None,
            &credential,
            deadline,
            &settings,
            &cancellation,
        )?)
        .map_err(|()| anyhow::anyhow!("provider list response is not bounded JSON"))?;
        ensure!(status == 200, "provider list answered HTTP {status}");
        Ok(body)
    };
    let mut resources = Vec::new();
    let sandboxes_complete = list_kind(
        ProviderResourceKind::Sandbox,
        &settings.owner_id,
        &mut |cursor| fetch(&sandboxes, cursor),
        &mut resources,
    )?;
    let snapshots_complete = list_kind(
        ProviderResourceKind::Snapshot,
        &settings.owner_id,
        &mut |cursor| fetch(&snapshots, cursor),
        &mut resources,
    )?;
    let response = ProviderInventoryAdapterResponse {
        protocol: PROVIDER_INVENTORY_PROTOCOL.into(),
        provider_id: request.provider_id.clone(),
        owner_scope: settings.owner_id.clone(),
        complete: sandboxes_complete && snapshots_complete,
        resources,
    };
    response.validate_for(&request)?;
    crate::write_response(&response)
}

pub(crate) fn page_url(base: &url::Url, owner_id: &str, cursor: Option<&str>) -> url::Url {
    let mut url = base.clone();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("ownerId", owner_id);
        query.append_pair("limit", &PAGE_LIMIT.to_string());
        if let Some(cursor) = cursor {
            query.append_pair("cursor", cursor);
        }
    }
    url
}

/// Page through one list. Returns whether the listing reached its end within
/// the page and row bounds; rows past the row bound are not kept.
pub(crate) fn list_kind(
    kind: ProviderResourceKind,
    owner_id: &str,
    fetch: &mut dyn FnMut(Option<&str>) -> Result<Vec<u8>>,
    resources: &mut Vec<ProviderResource>,
) -> Result<bool> {
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::BTreeSet::new();
    for _ in 0..MAX_PAGES {
        let body = fetch(cursor.as_deref())?;
        let (page, next) = scan_page(&body, kind, owner_id, &mut seen_cursors)?;
        let count = page.len();
        for resource in page {
            if resources.len() >= MAX_PROVIDER_INVENTORY_RESOURCES {
                return Ok(false);
            }
            resources.push(resource);
        }
        if count < PAGE_LIMIT {
            return Ok(true);
        }
        cursor = Some(next.context("full provider list page carries no cursor")?);
    }
    Ok(false)
}

/// One list page. A row that names another owner refuses the whole listing:
/// the provider answered for a scope this binding does not hold.
pub(crate) fn scan_page(
    body: &[u8],
    kind: ProviderResourceKind,
    owner_id: &str,
    seen_cursors: &mut std::collections::BTreeSet<String>,
) -> Result<(Vec<ProviderResource>, Option<String>)> {
    let entries: Vec<Value> =
        from_json_slice_strict(body, usize::try_from(crate::MAX_API_RESPONSE_BYTES)?)
            .context("provider list page is not a JSON array")?;
    ensure!(
        entries.len() <= PAGE_LIMIT,
        "provider list page exceeds its limit"
    );
    let key = match kind {
        ProviderResourceKind::Sandbox => "sandbox",
        ProviderResourceKind::Snapshot => "snapshot",
    };
    let mut rows = Vec::with_capacity(entries.len());
    let mut last_cursor = None;
    for entry in &entries {
        let cursor = entry["cursor"]
            .as_str()
            .filter(|cursor| {
                !cursor.is_empty()
                    && cursor.len() <= 512
                    && !cursor.bytes().any(|byte| byte.is_ascii_control())
            })
            .context("provider list entry has no usable cursor")?;
        ensure!(
            seen_cursors.insert(cursor.to_owned()),
            "provider list repeats a cursor"
        );
        last_cursor = Some(cursor.to_owned());
        let item = entry
            .get(key)
            .and_then(Value::as_object)
            .with_context(|| format!("provider list entry has no `{key}` object"))?;
        if let Some(owner) = item.get("ownerId") {
            ensure!(
                owner.as_str() == Some(owner_id),
                "provider list returned a resource of another owner"
            );
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .context("provider list entry has no id")?;
        let valid = match kind {
            ProviderResourceKind::Sandbox => crate::valid_sandbox_id(id),
            ProviderResourceKind::Snapshot => crate::valid_snapshot_id(id),
        };
        ensure!(valid, "provider list entry id is malformed");
        let state = item
            .get("status")
            .and_then(Value::as_str)
            .context("provider list entry has no status")?;
        let created_at = match item.get("createdAt") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => {
                chrono::DateTime::parse_from_rfc3339(value)
                    .context("provider list entry creation time is not RFC 3339")?;
                Some(value.clone())
            }
            Some(_) => anyhow::bail!("provider list entry creation time is not a string"),
        };
        let resource = ProviderResource {
            kind,
            provider_id: id.to_owned(),
            state: state.to_owned(),
            created_at,
        };
        resource.validate()?;
        rows.push(resource);
    }
    Ok((rows, last_cursor))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, id: &str, cursor: &str, owner: Option<&str>) -> Value {
        let mut item = serde_json::json!({
            "id": id,
            "status": "running",
            "createdAt": "2026-11-13T00:00:00Z",
            "region": "oregon",
        });
        if let Some(owner) = owner {
            item["ownerId"] = owner.into();
        }
        serde_json::json!({ kind: item, "cursor": cursor })
    }

    fn page(kind: &str, prefix: &str, start: usize, count: usize) -> Vec<u8> {
        let entries: Vec<Value> = (start..start + count)
            .map(|n| {
                entry(
                    kind,
                    &format!("{prefix}{n}"),
                    &format!("c{n}"),
                    Some("tea-owner"),
                )
            })
            .collect();
        serde_json::to_vec(&entries).unwrap()
    }

    #[test]
    fn inventory_parses_paginated_lists() {
        let mut calls = Vec::new();
        let mut resources = Vec::new();
        let complete = list_kind(
            ProviderResourceKind::Sandbox,
            "tea-owner",
            &mut |cursor| {
                calls.push(cursor.map(str::to_owned));
                Ok(match cursor {
                    None => page("sandbox", "sbx-", 0, PAGE_LIMIT),
                    Some(cursor) => {
                        assert_eq!(cursor, format!("c{}", PAGE_LIMIT - 1));
                        page("sandbox", "sbx-", PAGE_LIMIT, 3)
                    }
                })
            },
            &mut resources,
        )
        .unwrap();
        assert!(complete);
        assert_eq!(calls.len(), 2);
        assert_eq!(resources.len(), PAGE_LIMIT + 3);
        assert_eq!(resources[0].provider_id, "sbx-0");
        assert_eq!(resources[0].state, "running");

        let mut snapshots = Vec::new();
        let complete = list_kind(
            ProviderResourceKind::Snapshot,
            "tea-owner",
            &mut |_| Ok(page("snapshot", "snp-", 0, 2)),
            &mut snapshots,
        )
        .unwrap();
        assert!(complete);
        assert_eq!(snapshots[1].kind, ProviderResourceKind::Snapshot);

        // A listing longer than the row bound is reported incomplete.
        let mut many = Vec::new();
        let mut next = 0;
        let complete = list_kind(
            ProviderResourceKind::Sandbox,
            "tea-owner",
            &mut |_| {
                next += PAGE_LIMIT;
                Ok(page("sandbox", "sbx-", next, PAGE_LIMIT))
            },
            &mut many,
        )
        .unwrap();
        assert!(!complete);
        assert_eq!(many.len(), MAX_PROVIDER_INVENTORY_RESOURCES);
    }

    #[test]
    fn inventory_refuses_foreign_owner() {
        let body = serde_json::to_vec(&vec![
            entry("sandbox", "sbx-1", "c1", Some("tea-owner")),
            entry("sandbox", "sbx-2", "c2", Some("tea-other")),
        ])
        .unwrap();
        let error = scan_page(
            &body,
            ProviderResourceKind::Sandbox,
            "tea-owner",
            &mut Default::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("another owner"), "{error:#}");
    }

    #[test]
    fn inventory_refuses_repeated_cursors_and_malformed_ids() {
        let repeated = serde_json::to_vec(&vec![
            entry("sandbox", "sbx-1", "c1", None),
            entry("sandbox", "sbx-2", "c1", None),
        ])
        .unwrap();
        assert!(
            scan_page(
                &repeated,
                ProviderResourceKind::Sandbox,
                "tea-owner",
                &mut Default::default()
            )
            .is_err()
        );
        let malformed = serde_json::to_vec(&vec![entry("snapshot", "sbx-1", "c1", None)]).unwrap();
        assert!(
            scan_page(
                &malformed,
                ProviderResourceKind::Snapshot,
                "tea-owner",
                &mut Default::default()
            )
            .is_err()
        );
    }

    #[test]
    fn page_urls_carry_owner_limit_and_cursor_only() {
        let base =
            url::Url::parse("https://api.render.com/v1/sandboxes?ownerId=tea-owner").unwrap();
        let url = page_url(&base, "tea-owner", Some("abc"));
        assert_eq!(
            url.as_str(),
            "https://api.render.com/v1/sandboxes?ownerId=tea-owner&limit=100&cursor=abc"
        );
    }
}
