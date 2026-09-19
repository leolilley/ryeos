//! Concrete signed-remote composition for the stopped-node bundle-set update.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, bail};
use base64::Engine as _;
use ryeos_app::{
    bundle_publication::{PublicationObjectReader, consumer},
    bundle_set_transaction::{
        BundleSetActionKind, InstalledBundleIdentity, PreparedBundleSetAction,
        StoppedBundleSetApplyRequest,
    },
};
use ryeos_bundle_publication_contract::BundleTarget;
use ryeos_node::bundle_set_update::{
    BundleSetUpdateSelection, OfflineDeploymentSelectionInput, PreparedStoppedBundleSetUpdate,
    StoppedBundleSetUpdateAuthority, StoppedBundleSetUpdateReport, StoppedBundleSetUpdateRequest,
};

use super::{
    client::{
        NodeAdmittedObjectsClosureRequestOptions, ObjectsClosureRequestOptions, RemoteClient,
    },
    config,
};

fn admit_substrate_target(
    target: &BundleTarget,
    set_protocol: u32,
    substrate_protocol: u32,
    consumer_triple: &str,
) -> anyhow::Result<()> {
    if set_protocol == 0 || set_protocol != substrate_protocol {
        bail!("bundle set is incompatible with the pinned substrate protocol");
    }
    match target {
        BundleTarget::Portable => Ok(()),
        BundleTarget::Triple { triple } if triple == consumer_triple => Ok(()),
        BundleTarget::Triple { triple } => {
            bail!("bundle set target {triple} does not match consumer target {consumer_triple}")
        }
    }
}

pub async fn update_stopped_bundle_set(
    app_root: &Path,
    request: StoppedBundleSetUpdateRequest,
) -> anyhow::Result<StoppedBundleSetUpdateReport> {
    let authority = RemoteAuthority::load(app_root, &request)?;
    ryeos_node::bundle_set_update::update_stopped_bundle_set(app_root, request, &authority).await
}

struct RemoteAuthority {
    app_root: PathBuf,
    remote: config::RemoteConfig,
    identity: Arc<ryeos_app::identity::NodeIdentity>,
    policy: consumer::CurrentConsumerPublicationPolicy,
    closure_options: NodeAdmittedObjectsClosureRequestOptions,
    completion: ryeos_node::InitCompletionReport,
    substrate: ryeos_node::SubstrateIdentity,
}

impl RemoteAuthority {
    fn load(app_root: &Path, request: &StoppedBundleSetUpdateRequest) -> anyhow::Result<Self> {
        let completion = ryeos_node::verify_init_completion(app_root)?
            .context("node has no verified initialization completion")?;
        let substrate = ryeos_node::load_verified_substrate_identity(app_root)?;
        let remotes = config::load_remotes_layered(app_root, None)?;
        let remote = config::get_remote(&remotes, &request.catalog_remote)?;
        let identity = Arc::new(ryeos_app::identity::NodeIdentity::load(
            &request.operator_signing_key,
        )?);
        if identity.fingerprint() != completion.operator_fingerprint {
            bail!("explicit update signer is not the init-completion operator");
        }
        let trust = ryeos_engine::trust::TrustStore::load(
            None,
            &app_root.join(ryeos_engine::AI_DIR).join("config"),
        )?;
        let table = ryeos_app::node_policy::NodePolicyTable::new();
        let generation =
            ryeos_app::node_policy::generation::load_policy_generation(app_root, &trust, &table)?;
        let snapshot = ryeos_app::node_policy::compile_generation(
            app_root,
            &table,
            &generation,
            &completion.node_fingerprint,
        )?;
        let closure_policy = snapshot.require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?;
        let closure_options = NodeAdmittedObjectsClosureRequestOptions::for_policy(
            closure_policy,
            ObjectsClosureRequestOptions::default(),
        )?;
        let policy = ryeos_node::load_current_consumer_publication_policy(
            app_root,
            &request.catalog_namespace,
        )?;
        Ok(Self {
            app_root: app_root.to_owned(),
            remote,
            identity,
            policy,
            closure_options,
            completion,
            substrate,
        })
    }

