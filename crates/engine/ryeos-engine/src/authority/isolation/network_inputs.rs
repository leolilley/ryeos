//! Target-local network configuration is not portable executable content.
//! Only the signed node policy selects these finite regular files. Capture
//! them once per resolved policy generation; never reopen them at child exec,
//! inherit an ambient directory, or add provider-specific resolver exceptions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{
    EngineError, IsolationNetworkMode, IsolationNetworkPolicy, MAX_AUTHORITIES,
    canonicalize_launch_path, refused, validate_namespace_destination,
};

#[derive(Clone)]
pub(super) struct CapturedNetworkFile {
    pub destination: PathBuf,
    pub digest: String,
    pub authority: lillux::InheritedDescriptorAuthority,
}

pub(super) fn validate(policy: &IsolationNetworkPolicy) -> Result<(), EngineError> {
    if policy.runtime_files.len() > MAX_AUTHORITIES {
        return Err(refused(
            "network runtime files exceed authority ceiling".into(),
        ));
    }
    if policy.mode == IsolationNetworkMode::Isolated && !policy.runtime_files.is_empty() {
        return Err(refused(
            "isolated network policy cannot supply host network runtime files".into(),
        ));
    }
    let mut destinations = Vec::<&Path>::new();
    let mut total = 0_u64;
    for file in &policy.runtime_files {
        validate_namespace_destination("network input source", &file.source)?;
        validate_namespace_destination("network input destination", &file.destination)?;
        if file.source.file_name().is_none()
            || file.destination.file_name().is_none()
            || file.max_bytes == 0
        {
            return Err(refused(
                "network input requires regular-file coordinates and a positive byte bound".into(),
            ));
        }
        total = total
            .checked_add(file.max_bytes)
            .ok_or_else(|| refused("network input byte bounds overflow".into()))?;
        if destinations.iter().any(|prior| {
            prior.starts_with(&file.destination) || file.destination.starts_with(prior)
        }) {
            return Err(refused("network runtime file destinations overlap".into()));
        }
        destinations.push(&file.destination);
    }
    Ok(())
}

pub(super) fn capture(
    policy: &IsolationNetworkPolicy,
    app_root: Option<&Path>,
) -> Result<Vec<CapturedNetworkFile>, EngineError> {
    validate(policy)?;
    policy
        .runtime_files
        .iter()
        .map(|file| {
            // System resolver/certificate names may themselves be symlinks. Resolve
            // that operator-selected name once, then retain and read the exact
            // regular descriptor through Lillux. A later path or byte replacement
            // cannot change the sealed generation delivered to workers.
            let source = canonicalize_launch_path("network runtime input", &file.source)?;
            if app_root
                .is_some_and(|root| source.starts_with(root) || file.destination.starts_with(root))
            {
                return Err(refused(
                    "network runtime input cannot expose the node app root".into(),
                ));
            }
            let pinned = lillux::open_pinned_regular_file_no_follow(&source)
                .map_err(|error| refused(format!("pin network runtime input: {error}")))?;
            let observation = pinned
                .observation()
                .map_err(|error| refused(format!("observe network runtime input: {error}")))?;
            let bytes = pinned
                .read_stable_bounded(&observation, file.max_bytes)
                .map_err(|error| refused(format!("read network runtime input: {error}")))?;
            let digest = lillux::sha256_hex(&bytes);
            let authority = lillux::sealed_memfd(c"ryeos-network-input", &bytes)
                .map_err(|error| refused(format!("seal network runtime input: {error}")))?;
            Ok(CapturedNetworkFile {
                destination: file.destination.clone(),
                digest,
                authority,
            })
        })
        .collect()
}

pub(super) fn digests(files: &[CapturedNetworkFile]) -> BTreeMap<PathBuf, String> {
    files
        .iter()
        .map(|file| (file.destination.clone(), file.digest.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::IsolationNetworkRuntimeFile;
    use super::*;

    fn policy(source: &Path) -> IsolationNetworkPolicy {
        IsolationNetworkPolicy {
            mode: IsolationNetworkMode::Host,
            runtime_files: vec![IsolationNetworkRuntimeFile {
                source: source.to_owned(),
                destination: PathBuf::from("/etc/test-network-input"),
                max_bytes: 32,
            }],
        }
    }

    #[test]
    fn captured_input_survives_source_replacement_and_has_exact_digest() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("input");
        std::fs::write(&source, b"original").unwrap();
        let captured = capture(&policy(&source), None).unwrap();
        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, b"replacement").unwrap();
        assert_eq!(captured[0].digest, lillux::sha256_hex(b"original"));
        let (bytes, _) = captured[0]
            .authority
            .read_regular_file_stable_bounded(32)
            .unwrap();
        assert_eq!(bytes, b"original");
    }

    #[test]
    fn capture_refuses_missing_oversized_directory_and_node_private_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("input");
        assert!(capture(&policy(&source), None).is_err());
        assert!(capture(&policy(directory.path()), None).is_err());
        std::fs::write(&source, [b'x'; 33]).unwrap();
        assert!(capture(&policy(&source), None).is_err());
        std::fs::write(&source, b"small").unwrap();
        assert!(capture(&policy(&source), Some(directory.path())).is_err());
    }

    #[test]
    fn policy_refuses_isolated_inputs_overlap_and_unbounded_files() {
        let mut selection = policy(Path::new("/etc/test-source"));
        selection.mode = IsolationNetworkMode::Isolated;
        assert!(validate(&selection).is_err());
        selection.mode = IsolationNetworkMode::Host;
        selection
            .runtime_files
            .push(selection.runtime_files[0].clone());
        assert!(validate(&selection).is_err());
        selection.runtime_files.pop();
        selection.runtime_files[0].max_bytes = 0;
        assert!(validate(&selection).is_err());
    }
}
