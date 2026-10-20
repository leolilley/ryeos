//! Guest-runtime and activated-runtime qualification flows only the
//! qualification fixture drives: large-content guest products and the extra
//! evidence (product witness, activation receipt) its assertions read.

use std::path::Path;
use std::sync::Arc;

use super::qualification_node::manifest_hash;
use super::runtime_qualification::{
    ActivatedRuntimeQualification, GuestProductStorage, GuestProductSubject,
    GuestRuntimeQualification, activate_runtime, publish_guest_runtime_witness,
    qualify_activated_runtime, qualify_published_guest_runtime, walk_tree,
    write_guest_runtime_sources_for,
};

/// The large-content form: a joined runtime tree captured by large-object
/// storage.
pub const LARGE_GUEST_PRODUCT: GuestProductStorage = GuestProductStorage {
    storage: "large_content",
    maximum_file_bytes: 536_870_912,
    maximum_total_bytes: 1_073_741_824,
};

/// Write the guest-runtime recipe, policy and verifier into a signed authoring
/// bundle, before its node starts, for a large-content guest runtime; see
/// [`write_guest_runtime_sources_for`].
pub fn write_guest_runtime_sources(
    bundle_root: &Path,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    guest_runtime_manifest_hash: &str,
    verifier_runtime_manifest_hash: &str,
    required_claims: &[&str],
) {
    write_guest_runtime_sources_for(
        bundle_root,
        identity,
        guest_runtime_manifest_hash,
        verifier_runtime_manifest_hash,
        required_claims,
        &LARGE_GUEST_PRODUCT,
    );
}

impl GuestProductSubject {
    pub fn large(manifest: &ryeos_state::objects::ExternalLargeContentManifestObject) -> Self {
        Self {
            manifest_hash: manifest_hash(manifest),
            manifest_kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
            entry_count: manifest.entry_count,
            total_bytes: manifest.total_bytes,
        }
    }
}

/// A guest-runtime qualification with the product witness it was proved from.
pub struct QualifiedGuestRuntime {
    pub qualification: GuestRuntimeQualification,
    pub product_witness_hash: String,
}

/// Qualify one guest runtime tree that is already resident in the node (see
/// `install_large_runtime_content`) and was named in
/// [`write_guest_runtime_sources`]. Requires the node to be started with that
/// bundle. On return the caller is the sole owner of `state` again.
pub async fn qualify_guest_runtime(
    state: &Arc<ryeos_app::state::AppState>,
    guest_runtime: &ryeos_state::objects::ExternalLargeContentManifestObject,
    verifier_runtime_manifest_hash: &str,
    required_claims: &[&str],
) -> QualifiedGuestRuntime {
    let subject = GuestProductSubject::large(guest_runtime);
    let product_witness_hash = publish_guest_runtime_witness(state, &subject);
    let qualification = qualify_published_guest_runtime(
        state,
        &subject,
        product_witness_hash.clone(),
        verifier_runtime_manifest_hash,
        required_claims,
    )
    .await;
    QualifiedGuestRuntime {
        qualification,
        product_witness_hash,
    }
}

/// An activated-content qualification with the receipt of the managed
/// activation that acquired the runtime it was proved against.
pub struct QualifiedActivatedRuntime {
    pub qualification: ActivatedRuntimeQualification,
    pub activation_receipt_hash: String,
}

/// Activate the archived runtime and qualify it; see
/// [`activate_runtime`] and [`qualify_activated_runtime`].
pub async fn activate_and_qualify_runtime(
    state: &Arc<ryeos_app::state::AppState>,
    runtime_manifest_hash: &str,
    verifier_runtime_manifest_hash: &str,
    required_claims: &[&str],
) -> QualifiedActivatedRuntime {
    let activation_receipt_hash = activate_runtime(state).await;
    let qualification = qualify_activated_runtime(
        state,
        runtime_manifest_hash,
        verifier_runtime_manifest_hash,
        required_claims,
    )
    .await;
    QualifiedActivatedRuntime {
        qualification,
        activation_receipt_hash,
    }
}

/// Count the logical entries of a runtime tree as the archive packs them: the
/// tree's own entries plus the root directory.
pub fn logical_entry_count(runtime: &Path) -> usize {
    let mut entries = 1usize;
    walk_tree(runtime, "runtime", &mut |_, _, _| entries += 1);
    entries
}