    async fn fetch(
        &self,
        request: &StoppedBundleSetUpdateRequest,
        cas: &lillux::CasStore,
    ) -> anyhow::Result<consumer::ResolvedSetCoordinate> {
        let client = RemoteClient::new(
            &self.remote.url,
            &self.remote.principal_id,
            self.identity.clone(),
        );
        let root = match &request.selection {
            BundleSetUpdateSelection::Channel {
                set_name, channel, ..
            } => {
                let value = client.execute_service_result(
                    "service:bundle-catalog/resolve", &BTreeMap::new(), None,
                    &serde_json::json!({"catalog_namespace":request.catalog_namespace,"set_name":set_name,"channel":channel}),
                    &ryeos_app::execution_policy::ExecutionPolicy::projectless(ryeos_app::execution_policy::ExecutionResponse::Wait),
                ).await?;
                value
                    .get("catalog_publication_attestation_hash")
                    .and_then(|v| v.as_str())
                    .context("catalog resolve omitted exact publication head")?
                    .to_owned()
            }
            BundleSetUpdateSelection::Exact {
                catalog_publication_attestation_hash,
                ..
            } => catalog_publication_attestation_hash.clone(),
        };
        let response = client
            .objects_closure_get(&[root.clone()], self.closure_options.clone())
            .await?;
        for entry in response.entries {
            match entry.kind.as_str() {
                "object" => {
                    let value = entry.value.context("remote object entry omitted value")?;
                    let stored = cas.put_object(&value)?;
                    if stored.hash != entry.hash {
                        bail!("remote object content identity mismatch");
                    }
                }
                "blob" => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(entry.data.context("remote blob entry omitted data")?)?;
                    let stored = cas.put_blob(&bytes)?;
                    if stored.hash != entry.hash {
                        bail!("remote blob content identity mismatch");
                    }
                }
                _ => bail!("remote closure returned a non-materialized entry"),
            }
        }
        match &request.selection {
            BundleSetUpdateSelection::Channel {
                set_name, channel, ..
            } => consumer::resolve_set_channel(
                &root,
                &request.catalog_namespace,
                set_name,
                channel,
                cas,
                &self.policy,
            ),
            BundleSetUpdateSelection::Exact {
                catalog_publication_attestation_hash,
                catalog_publication_hash,
                catalog_snapshot_hash,
                set_attestation_hash,
                set_hash,
            } => {
                let coordinate = consumer::ResolvedSetCoordinate {
                    catalog_publication_attestation_hash: catalog_publication_attestation_hash
                        .clone(),
                    catalog_publication_hash: catalog_publication_hash.clone(),
                    catalog_snapshot_hash: catalog_snapshot_hash.clone(),
                    set_attestation_hash: set_attestation_hash.clone(),
                    set_hash: set_hash.clone(),
                };
                consumer::verify_exact_coordinate(
                    &coordinate,
                    &request.catalog_namespace,
                    cas,
                    &self.policy,
                )?;
                Ok(coordinate)
            }
        }
    }

    fn live_identities(&self) -> anyhow::Result<BTreeMap<String, InstalledBundleIdentity>> {
        let trees = self.app_root.join(ryeos_engine::AI_DIR).join("bundles");
        let registrations = self
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("node/bundles");
        let mut out = BTreeMap::new();
        for entry in std::fs::read_dir(&registrations)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|v| v.to_str()) != Some("yaml") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|v| v.to_str())
                .context("bundle registration name is not UTF-8")?
                .to_owned();
            let registration = lillux::read_regular_file_bounded_no_follow(&path, 256 * 1024)?;
            let tree = trees.join(&name);
            out.insert(
                name.clone(),
                InstalledBundleIdentity {
                    bundle_name: name,
                    tree_digest: ryeos_app::bundle_transaction::tree_digest(&tree)?,
                    registration_digest: lillux::sha256_hex(&registration),
                },
            );
        }
        if out.is_empty() {
            bail!("initialized node has no bundle registrations");
        }
        Ok(out)
    }
}

