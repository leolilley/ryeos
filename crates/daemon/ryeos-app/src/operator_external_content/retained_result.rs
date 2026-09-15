//! Exact owner-authorized retained result admission, using ordinary import staging.

use super::*;
use ryeos_state::external_content::products::{ProductDeclaration, ProductShape, ProductStorage};
use ryeos_state::external_content::retained_project::RetainedProjectContent;

pub(super) fn import(
    state: Arc<AppState>,
    context: HandlerContext,
    request: RetainedResultImportRequest,
) -> anyhow::Result<ImportResponse> {
    let operator = crate::operator_authority::require_local_configured_operator(&state, &context)?;
    import_inner(state, context, request, None, operator)?
        .context("retained result selection is absent")
}

/// Same capture/publication owner with an additional admitted product ceiling.
/// The caller must establish the declaration's admitted recipe relationship.
pub(super) fn import_product(
    state: Arc<AppState>,
    context: HandlerContext,
    request: RetainedResultImportRequest,
    product: &ProductDeclaration,
) -> anyhow::Result<Option<ImportResponse>> {
    product.validate()?;
    if request.path != product.path
        || request.shape
            != match product.shape {
                ProductShape::File => ImportShape::File,
                ProductShape::Tree => ImportShape::Tree,
            }
        || request.storage
            != match product.storage {
                ProductStorage::Content => ImportStorage::Content,
                ProductStorage::LargeContent => ImportStorage::LargeContent,
            }
        || request.maximum_bytes > product.bounds.maximum_total_bytes
        || request.expected_file_sha256.is_some()
    {
        bail!("retained product request contradicts its admitted declaration");
    }
    // Only the exact product-capture owner enters this lane. Public retained
    // result import remains local-only; no remote filesystem import is granted.
    let operator = crate::operator_authority::require_admitted_operator(&state, &context)?;
    import_inner(state, context, request, Some(product), operator)
}

