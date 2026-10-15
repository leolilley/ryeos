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
}
