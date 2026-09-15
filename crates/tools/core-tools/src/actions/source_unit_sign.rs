//! Conditional signing for one kind-declared executable source unit.
//!
//! The item descriptor remains the authority root. This module only signs the
//! exact file set that ordinary source-closure admission would capture for an
//! `item_namespace` / `owner_signed_files` contract after the independently
//! signed executor chain has selected that policy. Auxiliary files are not
//! parsed as runnable items and gain no canonical references of their own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use ryeos_engine::kind_registry::{
    KindSchema, SourceClosureLocationDecl, SourceClosureTestimonyDecl,
};
use ryeos_engine::source_closure::{
    CapturedSourceCandidate, ExecutorSourceLocation, ExecutorSourcePolicy, SourceRootRequest,
    SourceRootSelection,
};

/// Sign the exact currently selected source unit when both signed contracts
/// opt in. `None` means the executor chain did not declare source ownership,
/// matching ordinary admission's no-source-closure path.
pub(super) fn sign_owner_signed_source_unit(
    source_root: &Path,
    canonical_ref: &str,
    kind_name: &str,
    kind_schema: &KindSchema,
    expected_source_digest: &str,
    configured_ignore: &ryeos_state::ignore::IgnoreMatcher,
    executor_policy: Option<&ExecutorSourcePolicy>,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<Option<SourceUnitSignResult>> {
    let Some(contract) = kind_schema
        .execution()
        .and_then(|execution| execution.source_closure.as_ref())
    else {
        return Ok(None);
    };
    if !matches!(
        (&contract.location, contract.testimony),
        (
            SourceClosureLocationDecl::ItemNamespace,
            SourceClosureTestimonyDecl::OwnerSignedFiles
        )
    ) {
        return Ok(None);
    }
    let Some(executor_policy) = executor_policy else {
        return Ok(None);
    };
    if !matches!(
        executor_policy.location,
        ExecutorSourceLocation::ItemNamespace
    ) {
        bail!("executor source policy exceeds the signed kind location ceiling");
    }

    let source = ryeos_app::source_closure_admission::DirectorySourceContent::new(
        source_root,
        format!("source-unit-sign:{canonical_ref}:{expected_source_digest}"),
        configured_ignore,
    )?;
    let (request, root_entry) = ryeos_app::source_closure_admission::item_namespace_source_request(
        &source,
        kind_name,
        kind_schema,
        canonical_ref,
        expected_source_digest,
        contract.max_file_bytes,
    )?;
    let before = ryeos_engine::source_closure::capture_source_candidate(
        &source,
        std::slice::from_ref(&request),
        contract,
    )?;
    require_root_entry(&before, &root_entry)?;
    require_expected_root_digest(&before.manifest, &root_entry, expected_source_digest)?;
    let plan = preflight_plan(
        &before,
        &request,
        kind_schema,
        source_root,
        &root_entry,
        contract.max_file_bytes,
        contract.max_total_bytes,
        signing_key,
    )?;

    let bare_id = canonical_ref
        .split_once(':')
        .map(|(_, bare_id)| bare_id)
        .ok_or_else(|| anyhow!("source owner ref is not canonical"))?;
    let mut changed_files = 0usize;
    let mut root_published = false;
    let mut durability_uncertain = false;
    for (logical_path, entry) in &plan {
        let is_root = logical_path == &root_entry;
        if !entry.publish {
            continue;
        }
        let outcome = super::sign::sign_in_place_with_key(
            &entry.absolute_path,
            &entry.validated_content,
            &entry.envelope,
            signing_key,
            is_root.then_some((kind_schema, bare_id)),
        )?;
        changed_files += usize::from(matches!(outcome, super::sign::SignOutcome::Signed { .. }));
        durability_uncertain |= outcome.durability_uncertain();
        if is_root {
            root_published = true;
        }
    }

    let after = ryeos_engine::source_closure::capture_source_candidate(
        &source,
        std::slice::from_ref(&request),
        contract,
    )?;
    require_root_entry(&after, &root_entry)?;
    verify_published_unit(&after, &request, kind_schema, &plan, signing_key)?;
    let signed_root_digest = after
        .manifest
        .entries
        .iter()
        .find(|entry| entry.root == "source" && entry.path == root_entry)
        .map(|entry| entry.blob_hash.as_str())
        .ok_or_else(|| anyhow!("published source unit lost its canonical owner item"))?;
    let (selected_after, root_after) =
        ryeos_app::source_closure_admission::item_namespace_source_request(
            &source,
            kind_name,
            kind_schema,
            canonical_ref,
            signed_root_digest,
            contract.max_file_bytes,
        )?;
    if selected_after != request || root_after != root_entry {
        bail!("canonical source selection changed during source unit publication");
    }
    if !root_published {
        bail!("source unit did not publish its canonical owner item");
    }
    Ok(Some(SourceUnitSignResult {
        changed_files,
        durability_uncertain,
    }))
}

pub(super) struct SourceUnitSignResult {
    pub changed_files: usize,
    pub durability_uncertain: bool,
}

#[derive(Debug)]
struct PlannedSourceFile {
    absolute_path: PathBuf,
    validated_content: String,
    canonical_body: String,
    envelope: ryeos_engine::contracts::SignatureEnvelope,
    normalized_mode: u32,
    publish: bool,
}

fn preflight_plan(
    candidate: &CapturedSourceCandidate,
    request: &SourceRootRequest,
    kind_schema: &KindSchema,
    source_root: &Path,
    root_entry: &str,
    max_file_bytes: u64,
    max_total_bytes: u64,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<BTreeMap<String, PlannedSourceFile>> {
    if candidate.manifest.entries.len() != candidate.blobs.len() {
        bail!("source capture returned an incoherent entry/blob set");
    }
    let source_authority = lillux::PinnedDirectory::open(source_root)?
        .ok_or_else(|| anyhow!("source content root is unavailable during preflight"))?;
    let mut plan = BTreeMap::new();
    let mut projected_total_bytes = 0u64;
    for (entry, blob) in candidate.manifest.entries.iter().zip(&candidate.blobs) {
        if entry.root != request.id || entry.blob_hash != blob.blob_hash {
            bail!("source capture returned an incoherent ordered entry/blob set");
        }
        let relative = selected_path(request, &entry.path)?;
        let absolute_path = source_root.join(&relative);
        let extension = relative
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .ok_or_else(|| {
                anyhow!(
                    "source unit file has no UTF-8 extension: {}",
                    relative.display()
                )
            })?;
        let format = kind_schema.resolved_format_for(&extension).ok_or_else(|| {
            anyhow!(
                "source unit file extension `{extension}` is not owned by kind `{}`: {}",
                kind_schema.directory,
                relative.display()
            )
        })?;
        let validated_content = String::from_utf8(blob.bytes.clone())
            .with_context(|| format!("source unit file is not UTF-8: {}", relative.display()))?;
        let (canonical_body, _) = lillux::signature::strip_canonical_signature_with_envelope(
            &validated_content,
            &format.signature.prefix,
            format.signature.suffix.as_deref(),
            format.signature.after_shebang,
        )
        .with_context(|| {
            format!(
                "source unit file has a non-canonical signature envelope: {}",
                relative.display()
            )
        })?;
        let kind_root = Path::new(ryeos_engine::AI_DIR).join(&kind_schema.directory);
        let kind_relative = relative.strip_prefix(&kind_root).map_err(|_| {
            anyhow!(
                "source unit file escaped its kind-owned namespace: {}",
                relative.display()
            )
        })?;
        let is_root = entry.path == root_entry;
        let independently_addressable = !kind_schema.excludes_relative_path(kind_relative);
        let already_valid = valid_signature_for_owner(
            &validated_content,
            &canonical_body,
            &format.signature,
            signing_key,
        );
        if independently_addressable && !already_valid {
            // The canonical owner itself is allowed to be the descriptor this
            // operation has already parser- and path-validated. Every sibling
            // descriptor must have passed the ordinary item signer first; an
            // auxiliary-source path never becomes a raw item-signing surface.
            if !is_root {
                bail!(
                    "source unit contains an independently addressable item that was not validated and signed first: {}",
                    relative.display()
                );
            }
        }
        let projected_bytes = if already_valid {
            validated_content.len() as u64
        } else {
            lillux::signature::sign_content_with_options(
                &canonical_body,
                signing_key,
                &format.signature.prefix,
                format.signature.suffix.as_deref(),
                format.signature.after_shebang,
            )
            .len() as u64
        };
        if projected_bytes > max_file_bytes {
            bail!(
                "signed source unit file would exceed its kind-owned byte ceiling: {}",
                relative.display()
            );
        }
        projected_total_bytes = projected_total_bytes
            .checked_add(projected_bytes)
            .ok_or_else(|| anyhow!("signed source unit byte count overflow"))?;
        if projected_total_bytes > max_total_bytes {
            bail!("signed source unit would exceed its kind-owned aggregate byte ceiling");
        }
        let file = source_authority
            .open_pinned_regular_descendant(&relative, false)?
            .ok_or_else(|| anyhow!("source unit file disappeared during preflight"))?;
        let observation = file.observation()?;
        if observation.full_permission_mode()? & 0o7000 != 0 {
            bail!(
                "sign refuses a source unit file with set-id or sticky permission bits: {}",
                relative.display()
            );
        }
        if plan
            .insert(
                entry.path.clone(),
                PlannedSourceFile {
                    absolute_path,
                    validated_content,
                    canonical_body,
                    envelope: format.signature,
                    normalized_mode: match entry.mode {
                        ryeos_state::objects::SourceFileMode::ReadOnly => 0o644,
                        ryeos_state::objects::SourceFileMode::Executable => 0o755,
                    },
                    publish: !independently_addressable || is_root,
                },
            )
            .is_some()
        {
            bail!("source capture returned a duplicate logical path");
        }
    }
    source_authority.ensure_path_binding()?;
    Ok(plan)
}

fn valid_signature_for_owner(
    content: &str,
    canonical_body: &str,
    envelope: &ryeos_engine::contracts::SignatureEnvelope,
    signing_key: &lillux::crypto::SigningKey,
) -> bool {
    let Some(header) = ryeos_engine::item_resolution::parse_signature_header(content, envelope)
    else {
        return false;
    };
    let verifying_key = signing_key.verifying_key();
    let fingerprint = lillux::signature::compute_fingerprint(&verifying_key);
    lillux::signature::is_valid_signature_for(
        &header.content_hash,
        &header.signature_b64,
        &header.signer_fingerprint,
        lillux::signature::content_to_sign(canonical_body, envelope.after_shebang),
        &verifying_key,
        &fingerprint,
    )
}

fn verify_published_unit(
    candidate: &CapturedSourceCandidate,
    request: &SourceRootRequest,
    kind_schema: &KindSchema,
    plan: &BTreeMap<String, PlannedSourceFile>,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<()> {
    if candidate.manifest.entries.len() != candidate.blobs.len()
        || candidate.manifest.entries.len() != plan.len()
    {
        bail!("source unit changed while signatures were being published");
    }
    let verifying_key = signing_key.verifying_key();
    let fingerprint = lillux::signature::compute_fingerprint(&verifying_key);
    let mut seen = BTreeMap::new();
    for (entry, blob) in candidate.manifest.entries.iter().zip(&candidate.blobs) {
        if entry.root != request.id || entry.blob_hash != blob.blob_hash {
            bail!("published source capture returned an incoherent entry/blob set");
        }
        let planned = plan
            .get(&entry.path)
            .ok_or_else(|| anyhow!("source unit gained an unvalidated file during publication"))?;
        let relative = selected_path(request, &entry.path)?;
        let extension = relative
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .ok_or_else(|| anyhow!("published source unit file has no UTF-8 extension"))?;
        let format = kind_schema
            .resolved_format_for(&extension)
            .ok_or_else(|| anyhow!("published source unit file has an unowned extension"))?;
        if format.signature != planned.envelope {
            bail!("source unit signature envelope changed during publication");
        }
        let content =
            std::str::from_utf8(&blob.bytes).context("published source unit file is not UTF-8")?;
        let (body, header) = lillux::signature::strip_canonical_signature_with_envelope(
            content,
            &planned.envelope.prefix,
            planned.envelope.suffix.as_deref(),
            planned.envelope.after_shebang,
        )?;
        if body != planned.canonical_body {
            bail!("source unit body changed during signature publication");
        }
        let header = header.ok_or_else(|| anyhow!("published source unit file is unsigned"))?;
        let signed_body = lillux::signature::content_to_sign(&body, planned.envelope.after_shebang);
        if !lillux::signature::is_valid_signature_for(
            &header.content_hash,
            &header.signature_b64,
            &header.signer_fingerprint,
            signed_body,
            &verifying_key,
            &fingerprint,
        ) {
            bail!("published source unit file is not signed by the source owner");
        }
        let normalized_mode = match entry.mode {
            ryeos_state::objects::SourceFileMode::ReadOnly => 0o644,
            ryeos_state::objects::SourceFileMode::Executable => 0o755,
        };
        if normalized_mode != planned.normalized_mode {
            bail!("source unit mode changed during signature publication");
        }
        if seen.insert(entry.path.clone(), ()).is_some() {
            bail!("published source unit contains a duplicate logical path");
        }
    }
    if seen.len() != plan.len() {
        bail!("source unit lost a validated file during publication");
    }
    Ok(())
}

fn selected_path(request: &SourceRootRequest, entry: &str) -> Result<PathBuf> {
    ryeos_state::objects::validate_canonical_project_relative_path(entry)?;
    let entry = Path::new(entry);
    Ok(match &request.selection {
        SourceRootSelection::Tree { prefix } => prefix.join(entry),
        SourceRootSelection::File { path } => {
            let name = path
                .file_name()
                .ok_or_else(|| anyhow!("source file selection has no file name"))?;
            if entry != Path::new(name) {
                bail!("source file selection returned a contradictory entry path");
            }
            path.clone()
        }
    })
}

fn require_root_entry(candidate: &CapturedSourceCandidate, root_entry: &str) -> Result<()> {
    if !candidate
        .manifest
        .entries
        .iter()
        .any(|entry| entry.root == "source" && entry.path == root_entry)
    {
        bail!("source unit does not contain its canonical owner item");
    }
    Ok(())
}

fn require_expected_root_digest(
    manifest: &ryeos_state::objects::SourceClosureManifest,
    root_entry: &str,
    expected_source_digest: &str,
) -> Result<()> {
    let captured_digest = manifest
        .entries
        .iter()
        .find(|entry| entry.root == "source" && entry.path == root_entry)
        .map(|entry| entry.blob_hash.as_str())
        .ok_or_else(|| anyhow!("source unit does not contain its canonical owner item"))?;
    if captured_digest != expected_source_digest {
        bail!("captured source owner changed after descriptor validation and signing");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use ryeos_engine::contracts::{SignatureEnvelope, ValueShape};
    use ryeos_engine::kind_registry::{ExecutionSchema, ExtensionSpec};
    use ryeos_engine::source_closure::SourceMaterialization;
    use ryeos_state::objects::{LogicalSourceRoot, SourceClosureFile, SourceClosureManifest};

    fn schema(max_file_bytes: u64, max_total_bytes: u64) -> KindSchema {
        let execution: ExecutionSchema = serde_json::from_value(serde_json::json!({
            "aliases": {"@terminal": "tool:test/terminal"},
            "source_closure": {
                "derived": ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY,
                "location": {"type": "item_namespace"},
                "testimony": "owner_signed_files",
                "max_files": 8,
                "max_total_bytes": max_total_bytes,
                "max_file_bytes": max_file_bytes,
                "max_depth": 8
            }
        }))
        .unwrap();
        KindSchema {
            directory: "tools".to_owned(),
            excluded_directories: vec!["lib".to_owned()],
            extensions: vec![
                ExtensionSpec {
                    ext: ".yaml".to_owned(),
                    parser: "parser:test/yaml".to_owned(),
                    signature: SignatureEnvelope {
                        prefix: "#".to_owned(),
                        suffix: None,
                        after_shebang: false,
                    },
                },
                ExtensionSpec {
                    ext: ".py".to_owned(),
                    parser: "parser:test/python".to_owned(),
                    signature: SignatureEnvelope {
                        prefix: "#".to_owned(),
                        suffix: None,
                        after_shebang: true,
                    },
                },
            ],
            extraction_rules: Default::default(),
            resolution: Vec::new(),
            effective_trust: Default::default(),
            content: None,
            execution: Some(execution),
            composed_value_contract: ValueShape::any_mapping(),
            composer: "handler:test/identity".to_owned(),
            composer_config: serde_json::Value::Null,
            runtime: None,
            inventory_kinds: Vec::new(),
            inventory_schema_keys: Vec::new(),
            inventory_policy: Default::default(),
        }
    }

    fn policy() -> ExecutorSourcePolicy {
        ExecutorSourcePolicy {
            location: ExecutorSourceLocation::ItemNamespace,
            load_roots: Vec::new(),
            materialization: SourceMaterialization::ReadOnly,
        }
    }

    fn ignores(patterns: &[&str]) -> ryeos_state::ignore::IgnoreMatcher {
        ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
            patterns: patterns.iter().map(|value| (*value).to_owned()).collect(),
        })
        .unwrap()
    }

    fn assert_signed_by(
        path: &Path,
        envelope: &SignatureEnvelope,
        key: &lillux::crypto::SigningKey,
    ) {
        let content = std::fs::read_to_string(path).unwrap();
        let (body, header) = lillux::signature::strip_canonical_signature_with_envelope(
            &content,
            &envelope.prefix,
            envelope.suffix.as_deref(),
            envelope.after_shebang,
        )
        .unwrap();
        let header = header.unwrap();
        let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
        assert!(lillux::signature::is_valid_signature_for(
            &header.content_hash,
            &header.signature_b64,
            &header.signer_fingerprint,
            lillux::signature::content_to_sign(&body, envelope.after_shebang),
            &key.verifying_key(),
            &fingerprint,
        ));
    }

    #[test]
    fn signs_the_admission_selected_unit_without_parsing_auxiliary_files_as_items() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = temp.path().join(".ai/tools/example");
        std::fs::create_dir_all(namespace.join("lib")).unwrap();
        let owner = namespace.join("program.yaml");
        let helper = namespace.join("lib/helper.py");
        let ignored = namespace.join("ignored.py");
        std::fs::write(&owner, "name: program\n").unwrap();
        std::fs::write(&helper, "#!/usr/bin/python3\nVALUE = 1\n").unwrap();
        std::fs::write(&ignored, "VALUE = 2\n").unwrap();
        let owner_digest = lillux::sha256_hex(&std::fs::read(&owner).unwrap());
        let key = lillux::crypto::SigningKey::generate(&mut OsRng);
        let schema = schema(4096, 16 * 1024);

        let result = sign_owner_signed_source_unit(
            temp.path(),
            "tool:example/program",
            "tool",
            &schema,
            &owner_digest,
            &ignores(&["ignored.py"]),
            Some(&policy()),
            &key,
        )
        .unwrap()
        .unwrap();

        assert_eq!(result.changed_files, 2);
        assert_signed_by(&owner, &schema.extensions[0].signature, &key);
        assert_signed_by(&helper, &schema.extensions[1].signature, &key);
        assert_eq!(std::fs::read_to_string(ignored).unwrap(), "VALUE = 2\n");
    }

    #[test]
    fn absent_executor_source_scope_does_not_sign_the_kind_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = temp.path().join(".ai/tools/example");
        std::fs::create_dir_all(&namespace).unwrap();
        let owner = namespace.join("program.yaml");
        let original = "name: program\n";
        std::fs::write(&owner, original).unwrap();
        let key = lillux::crypto::SigningKey::generate(&mut OsRng);

        let result = sign_owner_signed_source_unit(
            temp.path(),
            "tool:example/program",
            "tool",
            &schema(4096, 16 * 1024),
            &lillux::sha256_hex(original.as_bytes()),
            &ignores(&[]),
            None,
            &key,
        )
        .unwrap();

        assert!(result.is_none());
        assert_eq!(std::fs::read_to_string(owner).unwrap(), original);
    }

    #[test]
    fn refuses_signature_growth_before_writing_any_source_unit_file() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = temp.path().join(".ai/tools/example");
        std::fs::create_dir_all(namespace.join("lib")).unwrap();
        let owner = namespace.join("program.yaml");
        let helper = namespace.join("lib/helper.py");
        let owner_body = "name: program\n";
        let helper_body = "VALUE = 1\n";
        std::fs::write(&owner, owner_body).unwrap();
        std::fs::write(&helper, helper_body).unwrap();
        let key = lillux::crypto::SigningKey::generate(&mut OsRng);
        let ceiling = owner_body.len().max(helper_body.len()) as u64;

        let error = sign_owner_signed_source_unit(
            temp.path(),
            "tool:example/program",
            "tool",
            &schema(ceiling, ceiling * 2),
            &lillux::sha256_hex(owner_body.as_bytes()),
            &ignores(&[]),
            Some(&policy()),
            &key,
        )
        .err()
        .expect("signature growth must fail before publication");

        assert!(error.to_string().contains("would exceed"));
        assert_eq!(std::fs::read_to_string(owner).unwrap(), owner_body);
        assert_eq!(std::fs::read_to_string(helper).unwrap(), helper_body);
    }

    #[test]
    fn retained_long_signatures_are_budgeted_before_any_helper_write() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = temp.path().join(".ai/tools/example");
        std::fs::create_dir_all(namespace.join("lib")).unwrap();
        let owner = namespace.join("program.yaml");
        let retained_helper = namespace.join("lib/retained.py");
        let new_helper = namespace.join("lib/new.py");
        let owner_body = "name: program\n";
        let retained_body = "VALUE = 1\n";
        let new_body = "VALUE = 2\n";
        let key = lillux::crypto::SigningKey::generate(&mut OsRng);
        let long_timestamp = "x".repeat(1024);
        let owner_signed = lillux::signature::sign_content_at_with_options(
            owner_body,
            &key,
            "#",
            None,
            &long_timestamp,
            false,
        );
        let retained_signed = lillux::signature::sign_content_at_with_options(
            retained_body,
            &key,
            "#",
            None,
            &long_timestamp,
            true,
        );
        std::fs::write(&owner, &owner_signed).unwrap();
        std::fs::write(&retained_helper, &retained_signed).unwrap();
        std::fs::write(&new_helper, new_body).unwrap();
        let current_total = (owner_signed.len() + retained_signed.len() + new_body.len()) as u64;

        let error = sign_owner_signed_source_unit(
            temp.path(),
            "tool:example/program",
            "tool",
            &schema(4096, current_total),
            &lillux::sha256_hex(owner_signed.as_bytes()),
            &ignores(&[]),
            Some(&policy()),
            &key,
        )
        .err()
        .expect("retained long signatures plus a new signature exceed the aggregate bound");

        assert!(error.to_string().contains("aggregate byte ceiling"));
        assert_eq!(std::fs::read_to_string(owner).unwrap(), owner_signed);
        assert_eq!(
            std::fs::read_to_string(retained_helper).unwrap(),
            retained_signed
        );
        assert_eq!(std::fs::read_to_string(new_helper).unwrap(), new_body);
    }

    #[test]
    fn refuses_an_unsigned_sibling_item_before_signing_auxiliary_source() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = temp.path().join(".ai/tools/example");
        std::fs::create_dir_all(namespace.join("lib")).unwrap();
        let owner = namespace.join("program.yaml");
        let sibling = namespace.join("runtime.yaml");
        let helper = namespace.join("lib/helper.py");
        let owner_body = "name: program\n";
        let sibling_body = "name: runtime\n";
        let helper_body = "VALUE = 1\n";
        std::fs::write(&owner, owner_body).unwrap();
        std::fs::write(&sibling, sibling_body).unwrap();
        std::fs::write(&helper, helper_body).unwrap();
        let key = lillux::crypto::SigningKey::generate(&mut OsRng);

        let error = sign_owner_signed_source_unit(
            temp.path(),
            "tool:example/program",
            "tool",
            &schema(4096, 16 * 1024),
            &lillux::sha256_hex(owner_body.as_bytes()),
            &ignores(&[]),
            Some(&policy()),
            &key,
        )
        .err()
        .expect("an unvalidated sibling item must not become auxiliary source");

        assert!(error.to_string().contains("independently addressable item"));
        assert_eq!(std::fs::read_to_string(owner).unwrap(), owner_body);
        assert_eq!(std::fs::read_to_string(sibling).unwrap(), sibling_body);
        assert_eq!(std::fs::read_to_string(helper).unwrap(), helper_body);
    }

    #[test]
    fn refuses_a_captured_root_that_differs_from_the_validated_owner() {
        let captured = "b".repeat(64);
        let manifest = SourceClosureManifest::new(
            vec![LogicalSourceRoot {
                id: "source".to_owned(),
            }],
            vec![SourceClosureFile {
                root: "source".to_owned(),
                path: "program.yaml".to_owned(),
                blob_hash: captured,
                size: 1,
                mode: ryeos_state::objects::SourceFileMode::ReadOnly,
            }],
        )
        .unwrap();

        let error =
            require_expected_root_digest(&manifest, "program.yaml", &"a".repeat(64)).unwrap_err();
        assert!(error.to_string().contains("captured source owner changed"));
    }
}