fn import_inner(
    state: Arc<AppState>,
    context: HandlerContext,
    request: RetainedResultImportRequest,
    product: Option<&ProductDeclaration>,
    operator: String,
) -> anyhow::Result<Option<ImportResponse>> {
    validate_relative_path(&request.path)?;
    if !lillux::valid_hash(&request.result_project_snapshot_hash)
        || request
            .result_project_snapshot_hash
            .bytes()
            .any(|b| b.is_ascii_uppercase())
    {
        bail!("retained result snapshot must be a canonical digest");
    }
    if request.expected_file_sha256.as_deref().is_some_and(|hash| {
        request.shape != ImportShape::File
            || !lillux::valid_hash(hash)
            || hash.bytes().any(|b| b.is_ascii_uppercase())
    }) {
        bail!("expected_file_sha256 requires a file source and a canonical digest");
    }
    let policy = state.node_policy.require::<
        crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord,
    >()?;
    if request.maximum_bytes == 0 || request.maximum_bytes > policy.limits.max_total_bytes {
        bail!("retained result import maximum_bytes is outside node policy");
    }
    // Hold the same pinned CAS generation against collection throughout exact
    // authority resolution, verification and durable upload-root publication.
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let (thread, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &request.chain_root_id,
            &request.thread_id,
        )?
        .context("retained result execution does not exist")?;
    let root = state
        .state_store
        .get_authoritative_root_thread_snapshot(&request.chain_root_id)?
        .context("retained result chain root does not exist")?;
    // Thread authors use the canonical principal (`fp:...`); upload stages
    // retain the operator key fingerprint. Do not interchange these owners.
    authorize_result(&root, &thread, &request, &context.fingerprint)?;
    let cas = authority.cas_store()?;
    let snapshot = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        &cas,
        &request.result_project_snapshot_hash,
    )?;
    let mut bounds = ryeos_state::LargeContentCaptureBounds {
        max_depth: policy.limits.max_depth,
        max_entries: policy.limits.max_entries,
        max_file_bytes: policy.limits.max_file_bytes.min(request.maximum_bytes),
        max_total_bytes: request.maximum_bytes,
    };
    if request.storage == ImportStorage::Content {
        bounds.max_depth = bounds.max_depth.min(ryeos_state::MAX_CAPTURE_DEPTH);
        bounds.max_entries = bounds.max_entries.min(ryeos_state::MAX_CAPTURE_ENTRIES);
        bounds.max_total_bytes = bounds.max_total_bytes.min(ryeos_state::MAX_CAPTURE_BYTES);
        bounds.max_file_bytes = bounds
            .max_file_bytes
            .min(ryeos_state::MAX_CAPTURE_FILE_BYTES)
            .min(bounds.max_total_bytes);
    }
    let capture_policy = ryeos_state::LargeContentCapturePolicy::new(
        request.path.clone(),
        state.ignore_matcher.as_ref(),
        bounds,
    )?;
    let selected = if let Some(product) = product {
        let Some(selected) =
            product.select_retained(&snapshot, state.ignore_matcher.as_ref(), &bounds)?
        else {
            return Ok(None);
        };
        selected
    } else {
        RetainedProjectContent::select(
            &snapshot,
            match request.shape {
                ImportShape::File => ryeos_state::ExternalContentCaptureKind::File,
                ImportShape::Tree => ryeos_state::ExternalContentCaptureKind::Tree,
            },
            &capture_policy,
        )?
    };
    if request
        .expected_file_sha256
        .as_deref()
        .is_some_and(|hash| !selected.expected_file_matches(hash))
    {
        bail!("retained result file contradicts expected_file_sha256");
    }
    let mut capture_identity = serde_json::json!({
        "source": "retained_result",
        "request": request,
        "admitted_launch_capsule_hash": thread.admitted_launch_capsule_hash,
        "limits": policy.limits,
        "capture_floor_rules": ryeos_state::project_sync::durable_content_capture_floor_rules(),
        "configured_ignore_patterns": state.ignore_matcher.canonical_patterns(),
    });
    if let Some(product) = product {
        capture_identity["product_declaration"] = serde_json::to_value(product)?;
    }
    let digest = lillux::sha256_hex(lillux::canonical_json(&capture_identity)?.as_bytes());
    let key = ryeos_state::DurableCasPublicationKey::external_content_import(&digest)?;
    // Existing blobs need no duplicate payload allocation. Keep conservative
    // metadata/inode reserves; only large-store conversion reserves payload.
    require_import_store_capacity(
        "external-content CAS",
        cas.filesystem_capacity()?,
        policy.limits.minimum_free_bytes,
        ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64,
        selected.entry_count(),
    )?;
    let large_store = authority.large_object_store()?;
    if selected.large_file_bytes() != 0 {
        require_import_store_capacity(
            "external-content large store",
            large_store.filesystem_capacity()?,
            policy.limits.minimum_free_bytes,
            selected.large_file_bytes(),
            selected.entry_count(),
        )?;
        if large_store
            .total_stored_bytes()?
            .checked_add(selected.large_file_bytes())
            .context("retained import large-store budget overflow")?
            > policy.limits.store_budget_bytes
        {
            bail!("retained result import would exceed the node large-store budget");
        }
    }
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire external-content write permit: {error}")
        })?;
    let mut stage = authority
        .require_recovery()?
        .begin_durable_cas_upload_admitted(
            &guard,
            &operator,
            "external-content-import",
            &key,
            None,
        )?;
    // GC is excluded until the complete verified manifest is durably staged.
    // Typed manifest edges retain ordinary CAS blobs without duplicating the
    // closure in this receipt. Preserve the shared binding contract's explicit
    // large-object import authority independently of those reachability edges.
    let (manifest, kind) = match request.storage {
        ImportStorage::Content => (
            serde_json::to_value(selected.content_manifest(&cas)?)?,
            ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
        ),
        ImportStorage::LargeContent => {
            let manifest = selected.large_manifest(&cas, &large_store)?;
            for entry in &manifest.entries {
                if let Some(file_sha256) = entry.file_sha256.as_deref() {
                    large_store.verify_manifest_commitment(entry)?;
                    stage.protect_large_object_hash(&guard, file_sha256)?;
                }
            }
            (
                manifest.to_value()?,
                ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
            )
        }
    };
    // Store the complete verified value before acknowledging its durable root.
    // A failed CAS write must not leave a staged reference to an absent object.
    let manifest_hash = cas.store_object(&manifest)?;
    if product
        .and_then(|product| product.expected_manifest_hash.as_deref())
        .is_some_and(|expected| expected != manifest_hash)
    {
        bail!("retained product contradicts expected_manifest_hash");
    }
    stage.protect_cas_closure(&guard, [manifest_hash.as_str()], std::iter::empty())?;
    Ok(Some(ImportResponse {
        staging_id: stage.staging_id().to_owned(),
        request_digest: digest,
        manifest_hash,
        manifest_kind: kind.to_owned(),
        entry_count: selected.entry_count(),
        total_bytes: selected.total_bytes(),
    }))
}

fn authorize_result(
    root: &ryeos_state::ThreadSnapshot,
    thread: &ryeos_state::ThreadSnapshot,
    request: &RetainedResultImportRequest,
    operator: &str,
) -> anyhow::Result<()> {
    authorize_terminal_result(
        root,
        thread,
        &request.chain_root_id,
        &request.thread_id,
        &request.result_project_snapshot_hash,
        operator,
    )
}

