//! Guest-runtime and placement flows only the protocol tests drive: an
//! ordinary-content guest owner runtime product and the signed placement
//! selection built from both proofs.

use std::path::Path;
use std::sync::Arc;

use super::runtime_qualification::{
    ActivatedRuntimeQualification, GuestProductStorage, GuestProductSubject,
    GuestRuntimeQualification, activate_runtime, publish_guest_runtime_witness,
    qualify_activated_runtime, qualify_published_guest_runtime,
};

/// The ordinary-content form a guest owner runtime product takes: the four
/// owner members, observed as one ordinary tree manifest.
pub const OWNER_GUEST_PRODUCT: GuestProductStorage = GuestProductStorage {
    storage: "content",
    maximum_file_bytes: 1_048_576,
    maximum_total_bytes: 2_097_152,
};

/// Lay out the four-member guest owner runtime a placement binding names as its
/// `guest_runtime_manifest_hash`: the owner executable, the controller's public
/// root and the owner profile. `owner_bytes` stands in for the executable; the
/// synthetic adapter never runs it.
pub fn write_owner_runtime_tree(
    root: &Path,
    controller_root: &lillux::crypto::VerifyingKey,
    owner_bytes: &[u8],
) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(root.join("bin")).unwrap();
    let write = |relative: &str, bytes: &[u8], mode: u32| {
        let path = root.join(relative);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    write(
        "bin/ryeos-external-guest-occurrence-owner",
        owner_bytes,
        0o755,
    );
    write(
        "controller-root.hex",
        controller_root
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            .as_bytes(),
        0o644,
    );
    let profile = ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile {
        schema: 2,
        account: lillux::GuestRuntimeAccount::Unix {
            uid: 65534,
            gid: 65534,
        },
        private_source_max_bytes: 32 * 1024 * 1024,
        private_source_max_inodes: 1024,
        owner_timeout_seconds: 600,
    };
    write(
        "guest-owner-profile.json",
        &ryeos_external_execution_contract::canonical_json(&profile).unwrap(),
        0o644,
    );
}

/// Observe one runtime tree as an ordinary content manifest and leave its
/// bytes resident in the node's object store.
pub fn install_ordinary_runtime_content(
    state: &ryeos_app::state::AppState,
    runtime_root: &Path,
) -> ryeos_state::objects::ExternalContentManifestObject {
    use ryeos_state::objects::ExternalContentManifestEntryKind;

    let directory = lillux::PinnedDirectory::open(runtime_root)
        .unwrap()
        .unwrap();
    let manifest = ryeos_state::observe_external_content_tree_exact(&directory).unwrap();
    let authority = state.state_store.pinned_state_authority().unwrap();
    let _guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    for entry in &manifest.entries {
        if entry.kind != ExternalContentManifestEntryKind::File {
            continue;
        }
        let bytes = std::fs::read(runtime_root.join(&entry.path)).unwrap();
        assert_eq!(entry.size, Some(bytes.len() as u64));
        let blob_hash = cas.store_blob(&bytes).unwrap();
        assert_eq!(entry.blob_hash.as_deref(), Some(blob_hash.as_str()));
    }
    let stored = cas
        .store_object(&serde_json::to_value(&manifest).unwrap())
        .unwrap();
    assert_eq!(
        stored,
        ryeos_state::external_content_manifest_digest(&manifest).unwrap()
    );
    manifest
}

/// The signed placement selection a binding carries. A product-sourced
/// session passes no content proof: its runtime authority is its admitted
/// product selection. Only an activation-sourced session passes one.
pub fn placement_selection(
    guest: &GuestRuntimeQualification,
    content: Option<&ActivatedRuntimeQualification>,
) -> ryeos_app::external_placement::test_support::PlacementQualificationSelection {
    use ryeos_app::external_placement::test_support::{
        ContentRuntimeQualificationSelection, GuestRuntimeQualificationSelection,
        PlacementQualificationSelection,
    };
    PlacementQualificationSelection {
        guest_runtime_manifest_hash: guest.guest_runtime_manifest_hash.clone(),
        guest: Some(GuestRuntimeQualificationSelection {
            attestation_hash: guest.attestation_hash.clone(),
            owner_principal: guest.owner_principal.clone(),
            policy_ref: guest.policy_ref.clone(),
            required_claims: guest.required_claims.clone(),
        }),
        content: content.map(|content| ContentRuntimeQualificationSelection {
            activation_ref: content.activation_ref.clone(),
            coordinate_id: content.coordinate_id.clone(),
            attestation_hash: content.attestation_hash.clone(),
            owner_principal: content.owner_principal.clone(),
            required_claims: content.required_claims.clone(),
        }),
    }
}

impl GuestProductSubject {
    pub fn ordinary(manifest: &ryeos_state::objects::ExternalContentManifestObject) -> Self {
        Self {
            manifest_hash: ryeos_state::external_content_manifest_digest(manifest).unwrap(),
            manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
            entry_count: manifest.entry_count,
            total_bytes: manifest.total_bytes,
        }
    }
}

/// Qualify one guest runtime product resident in the node, of either manifest
/// kind. On return the caller is the sole owner of `state` again.
pub async fn qualify_guest_runtime_subject(
    state: &Arc<ryeos_app::state::AppState>,
    subject: &GuestProductSubject,
    verifier_runtime_manifest_hash: &str,
    required_claims: &[&str],
) -> GuestRuntimeQualification {
    let witness_hash = publish_guest_runtime_witness(state, subject);
    qualify_published_guest_runtime(
        state,
        subject,
        witness_hash,
        verifier_runtime_manifest_hash,
        required_claims,
    )
    .await
}

/// Activate the archived runtime and qualify it; see
/// [`activate_runtime`] and [`qualify_activated_runtime`].
pub async fn activate_and_qualify_runtime(
    state: &Arc<ryeos_app::state::AppState>,
    runtime_manifest_hash: &str,
    verifier_runtime_manifest_hash: &str,
    required_claims: &[&str],
) -> ActivatedRuntimeQualification {
    activate_runtime(state).await;
    qualify_activated_runtime(
        state,
        runtime_manifest_hash,
        verifier_runtime_manifest_hash,
        required_claims,
    )
    .await
}
