//! Describing project items for an endorsement.
//!
//! `ryeos endorse` signs, in the operator's own process, a statement that
//! names the exact items the node's authoring key signed and the operator
//! reviewed. This module resolves the refs (or `.ai` file paths) the operator
//! typed to the facts that statement carries: the item's canonical ref, its
//! kind and the digest of its signed bytes after the signature line, which is
//! what the engine recomputes when it judges trust. It refuses an item that is
//! unsigned, changed since it was signed, or signed by anything but a pinned
//! authoring key: only those need an endorsement.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::trust::{SignerRole, SignerStatus, TrustStore};

use super::sign::{
    SignSource, derive_bare_id, glob_match_items, is_glob, load_offline_authoring, looks_path_arg,
    parse_sign_target, resolve_path_to_ref, source_ai_root, source_kind_dir,
};

/// One item an endorsement can name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DescribedItem {
    pub item_ref: String,
    pub kind: String,
    /// Digest of the signed bytes after the signature line.
    pub digest: String,
    /// Fingerprint of the key that signed the item.
    pub signer: String,
    pub path: PathBuf,
}

/// Resolve `item_refs` (canonical refs, globs, or paths under the project's
/// `.ai`) in `project` to the items an endorsement would name.
pub fn describe_project_items(
    app_root: &Path,
    item_refs: &[String],
    project: &Path,
) -> Result<Vec<DescribedItem>> {
    if item_refs.is_empty() {
        bail!("endorse requires at least one item ref");
    }
    let authoring = load_offline_authoring(app_root)?;
    describe_with(
        &authoring.kinds,
        &authoring.node_trust_store,
        item_refs,
        project,
    )
}

