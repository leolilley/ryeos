//! Signed runtime capability admission shared by execution and exact program preparation.

use ryeos_engine::contracts::ResolvedItem;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub fn mint_runtime_capability_caps(
    requires_value: Option<&Value>,
    resolved_item: &ResolvedItem,
    effective_trust_class: ryeos_engine::resolution::TrustClass,
    engine: &ryeos_engine::engine::Engine,
) -> Result<Vec<String>, String> {
    // (1) Requirement contract. No `requires:` → no runtime callback authority.
    let Some(requires_value) = requires_value else {
        return Ok(Vec::new());
    };
    let reqs = ryeos_bundle::runtime_authority::parse_runtime_requires(requires_value)
        .map_err(|err| format!("invalid `requires.capabilities`: {err}"))?;
    if reqs.manifest.runtime_authority.is_empty() {
        return Ok(Vec::new());
    }

    // Single source of truth for the bundle identity: the resolved canonical
    // ref. The callback token's `effective_bundle_id` is stamped from this same
    // value (see `effective_bundle_id_for_request`), so the caps minted here and
    // the token that carries them can never claim different bundles.
    let effective_bundle_id = crate::callback_token::effective_bundle_id_from_item_ref(
        &resolved_item.canonical_ref.to_string(),
    )
    .ok_or_else(|| {
        "runtime capability requirements need a bundle-qualified item ref".to_string()
    })?;
    ryeos_state::objects::validate_bundle_identifier("bundle_id", &effective_bundle_id)
        .map_err(|err| err.to_string())?;

    // Deep, segment-grammar validation of each requested resource. The static
    // shape (known keys, valid ops, non-empty arrays) was already enforced by
    // `parse_runtime_requires`; this checks the bundle-id segment grammar that
    // the cap-string scheme depends on.
    let requested_authority = &reqs.manifest.runtime_authority;
    for req in &requested_authority.bundle_events {
        ryeos_state::objects::validate_bundle_identifier("event_kind", &req.event_kind)
            .map_err(|err| err.to_string())?;
    }
    for req in &requested_authority.runtime_vault {
        crate::vault::validate_runtime_vault_segment("namespace", &req.namespace)
            .map_err(|err| err.to_string())?;
    }
    for req in &requested_authority.item_authoring {
        ryeos_bundle::runtime_authority::validate_item_author_pattern(&req.kind, &req.namespace)?;
    }

    // (2) Authority upper bound comes from the signed manifest at the exact
    // provenance boundary that supplied the item. Installed bundle authority
    // remains node-trusted. A live project may use its own signed manifest,
    // but only for a TrustedProject item physically below that exact project's
    // `.ai/` root and only with the request engine's effective project trust
    // store. Mixed-trust composition of an installed item therefore remains
    // unable to acquire project authority.
    let manifest = match resolved_item.source_space {
        ryeos_engine::contracts::ItemSpace::Bundle => {
            if effective_trust_class != ryeos_engine::resolution::TrustClass::TrustedBundle {
                return Err(format!(
                    "installed runtime capability requirements need TrustedBundle provenance; \
                     effective trust class is {effective_trust_class:?}"
                ));
            }
            let ai_dir = authoritative_runtime_authority_ai_dir(
                resolved_item,
                &effective_bundle_id,
                engine,
                &engine.node_trust_store,
            )?;
            ryeos_bundle::manifest::load_verified_manifest(
                &ai_dir,
                &effective_bundle_id,
                &engine.node_trust_store,
            )
            .map_err(|err| err.to_string())?
            .manifest
        }
        ryeos_engine::contracts::ItemSpace::Project => {
            if effective_trust_class != ryeos_engine::resolution::TrustClass::TrustedProject {
                return Err(format!(
                    "project runtime capability requirements need TrustedProject provenance; \
                     effective trust class is {effective_trust_class:?}"
                ));
            }
            let project_root = resolved_item
                .materialized_project_root
                .as_deref()
                .ok_or_else(|| {
                    "project runtime capability item has no materialized project root".to_string()
                })?;
            let ai_dir =
                authoritative_project_runtime_authority_ai_dir(resolved_item, project_root)?;
            let project_trust = engine
                .trust_store
                .with_project_keys(project_root)
                .map_err(|err| err.to_string())?;
            ryeos_bundle::manifest::load_verified_manifest(
                &ai_dir,
                &effective_bundle_id,
                project_trust.as_ref(),
            )
            .map_err(|err| err.to_string())?
            .manifest
        }
        ryeos_engine::contracts::ItemSpace::Node => {
            return Err(
                "node-local configuration cannot supply runtime capability authority".to_string(),
            );
        }
    };
    manifest.runtime_authority.validate()?;

    // Manifest-declared caps form the upper bound. Cap strings come from the
    // manifest declarations' own constructors (`runtime_authority`), so the
    // minter and the daemon callback services share one definition.
    let manifest_caps = manifest
        .runtime_authority
        .declared_caps(&effective_bundle_id);

    // (3) Subset check + mint exactly the requested subset. A wildcard-carrying
    // request must be backed by an identical manifest declaration, not merely
    // glob-matched — see `manifest_backs_requested_cap`.
    let requested =
        ryeos_bundle::runtime_authority::requested_runtime_caps(&reqs, &effective_bundle_id);
    let missing: Vec<String> = requested
        .iter()
        .filter(|requested_cap| {
            !ryeos_bundle::runtime_authority::manifest_backs_requested_cap(
                &manifest_caps,
                requested_cap,
            )
        })
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "requested runtime capabilities are not declared in the signed manifest \
             (authority upper bound): {}",
            missing.join(", ")
        ));
    }

    Ok(requested.into_iter().collect())
}