impl StoppedBundleSetUpdateAuthority for RemoteAuthority {
    async fn prepare(
        &self,
        app_root: &Path,
        request: &StoppedBundleSetUpdateRequest,
    ) -> anyhow::Result<PreparedStoppedBundleSetUpdate> {
        if app_root != self.app_root {
            bail!("update authority changed app root");
        }
        let cas = Arc::new(ryeos_node::bundle_set_update::open_stopped_bundle_cas(
            app_root,
        )?);
        let coordinate = self.fetch(request, &cas).await?;
        let binding = ryeos_app::bundle_publication::ReleasePolicyBinding {
            catalog_namespace: request.catalog_namespace.clone(),
            bundle_publication_policy_section_digest: self
                .policy
                .release_proof_policy()
                .bundle_publication_policy_section_digest
                .clone(),
            trust_epoch: self.policy.release_proof_policy().trust_epoch,
        };
        let proof = self.policy.fetched_cas_release_proof(cas.clone())?;
        let verified = consumer::verify_curated_set(
            &coordinate.set_attestation_hash,
            cas.as_ref(),
            &self.policy,
            &proof,
            &proof,
            &binding,
        )?;
        admit_substrate_target(
            &verified.set().target,
            verified.set().substrate_protocol,
            self.substrate.protocol,
            env!("RYEOS_CONSUMER_TARGET"),
        )?;
        let active =
            ryeos_node::bundle_set_update::load_active_published_set(app_root, cas.as_ref())?;
        let live = self.live_identities()?;
        let installed = match active.as_ref() {
            Some((_, _, set)) => ryeos_node::bundle_set_update::installed_generation_map(Some(set)),
            None => live
                .keys()
                .map(|name| {
                    let selected = verified
                        .set()
                        .entries
                        .iter()
                        .find(|entry| entry.bundle_name == *name);
                    let generation = if name == "core" {
                        selected
                            .map(|v| v.generation_hash.clone())
                            .unwrap_or_else(|| "0".repeat(64))
                    } else {
                        "0".repeat(64)
                    };
                    (name.clone(), generation)
                })
                .collect(),
        };
        let plan = consumer::plan_exact_set(&verified, &installed)?;
        let expected_active = active
            .as_ref()
            .map(|(value, _, _)| value.selection_hash.clone());
        let offline = ryeos_node::bundle_set_update::prepare_offline_deployment_selection(
            &request.operator_signing_key,
            &coordinate,
            OfflineDeploymentSelectionInput {
                target_identity: self.completion.node_fingerprint.clone(),
                substrate_image_digest: self.substrate.image_digest.clone(),
                substrate_protocol: self.substrate.protocol,
                bundle_publication_policy_section_digest: binding
                    .bundle_publication_policy_section_digest
                    .clone(),
                node_policy_generation_digest: self.completion.policy_generation_digest.clone(),
                expected_active_selection: expected_active.clone(),
                issued_at: lillux::time::iso8601_now(),
            },
        )?;
        let selection_stored = cas.put_object(&offline.selection.to_value()?)?;
        let authorization_stored =
            cas.put_object(&serde_json::to_value(&offline.authorization)?)?;
        if selection_stored.hash != offline.selection_hash
            || authorization_stored.hash != offline.authorization_hash
        {
            bail!("offline deployment authority CAS identity mismatch");
        }
        consumer::admit_node_bundle_selection(
            &offline.authorization_hash,
            &verified,
            consumer::SelectionAdmissionContext {
                target_identity: &self.completion.node_fingerprint,
                substrate_image_digest: &self.substrate.image_digest,
                substrate_protocol: self.substrate.protocol,
                bundle_target: &verified.set().target,
                publication_policy_section_digest: &binding
                    .bundle_publication_policy_section_digest,
                node_policy_generation_digest: &self.completion.policy_generation_digest,
                active_selection: expected_active.as_deref(),
            },
            cas.as_ref(),
            &self.policy,
        )?;

        let transaction_id = lillux::sha256_hex(
            format!("{}:{}", offline.selection_hash, lillux::time::iso8601_now()).as_bytes(),
        );
        let prep_root = app_root
            .join(ryeos_engine::AI_DIR)
            .join("transactions/bundle-set-prepared")
            .join(&transaction_id);
        std::fs::create_dir_all(&prep_root)?;
        let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
            app_root: Some(app_root.to_owned()),
            ..Default::default()
        })?;
        let mut actions = Vec::new();
        let mut future = live.clone();
        for entry in &plan.entries {
            let old = live.get(&entry.bundle_name);
            let kind = match entry.action {
                consumer::ExactSetAction::Add => BundleSetActionKind::Add,
                consumer::ExactSetAction::Replace => BundleSetActionKind::Replace,
                consumer::ExactSetAction::Remove => BundleSetActionKind::Remove,
                consumer::ExactSetAction::Keep => BundleSetActionKind::Keep,
            };
            if entry.bundle_name == "core" && kind != BundleSetActionKind::Keep {
                bail!("core is substrate-owned and must remain an exact keep");
            }
            if entry.bundle_name == "core" {
                let generation = verified
                    .generations()
                    .get("core")
                    .context("verified set omitted core generation")?
                    .generation();
                let expected: ryeos_state::objects::ExternalContentManifestObject =
                    serde_json::from_value(
                        cas.get_object(&generation.content_manifest_hash)?
                            .context("core content manifest is absent")?,
                    )?;
                let live_core = app_root.join(ryeos_engine::AI_DIR).join("bundles/core");
                let actual = ryeos_app::bundle_publication::tree::inspect_bundle_tree(&live_core)?;
                if actual != expected {
                    bail!("selected core generation differs from the pinned substrate tree");
                }
            }
            let mut new_tree_source = None;
            let mut new_registration_source = None;
            let mut new_tree_digest = old.map(|v| v.tree_digest.clone());
            let mut new_registration_digest = old.map(|v| v.registration_digest.clone());
            if matches!(
                kind,
                BundleSetActionKind::Add | BundleSetActionKind::Replace
            ) {
                let generation = verified
                    .generations()
                    .get(&entry.bundle_name)
                    .context("verified set omitted selected generation")?
                    .generation();
                let value = cas
                    .get_object(&generation.content_manifest_hash)?
                    .context("verified bundle manifest is absent")?;
                let manifest: ryeos_state::objects::ExternalContentManifestObject =
                    serde_json::from_value(value)?;
                let tree = prep_root.join(format!("{}.tree", entry.bundle_name));
                ryeos_app::bundle_publication::tree::materialize_bundle_tree(
                    &manifest, &cas, &tree,
                )?;
                let registration_path = prep_root.join(format!("{}.yaml", entry.bundle_name));
                let live_target = app_root
                    .join(ryeos_engine::AI_DIR)
                    .join("bundles")
                    .join(&entry.bundle_name);
                let bytes = ryeos_app::bundle_transaction::prepare_signed_bundle_registration(
                    &live_target,
                    &config.node_signing_key_path,
                )?;
                lillux::atomic_write_private(&registration_path, &bytes)?;
                let identity = InstalledBundleIdentity {
                    bundle_name: entry.bundle_name.clone(),
                    tree_digest: ryeos_app::bundle_transaction::tree_digest(&tree)?,
                    registration_digest: lillux::sha256_hex(&bytes),
                };
                new_tree_digest = Some(identity.tree_digest.clone());
                new_registration_digest = Some(identity.registration_digest.clone());
                future.insert(entry.bundle_name.clone(), identity);
                new_tree_source = Some(tree);
                new_registration_source = Some(registration_path);
            } else if kind == BundleSetActionKind::Remove {
                future.remove(&entry.bundle_name);
                new_tree_digest = None;
                new_registration_digest = None;
            }
            actions.push(PreparedBundleSetAction {
                bundle_name: entry.bundle_name.clone(),
                kind,
                old_tree_digest: old.map(|v| v.tree_digest.clone()),
                new_tree_digest,
                old_registration_digest: old.map(|v| v.registration_digest.clone()),
                new_registration_digest,
                new_tree_source,
                new_registration_source,
            });
        }
        let old_installed_set_digest =
            ryeos_app::bundle_set_transaction::installed_set_digest(live.values().cloned())?;
        if active
            .as_ref()
            .is_some_and(|(a, _, _)| a.installed_set_digest != old_installed_set_digest)
        {
            bail!("live installed set differs from active selection identity");
        }
        let new_installed_set_digest =
            ryeos_app::bundle_set_transaction::installed_set_digest(future.values().cloned())?;
        let operator_pem =
            lillux::read_regular_file_bounded_no_follow(&request.operator_signing_key, 64 * 1024)?;
        let signer = ryeos_node::OfflineInitCompletionSigner::from_pkcs8_pem(std::str::from_utf8(
            &operator_pem,
        )?)?;
        let next_completion = ryeos_node::prepare_bundle_set_init_completion(
            app_root,
            &signer,
            self.completion.node_fingerprint.clone(),
            self.completion.vault_fingerprint.clone(),
            self.completion.policy_generation_digest.clone(),
            &actions,
        )?;
        let old_completion_bytes = lillux::read_regular_file_bounded_no_follow(
            &ryeos_app::bundle_set_transaction::init_completion_path(app_root),
            256 * 1024,
        )?;
        let apply = StoppedBundleSetApplyRequest {
            transaction_id,
            selection_hash: offline.selection_hash,
            expected_active_selection: expected_active,
            old_completion_hash: lillux::sha256_hex(&old_completion_bytes),
            old_completion_bytes,
            new_completion_hash: next_completion.document_hash,
            new_completion_bytes: next_completion.bytes,
            old_installed_set_digest,
            new_installed_set_digest: new_installed_set_digest.clone(),
            actions,
        };
        Ok(PreparedStoppedBundleSetUpdate {
            coordinate,
            plan,
            apply,
            installed_set_digest: new_installed_set_digest,
        })
    }

    fn admit_journal(
        &self,
        prepared: &PreparedStoppedBundleSetUpdate,
        journal: &ryeos_app::bundle_set_transaction::BundleSetJournal,
    ) -> anyhow::Result<()> {
        if journal.selection_hash != prepared.apply.selection_hash
            || journal.new_installed_set_digest != prepared.installed_set_digest
            || journal.actions.len() != prepared.plan.entries.len()
        {
            bail!("transaction journal differs from admitted exact-set plan");
        }
        for (journal_action, admitted) in journal.actions.iter().zip(&prepared.apply.actions) {
            if journal_action.bundle_name != admitted.bundle_name
                || journal_action.kind != admitted.kind
                || journal_action.old_tree_digest != admitted.old_tree_digest
                || journal_action.new_tree_digest != admitted.new_tree_digest
                || journal_action.old_registration_digest != admitted.old_registration_digest
                || journal_action.new_registration_digest != admitted.new_registration_digest
            {
                bail!("transaction journal action differs from admitted prospective identity");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod target_tests {
    use super::*;

    #[test]
    fn native_admission_requires_exact_arch_os_and_abi() {
        let host = "x86_64-unknown-linux-gnu";
        for triple in [
            host,
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "x86_64-apple-darwin",
        ] {
            let result = admit_substrate_target(
                &BundleTarget::Triple {
                    triple: triple.into(),
                },
                1,
                1,
                host,
            );
            assert_eq!(result.is_ok(), triple == host);
        }
    }

    #[test]
    fn portable_and_native_sets_still_require_the_pinned_nonzero_protocol() {
        let host = "x86_64-unknown-linux-gnu";
        for target in [
            BundleTarget::Portable,
            BundleTarget::Triple {
                triple: host.into(),
            },
        ] {
            assert!(admit_substrate_target(&target, 1, 1, host).is_ok());
            assert!(admit_substrate_target(&target, 2, 1, host).is_err());
            assert!(admit_substrate_target(&target, 0, 0, host).is_err());
        }
    }
}
