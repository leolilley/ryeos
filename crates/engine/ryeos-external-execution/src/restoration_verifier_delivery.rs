//! Exact, credential-free delivery body for the independently admitted guest
//! restoration verifier. Lillux retains and reads the executable descriptor;
//! this module owns only bounded directory-upload tar packaging. Sealing proves
//! byte identity, not signed admission or permission to contact a provider.

use anyhow::{Result, ensure};

pub use ryeos_external_execution_contract::restored_runtime_measurement::{
    CONSUMER_VERIFIER_REMOTE_NAME, MAX_RESTORATION_VERIFIER_BYTES,
    RESTORATION_VERIFIER_REMOTE_DIRECTORY, RESTORATION_VERIFIER_REMOTE_NAME,
};

pub struct SealedQualificationVerifierUpload {
    descriptor: lillux::InheritedDescriptorAuthority,
    bytes: u64,
    sha256: String,
}

impl SealedQualificationVerifierUpload {
    pub fn descriptor(&self) -> &lillux::InheritedDescriptorAuthority {
        &self.descriptor
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

pub fn seal_restoration_verifier_upload(
    executable: &lillux::InheritedDescriptorAuthority,
    expected_executable_sha256: &str,
) -> Result<SealedQualificationVerifierUpload> {
    executable.require_owned_executable()?;
    seal_verifier_upload(
        executable,
        expected_executable_sha256,
        RESTORATION_VERIFIER_REMOTE_NAME,
    )
}

/// The caller owns signed consumer-artifact admission and retained source
/// proof. A foreign-target payload remains sealed data on the controller;
/// it is not promoted to a controller executable. The closed entrypoint cannot
/// overwrite the prerequisite verifier.
pub fn seal_consumer_verifier_upload(
    executable: &lillux::InheritedDescriptorAuthority,
    expected_executable_sha256: &str,
) -> Result<SealedQualificationVerifierUpload> {
    seal_verifier_upload(
        executable,
        expected_executable_sha256,
        CONSUMER_VERIFIER_REMOTE_NAME,
    )
}

fn seal_verifier_upload(
    executable: &lillux::InheritedDescriptorAuthority,
    expected_executable_sha256: &str,
    remote_name: &'static str,
) -> Result<SealedQualificationVerifierUpload> {
    ensure!(
        lillux::valid_hash(expected_executable_sha256),
        "restoration verifier executable digest is invalid"
    );
    let (binary, observation) =
        executable.read_regular_file_stable_bounded(MAX_RESTORATION_VERIFIER_BYTES)?;
    ensure!(
        !binary.is_empty()
            && binary.len() as u64 <= MAX_RESTORATION_VERIFIER_BYTES
            && lillux::sha256_hex(&binary) == expected_executable_sha256
            && executable.digest_regular_file_stable_exact(&observation)?
                == expected_executable_sha256,
        "restoration verifier differs from its admitted executable"
    );
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(binary.len() as u64);
    header.set_mode(0o500);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, remote_name, binary.as_slice())?;
    let bytes = archive.into_inner()?;
    ensure!(
        bytes.len() as u64 <= MAX_RESTORATION_VERIFIER_BYTES + 16 * 1024,
        "restoration verifier upload exceeds its bound"
    );
    let descriptor = lillux::sealed_memfd(c"ryeos-restoration-verifier-upload", &bytes)
        .map_err(anyhow::Error::msg)?;
    Ok(SealedQualificationVerifierUpload {
        descriptor,
        bytes: bytes.len() as u64,
        sha256: lillux::sha256_hex(&bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn consumer_delivery_cannot_overwrite_owner_measurement_entrypoint() {
        let executable =
            lillux::sealed_memfd(c"consumer-verifier-fixture", b"consumer-fixture").unwrap();
        let digest = lillux::sha256_hex(b"consumer-fixture");
        let upload = seal_consumer_verifier_upload(&executable, &digest).unwrap();
        let (body, _) = upload
            .descriptor()
            .read_regular_file_stable_bounded(upload.bytes())
            .unwrap();
        assert_eq!(lillux::sha256_hex(&body), upload.sha256());
        let mut archive = tar::Archive::new(body.as_slice());
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(
            entry.path().unwrap().as_ref(),
            std::path::Path::new(CONSUMER_VERIFIER_REMOTE_NAME)
        );
        assert_ne!(
            CONSUMER_VERIFIER_REMOTE_NAME,
            RESTORATION_VERIFIER_REMOTE_NAME
        );
        assert_eq!(entry.header().mode().unwrap(), 0o500);
        let mut restored = Vec::new();
        entry.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, b"consumer-fixture");
        assert!(entries.next().is_none());
        assert!(seal_consumer_verifier_upload(&executable, &"0".repeat(64)).is_err());
    }

    #[test]
    fn exact_verifier_is_sealed_as_one_executable_tar_member() {
        let executable =
            lillux::sealed_executable_memfd(c"verifier-fixture", b"fixture-executable").unwrap();
        let digest = lillux::sha256_hex(b"fixture-executable");
        let upload = seal_restoration_verifier_upload(&executable, &digest).unwrap();
        let (body, _) = upload
            .descriptor()
            .read_regular_file_stable_bounded(upload.bytes())
            .unwrap();
        assert_eq!(lillux::sha256_hex(&body), upload.sha256());
        let mut archive = tar::Archive::new(body.as_slice());
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(
            entry.path().unwrap().as_ref(),
            std::path::Path::new(RESTORATION_VERIFIER_REMOTE_NAME)
        );
        assert_eq!(entry.header().mode().unwrap(), 0o500);
        let mut restored = Vec::new();
        entry.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, b"fixture-executable");
        assert!(entries.next().is_none());
        assert!(seal_restoration_verifier_upload(&executable, &"0".repeat(64)).is_err());
    }
}