/// Bind project runtime authority to the exact live project resolution root.
/// This deliberately does not walk upward looking for a convenient manifest.
fn authoritative_project_runtime_authority_ai_dir(
    resolved_item: &ResolvedItem,
    project_root: &Path,
) -> Result<PathBuf, String> {
    if resolved_item.source_space != ryeos_engine::contracts::ItemSpace::Project
        || resolved_item.source_root != ryeos_engine::contracts::ItemSourceRoot::Project
    {
        return Err(format!(
            "project runtime capability requirements need exact Project source provenance; \
             found {:?} in {} space",
            resolved_item.source_root,
            resolved_item.source_space.as_str()
        ));
    }
    let canonical_project = std::fs::canonicalize(project_root).map_err(|err| {
        format!(
            "canonicalize runtime-authority project {}: {err}",
            project_root.display()
        )
    })?;
    let ai_dir = canonical_project.join(ryeos_engine::AI_DIR);
    let canonical_source = std::fs::canonicalize(&resolved_item.source_path).map_err(|err| {
        format!(
            "canonicalize resolved project runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    let source_metadata = std::fs::symlink_metadata(&resolved_item.source_path).map_err(|err| {
        format!(
            "stat resolved project runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    if source_metadata.file_type().is_symlink() || !source_metadata.file_type().is_file() {
        return Err(format!(
            "project runtime-authority item {} must be a regular file (symlinks rejected)",
            resolved_item.source_path.display()
        ));
    }
    if !canonical_source.starts_with(&ai_dir) {
        return Err(format!(
            "project runtime-authority item {} is outside the exact project .ai root {}",
            canonical_source.display(),
            ai_dir.display()
        ));
    }
    Ok(ai_dir)
}

/// Establish the only provenance permitted to mint daemon callback authority:
/// a regular, content-pinned, node-signed item below exactly one registered
/// installed bundle's `.ai/` directory. Returns that authoritative `.ai/`
/// directory for manifest loading.
fn authoritative_runtime_authority_ai_dir(
    resolved_item: &ResolvedItem,
    expected_bundle_id: &str,
    engine: &ryeos_engine::engine::Engine,
    node_trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<PathBuf, String> {
    if resolved_item.source_space != ryeos_engine::contracts::ItemSpace::Bundle {
        return Err(format!(
            "runtime capability requirements require installed TrustedBundle provenance; \
             item resolved from {} space",
            resolved_item.source_space.as_str()
        ));
    }

    let ryeos_engine::contracts::ItemSourceRoot::Bundle { name } = &resolved_item.source_root
    else {
        return Err(format!(
            "runtime capability requirements need exact Bundle source provenance; found {:?}",
            resolved_item.source_root
        ));
    };
    if name != expected_bundle_id {
        return Err(format!(
            "runtime-authority item ref names bundle {expected_bundle_id}, but typed source \
             provenance names bundle {name}"
        ));
    }
    let bundle_root = engine.registered_bundle_root(name).ok_or_else(|| {
        format!("typed runtime-authority bundle {name} is absent from the admitted generation")
    })?;
    let ai_dir = bundle_root.join(ryeos_engine::AI_DIR);
    let canonical_ai_dir = std::fs::canonicalize(&ai_dir).map_err(|err| {
        format!(
            "canonicalize typed runtime-authority bundle root {}: {err}",
            ai_dir.display()
        )
    })?;

    let source_metadata = std::fs::symlink_metadata(&resolved_item.source_path).map_err(|err| {
        format!(
            "stat resolved runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    if source_metadata.file_type().is_symlink() || !source_metadata.file_type().is_file() {
        return Err(format!(
            "runtime-authority item {} must be a regular installed file (symlinks rejected)",
            resolved_item.source_path.display()
        ));
    }

    let canonical_source = std::fs::canonicalize(&resolved_item.source_path).map_err(|err| {
        format!(
            "canonicalize resolved runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    if !canonical_source.starts_with(&canonical_ai_dir) {
        return Err(format!(
            "runtime-authority item {} contradicts typed bundle root {}",
            resolved_item.source_path.display(),
            canonical_ai_dir.display()
        ));
    }

    // Re-read once, pin it to the bytes that produced ResolvedItem metadata,
    // and verify the signature solely with persistent node trust. This keeps a
    // project key/caller overlay from turning an installed-path item into node
    // callback authority and detects a source replacement after resolution.
    let source = std::fs::read_to_string(&canonical_source).map_err(|err| {
        format!(
            "read resolved runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    let live_content_hash = ryeos_engine::item_resolution::content_hash(&source);
    if live_content_hash != resolved_item.content_hash {
        return Err(format!(
            "runtime-authority item {} changed after resolution (expected {}, found {})",
            resolved_item.source_path.display(),
            resolved_item.content_hash,
            live_content_hash
        ));
    }
    let signature_header = resolved_item.signature_header.as_ref().ok_or_else(|| {
        format!(
            "runtime-authority item {} is unsigned; installed TrustedBundle provenance is required",
            resolved_item.source_path.display()
        )
    })?;
    let (trust_class, _) = ryeos_engine::trust::verify_item_signature(
        &source,
        signature_header,
        &resolved_item.source_format.signature,
        node_trust_store,
    )
    .map_err(|err| {
        format!(
            "node verification failed for runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    if trust_class != ryeos_engine::contracts::TrustClass::Trusted {
        return Err(format!(
            "runtime-authority item {} is not signed by a node-trusted publisher",
            resolved_item.source_path.display()
        ));
    }

    Ok(canonical_ai_dir)
}
