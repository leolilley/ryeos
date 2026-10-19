//! The activation inputs a leased guest reports verified in its Ready frame.
//!
//! The controller records three digests when it activates a lease; the guest
//! names the same three, in the same order, in the boot it reports; the
//! adapter compares them position by position. Every party derives the order
//! and the signed-import digest through this module so they cannot drift.

use sha2::{Digest, Sha256};

/// How many activation inputs a leased guest's Ready names.
pub const LEASE_READY_INPUT_COUNT: usize = 3;

/// Digest of the exact canonical bytes of the signed guest import
/// authorization as delivered to the guest.
pub fn signed_import_sha256(signed_import_bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(signed_import_bytes))
}

/// The activation inputs in the order a leased guest's Ready names them: the
/// activation request digest, the signed import digest, then the package
/// payload digest.
pub fn ordered_lease_ready_inputs<'a>(
    activation_request_digest: &'a str,
    signed_import_sha256: &'a str,
    guest_package_payload_sha256: &'a str,
) -> [&'a str; LEASE_READY_INPUT_COUNT] {
    [
        activation_request_digest,
        signed_import_sha256,
        guest_package_payload_sha256,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_FIELD_ORDER_SHA256: &str =
        "6be0689c0b3c74dfa90e1ac9b4e4e7e2adcbdee53f006148767d2b734c09cf0c";

    #[test]
    fn golden_vector_pins_digest_and_order() {
        // sha256("abc"), a published test vector.
        assert_eq!(
            signed_import_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let activation = "1".repeat(64);
        let payload = "3".repeat(64);
        let import = signed_import_sha256(b"abc");
        assert_eq!(
            ordered_lease_ready_inputs(&activation, &import, &payload),
            [
                activation.as_str(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                payload.as_str(),
            ]
        );
    }

    /// A fixed signed import: the digest the controller records and the guest
    /// names is over this exact struct-field-order serialization, which is
    /// not the key-sorted encoding.
    #[test]
    fn golden_signed_import_pins_the_field_order_derivation() {
        use crate::guest_import_authorization::{
            GUEST_IMPORT_AUTHORIZATION_SCHEMA, GuestImportAuthorization,
            SignedGuestImportAuthorization,
        };
        use crate::staging_package::{GUEST_IMPORT_TICKET_SCHEMA, GuestImportTicket};
        use crate::{
            EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, ExternalGuestInputProjection,
            GuestBaseSnapshotInput, GuestWorkspaceView,
        };

        let digest = |byte: char| byte.to_string().repeat(64);
        let signed = SignedGuestImportAuthorization {
            authorization: GuestImportAuthorization {
                schema: GUEST_IMPORT_AUTHORIZATION_SCHEMA,
                placement_thread_id: "T-golden".into(),
                admitted_capsule_hash: digest('5'),
                base_snapshot_hash: digest('a'),
                execution_binding_hash: digest('d'),
                allocation_request_digest: digest('e'),
                occurrence_id: "occ-golden".into(),
                activation_request_digest: digest('f'),
                supervisor_runtime_hash: digest('6'),
                guest_runtime_manifest_hash: digest('7'),
                attachment_deadline_ms: 20_000,
                admission_deadline_ms: 10_000,
                nonce_sha256: digest('8'),
                ticket: GuestImportTicket {
                    schema: GUEST_IMPORT_TICKET_SCHEMA,
                    binding_hash: digest('d'),
                    allocation_request_digest: digest('e'),
                    occurrence_id: "occ-golden".into(),
                    activation_request_digest: digest('f'),
                    guest_input_identity: digest('9'),
                    payload_sha256: digest('0'),
                    manifest_sha256: digest('1'),
                    framed_bytes: 200,
                    regular_bytes: 100,
                    bootstrap_sha256: digest('2'),
                    supervisor_sha256: digest('3'),
                    launcher_sha256: digest('4'),
                    maximum_regular_bytes: 1000,
                    maximum_framed_bytes: 2000,
                },
                guest_inputs: ExternalGuestInputProjection {
                    workspace_view: GuestWorkspaceView::FullSnapshot,
                    schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
                    base_snapshot: GuestBaseSnapshotInput {
                        descriptor: 56,
                        snapshot_hash: digest('a'),
                        closure_digest: digest('b'),
                        object_count: 3,
                        blob_count: 1,
                        total_bytes: 1,
                    },
                    workspace_outputs: None,
                    inputs: Vec::new(),
                    executable_search: Vec::new(),
                    environment: Default::default(),
                },
            },
            signature_hex: "ab".repeat(64),
        };
        let field_order = crate::canonical_json(&signed).unwrap();
        let key_sorted = serde_json::to_vec(&serde_json::to_value(&signed).unwrap()).unwrap();
        assert!(
            field_order.starts_with(b"{\"authorization\":{\"schema\":"),
            "the delivered form leads with struct field order"
        );
        assert_ne!(field_order, key_sorted);
        assert_ne!(
            signed_import_sha256(&field_order),
            signed_import_sha256(&key_sorted)
        );
        assert_eq!(
            signed_import_sha256(&field_order),
            GOLDEN_FIELD_ORDER_SHA256
        );
    }
}