/// [`describe_project_items`] against an already-loaded kind registry and
/// node trust store.
pub fn describe_with(
    kinds: &KindRegistry,
    trust: &TrustStore,
    item_refs: &[String],
    project: &Path,
) -> Result<Vec<DescribedItem>> {
    if item_refs.is_empty() {
        bail!("endorse requires at least one item ref");
    }
    let source = SignSource::Project;
    let ai_root = source_ai_root(source, Some(project))?;
    let mut described: Vec<DescribedItem> = Vec::new();
    for item_ref in item_refs {
        let target = match parse_sign_target(item_ref) {
            Ok(target) => target,
            Err(error) => {
                let resolved = looks_path_arg(item_ref)
                    .then(|| resolve_path_to_ref(item_ref, source, Some(project), kinds))
                    .flatten();
                match resolved {
                    Some(resolved) => parse_sign_target(&resolved)?,
                    None => return Err(error),
                }
            }
        };
        let kind_schema = kinds
            .get(&target.kind)
            .ok_or_else(|| anyhow!("unknown kind `{}` — no kind schema registered", target.kind))?;
        let kind_dir = source_kind_dir(kind_schema, source, Some(project))?;
        let files: Vec<PathBuf> = if is_glob(&target.bare_id) {
            glob_match_items(&kind_dir, kind_schema, &target.bare_id)?
                .into_iter()
                .filter(|file| {
                    !crate::actions::runtime_owned::is_runtime_owned_file(file, &ai_root)
                        && file
                            .strip_prefix(&kind_dir)
                            .is_ok_and(|relative| !kind_schema.excludes_relative_path(relative))
                })
                .collect()
        } else {
            let found = kind_schema
                .extensions
                .iter()
                .map(|spec| kind_dir.join(format!("{}{}", target.bare_id, spec.ext)))
                .find(|candidate| candidate.is_file());
            match found {
                Some(path) => vec![path],
                None => bail!(
                    "item `{}:{}` not found in the project (searched {} with extensions {:?})",
                    target.kind,
                    target.bare_id,
                    kind_dir.display(),
                    kind_schema.extension_strs()
                ),
            }
        };
        if files.is_empty() {
            bail!(
                "no items matched `{}:{}` in the project (searched {})",
                target.kind,
                target.bare_id,
                kind_dir.display()
            );
        }
        for file in files {
            let bare_id = derive_bare_id(&file, &kind_dir, kind_schema)
                .ok_or_else(|| anyhow!("cannot derive the item id of {}", file.display()))?;
            let display_ref = format!("{}:{bare_id}", target.kind);
            let content = lillux::read_regular_file_bounded_no_follow(
                &file,
                ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES,
            )
            .with_context(|| format!("read {}", file.display()))?;
            let content = String::from_utf8(content)
                .with_context(|| format!("{display_ref} is not UTF-8"))?;
            let extension = file
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .ok_or_else(|| anyhow!("{} has no extension", file.display()))?;
            let format = kind_schema
                .resolved_format_for(&extension)
                .ok_or_else(|| anyhow!("extension `{extension}` is not registered for the kind"))?;
            let header =
                ryeos_engine::item_resolution::parse_signature_header(&content, &format.signature)
                    .ok_or_else(|| {
                        anyhow!(
                            "{display_ref} is unsigned; an endorsement names only items the \
                             authoring key signed"
                        )
                    })?;
            let actual =
                ryeos_engine::trust::content_hash_after_signature(&content, &format.signature)
                    .ok_or_else(|| anyhow!("{display_ref}: no locatable signature line"))?;
            if actual != header.content_hash {
                bail!("{display_ref} changed after it was signed; its signature does not match");
            }
            let pinned = trust
                .signer(&header.signer_fingerprint)
                .filter(|record| record.status != SignerStatus::Revoked);
            if pinned.is_none_or(|record| record.role != SignerRole::Authoring) {
                bail!(
                    "{display_ref} is signed by {}, which is not an authoring key pinned on this \
                     node; only items the authoring key signed need an endorsement",
                    header.signer_fingerprint
                );
            }
            if !described.iter().any(|item| item.item_ref == display_ref) {
                described.push(DescribedItem {
                    item_ref: display_ref,
                    kind: target.kind.clone(),
                    digest: actual,
                    signer: header.signer_fingerprint,
                    path: file,
                });
            }
        }
    }
    Ok(described)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use ryeos_engine::trust::{SignerRecord, SignerScope};

    fn record(key: &SigningKey, role: SignerRole, scope: SignerScope) -> SignerRecord {
        let mut record =
            SignerRecord::new(&key.verifying_key(), "t", role, scope, SignerStatus::Active);
        record.event = "e".repeat(64);
        record
    }

    struct Fixture {
        project: tempfile::TempDir,
        authoring: SigningKey,
        kinds: KindRegistry,
        trust: TrustStore,
    }

    fn fixture() -> Fixture {
        let authoring = SigningKey::from_bytes(&[3; 32]);
        let publishers = [
            ryeos_engine::test_support::platform_author_verifying_key(),
            ryeos_engine::test_support::dev_publisher_verifying_key(),
        ];
        let mut records: Vec<SignerRecord> = publishers
            .iter()
            .map(|key| {
                let mut record = SignerRecord::new(
                    key,
                    "publisher",
                    SignerRole::Publisher,
                    SignerScope::everywhere(),
                    SignerStatus::Active,
                );
                record.event = "e".repeat(64);
                record
            })
            .collect();
        records.push(record(
            &authoring,
            SignerRole::Authoring,
            SignerScope::authoring_default(),
        ));
        let trust =
            TrustStore::from_registry(SigningKey::from_bytes(&[1; 32]).verifying_key(), records)
                .unwrap();
        let core = ryeos_engine::test_support::core_bundle_root();
        let workflow = ryeos_engine::test_support::workflow_bundle_roots();
        let kind_roots: Vec<_> = std::iter::once(&core)
            .chain(workflow.iter())
            .map(|root| {
                root.join(ryeos_engine::AI_DIR)
                    .join(ryeos_engine::KIND_SCHEMAS_DIR)
            })
            .collect();
        let kinds = KindRegistry::load_base(&kind_roots, &trust).unwrap();
        Fixture {
            project: tempfile::tempdir().unwrap(),
            authoring,
            kinds,
            trust,
        }
    }

    fn plant(fx: &Fixture, id: &str, text: &str, signer: &SigningKey) -> PathBuf {
        let path = fx.project.path().join(format!(".ai/knowledge/{id}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content = format!("```yaml\nname: note\ncategory: \"acme\"\n```\n\n{text}\n");
        std::fs::write(
            &path,
            lillux::signature::sign_content(&content, signer, "<!--", Some("-->")),
        )
        .unwrap();
        path
    }

    #[test]
    fn describes_authoring_signed_items_by_ref_path_and_glob() {
        let fx = fixture();
        let path = plant(&fx, "acme/note", "one", &fx.authoring);
        plant(&fx, "acme/other", "two", &fx.authoring);
        let by_ref = describe_with(
            &fx.kinds,
            &fx.trust,
            &["knowledge:acme/note".to_owned()],
            fx.project.path(),
        )
        .unwrap();
        assert_eq!(by_ref.len(), 1);
        assert_eq!(by_ref[0].item_ref, "knowledge:acme/note");
        assert_eq!(by_ref[0].kind, "knowledge");
        assert_eq!(by_ref[0].digest.len(), 64);

        let by_path = describe_with(
            &fx.kinds,
            &fx.trust,
            &[path.to_string_lossy().into_owned()],
            fx.project.path(),
        )
        .unwrap();
        assert_eq!(by_path, by_ref);

        let by_glob = describe_with(
            &fx.kinds,
            &fx.trust,
            &["knowledge:acme/*".to_owned()],
            fx.project.path(),
        )
        .unwrap();
        assert_eq!(by_glob.len(), 2);
    }

    #[test]
    fn the_digest_is_the_one_the_engine_recomputes_and_moves_with_the_bytes() {
        let fx = fixture();
        let path = plant(&fx, "acme/note", "one", &fx.authoring);
        let refs = ["knowledge:acme/note".to_owned()];
        let first = describe_with(&fx.kinds, &fx.trust, &refs, fx.project.path()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let envelope = ryeos_engine::contracts::SignatureEnvelope {
            prefix: "<!--".to_owned(),
            suffix: Some("-->".to_owned()),
            after_shebang: false,
        };
        let header =
            ryeos_engine::item_resolution::parse_signature_header(&content, &envelope).unwrap();
        assert_eq!(first[0].digest, header.content_hash);
        plant(&fx, "acme/note", "changed", &fx.authoring);
        let second = describe_with(&fx.kinds, &fx.trust, &refs, fx.project.path()).unwrap();
        assert_ne!(first[0].digest, second[0].digest);
    }

    #[test]
    fn refuses_unsigned_changed_and_non_authoring_items() {
        let fx = fixture();
        let refs = ["knowledge:acme/note".to_owned()];

        let other = SigningKey::from_bytes(&[9; 32]);
        plant(&fx, "acme/note", "one", &other);
        let error = describe_with(&fx.kinds, &fx.trust, &refs, fx.project.path()).unwrap_err();
        assert!(
            error.to_string().contains("not an authoring key"),
            "{error:#}"
        );

        let path = plant(&fx, "acme/note", "one", &fx.authoring);
        let signed = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, signed.replace("\n\none\n", "\n\ntampered\n")).unwrap();
        let error = describe_with(&fx.kinds, &fx.trust, &refs, fx.project.path()).unwrap_err();
        assert!(
            error.to_string().contains("changed after it was signed"),
            "{error:#}"
        );

        std::fs::write(
            &path,
            "```yaml\nname: note\ncategory: \"acme\"\n```\n\nbare\n",
        )
        .unwrap();
        let error = describe_with(&fx.kinds, &fx.trust, &refs, fx.project.path()).unwrap_err();
        assert!(error.to_string().contains("unsigned"), "{error:#}");

        let error = describe_with(
            &fx.kinds,
            &fx.trust,
            &["knowledge:acme/missing".to_owned()],
            fx.project.path(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("not found"), "{error:#}");
    }
}