pub(super) fn authorize_terminal_result(
    root: &ryeos_state::ThreadSnapshot,
    thread: &ryeos_state::ThreadSnapshot,
    chain_root_id: &str,
    thread_id: &str,
    result_snapshot_hash: &str,
    operator: &str,
) -> anyhow::Result<()> {
    if root.thread_id != chain_root_id
        || root.chain_root_id != chain_root_id
        || root.requested_by.as_deref() != Some(operator)
        || thread.chain_root_id != chain_root_id
        || thread.thread_id != thread_id
        || thread.requested_by.as_deref() != Some(operator)
    {
        bail!("retained result execution is not owned at the requested coordinate");
    }
    // The trust-verified thread snapshot carries the lifecycle owner's
    // terminal classification. Outcome labels belong to the admitted
    // execution implementation; do not introduce an outcome vocabulary or
    // reclassify subprocess/runtime results at this generic import boundary.
    if thread.status != ryeos_state::objects::thread_snapshot::ThreadStatus::Completed
        || thread.error.is_some()
        || thread.finished_at.is_none()
    {
        bail!("retained result execution has no successful terminal authority");
    }
    if !thread
        .project_authority
        .records_terminal_project_generation()
        || thread.admitted_launch_capsule_hash.is_none()
        || thread.result_project_snapshot_hash.as_deref() != Some(result_snapshot_hash)
    {
        bail!("execution does not attest the exact retained result snapshot");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::objects::thread_snapshot::{ThreadSnapshotBuilder, ThreadStatus};
    use ryeos_state::objects::{
        EnvironmentAuthority, ExecutionProjectAuthority, PinnedProjectRealization,
        PinnedTerminalPublication,
    };

    fn fixture() -> (
        ryeos_state::ThreadSnapshot,
        RetainedResultImportRequest,
        String,
    ) {
        let owner = format!("fp:{}", "a".repeat(64));
        let mut thread = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "tool",
            "tool:test/build",
            "native:test",
        )
        .build();
        thread.requested_by = Some(owner.clone());
        thread.status = ThreadStatus::Completed;
        thread.outcome_code = Some("success".to_owned());
        thread.finished_at = Some("2026-09-06T00:00:00Z".to_owned());
        thread.admitted_launch_capsule_hash = Some("b".repeat(64));
        thread.result_project_snapshot_hash = Some("c".repeat(64));
        thread.project_authority = ExecutionProjectAuthority::pinned(
            "d".repeat(64),
            None,
            "e".repeat(64),
            PinnedProjectRealization::Cow {
                terminal_publication: PinnedTerminalPublication::RetainResult,
            },
            EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        let request = RetainedResultImportRequest {
            chain_root_id: thread.chain_root_id.clone(),
            thread_id: thread.thread_id.clone(),
            result_project_snapshot_hash: "c".repeat(64),
            path: "dist/runtime".to_owned(),
            shape: ImportShape::Tree,
            storage: ImportStorage::Content,
            maximum_bytes: 1024,
            expected_file_sha256: None,
        };
        (thread, request, owner)
    }

    #[test]
    fn retained_result_requires_exact_terminal_owner_authority() {
        let (thread, request, owner) = fixture();
        authorize_result(&thread, &thread, &request, &owner).unwrap();
        assert!(authorize_result(&thread, &thread, &request, &"f".repeat(64)).is_err());
        for alter in [
            |t: &mut ryeos_state::ThreadSnapshot| t.status = ThreadStatus::Running,
            |t: &mut ryeos_state::ThreadSnapshot| t.status = ThreadStatus::Continued,
            |t: &mut ryeos_state::ThreadSnapshot| t.status = ThreadStatus::Failed,
            |t: &mut ryeos_state::ThreadSnapshot| t.finished_at = None,
            |t: &mut ryeos_state::ThreadSnapshot| {
                t.error = Some(serde_json::json!({"code":"failed"}))
            },
            |t: &mut ryeos_state::ThreadSnapshot| t.admitted_launch_capsule_hash = None,
            |t: &mut ryeos_state::ThreadSnapshot| t.result_project_snapshot_hash = None,
            |t: &mut ryeos_state::ThreadSnapshot| {
                t.result_project_snapshot_hash = Some("f".repeat(64))
            },
            |t: &mut ryeos_state::ThreadSnapshot| {
                t.project_authority = ExecutionProjectAuthority::PROJECTLESS
            },
            |t: &mut ryeos_state::ThreadSnapshot| t.chain_root_id = "T-other".into(),
            |t: &mut ryeos_state::ThreadSnapshot| t.thread_id = "T-other".into(),
            |t: &mut ryeos_state::ThreadSnapshot| t.requested_by = Some("f".repeat(64)),
        ] {
            let mut bad = thread.clone();
            alter(&mut bad);
            assert!(authorize_result(&thread, &bad, &request, &owner).is_err());
        }
        let mut foreign_root = thread.clone();
        foreign_root.requested_by = Some("f".repeat(64));
        assert!(authorize_result(&foreign_root, &thread, &request, &owner).is_err());
    }

    #[test]
    fn retained_result_uses_terminal_classification_not_outcome_vocabulary() {
        let (root, request, owner) = fixture();
        for outcome in [
            Some("success"),
            Some("exit:0"),
            Some("producer:assembled"),
            None,
        ] {
            let mut thread = root.clone();
            thread.outcome_code = outcome.map(str::to_owned);
            authorize_result(&root, &thread, &request, &owner).unwrap();
        }
    }

    #[test]
    fn retained_result_outcome_labels_cannot_authorize_noncompleted_executions() {
        let (root, request, owner) = fixture();
        for status in [
            ThreadStatus::Created,
            ThreadStatus::Running,
            ThreadStatus::Failed,
            ThreadStatus::Cancelled,
            ThreadStatus::Killed,
            ThreadStatus::TimedOut,
            ThreadStatus::Continued,
        ] {
            for outcome in ["success", "exit:0"] {
                let mut thread = root.clone();
                thread.status = status;
                thread.outcome_code = Some(outcome.to_owned());
                assert!(authorize_result(&root, &thread, &request, &owner).is_err());
            }
        }
    }

    #[test]
    fn import_source_wire_is_explicit_and_disjoint() {
        let (_, request, _) = fixture();
        let mut retained = serde_json::to_value(request).unwrap();
        retained["source"] = serde_json::json!("retained_result");
        assert!(serde_json::from_value::<ImportRequest>(retained.clone()).is_ok());
        retained["root"] = serde_json::json!("host");
        assert!(serde_json::from_value::<ImportRequest>(retained).is_err());
        let mut filesystem = serde_json::json!({"source":"filesystem", "root":"exports", "path":"file",
            "shape":"file", "storage":"content", "maximum_bytes":1024});
        assert!(serde_json::from_value::<ImportRequest>(filesystem.clone()).is_ok());
        filesystem["result_project_snapshot_hash"] = serde_json::json!("c".repeat(64));
        assert!(serde_json::from_value::<ImportRequest>(filesystem.clone()).is_err());
        filesystem
            .as_object_mut()
            .unwrap()
            .remove("result_project_snapshot_hash");
        filesystem.as_object_mut().unwrap().remove("source");
        assert!(serde_json::from_value::<ImportRequest>(filesystem).is_err());
    }

    #[test]
    fn import_commands_bind_both_sources_through_the_generic_contract() {
        let service: serde_json::Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/core/.ai/services/external-content/import.yaml"
        )))
        .unwrap();
        let contract =
            ryeos_runtime::command::InvocationInputContract::from_lightweight_schema_value(
                &service["schema"],
            )
            .unwrap()
            .unwrap();
        let cases = [
            (
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../bundles/core/.ai/node/commands/external-content-import.yaml"
                )),
                vec![
                    "exports".to_owned(),
                    "dist/tool".into(),
                    "file".into(),
                    "content".into(),
                    "1024".into(),
                ],
                "filesystem",
            ),
            (
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../bundles/core/.ai/node/commands/external-content-import-result.yaml"
                )),
                vec![
                    "T-root".to_owned(),
                    "T-root".into(),
                    "a".repeat(64),
                    "dist/tool".into(),
                    "file".into(),
                    "content".into(),
                    "1024".into(),
                ],
                "retained_result",
            ),
        ];
        for (yaml, args, source) in cases {
            let command: ryeos_runtime::command::CommandDef = serde_yaml::from_str(yaml).unwrap();
            for with_digest in [false, true] {
                let mut argv = args.clone();
                if with_digest {
                    argv.push("b".repeat(64));
                }
                let value = ryeos_runtime::arg_binder::bind_argv_with_command_and_contract(
                    &argv,
                    Some(&command),
                    Some(&contract),
                )
                .unwrap();
                assert_eq!(value["source"], source);
                assert_eq!(value["maximum_bytes"], 1024);
                assert_eq!(value.get("expected_file_sha256").is_some(), with_digest);
                serde_json::from_value::<ImportRequest>(value).unwrap();
            }
        }
    }
}
