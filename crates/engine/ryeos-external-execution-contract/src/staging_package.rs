//! Provider-neutral semantic inventory for one external guest staging package.
//!
//! This is not a filesystem extractor or a delivery receipt. The producer
//! enumerates exact inherited authorities through Lillux; the guest importer
//! must verify every streamed byte and materialize only through Lillux's
//! descriptor-relative, no-follow operations before starting the existing
//! fixed-FD supervisor. Neither a provider upload acknowledgement nor this
//! manifest alone authorizes Ready.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    ExternalGuestInputProjection, GuestMountContentAuthority, GuestMountKind, canonical_json,
    digest, from_json_slice_strict,
};

pub const GUEST_STAGING_PACKAGE_SCHEMA: u32 = 1;
// The base CAS transfer alone admits up to 400,010 filesystem entries. Other
// inputs share this package and must be accounted for by backend admission.
pub const MAX_GUEST_STAGING_ENTRIES: usize = 500_000;
pub const MAX_GUEST_STAGING_MANIFEST_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestStagingPackageManifest {
    pub schema: u32,
    pub activation_request_digest: String,
    pub guest_input_identity: String,
    pub bootstrap_sha256: String,
    pub supervisor_sha256: String,
    pub launcher_sha256: String,
    pub total_regular_bytes: u64,
    pub entries: Vec<GuestStagingEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestStagingEntry {
    Directory {
        path: String,
        mode: u32,
    },
    RegularFile {
        path: String,
        mode: u32,
        bytes: u64,
        sha256: String,
    },
}

impl GuestStagingEntry {
    pub fn path(&self) -> &str {
        match self {
            Self::Directory { path, .. } | Self::RegularFile { path, .. } => path,
        }
    }

    fn is_directory(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }
}

#[derive(Clone, Copy)]
enum RootKind<'a> {
    Directory,
    File {
        bytes: Option<u64>,
        hash: Option<&'a str>,
        mode: Option<u32>,
    },
}

impl GuestStagingPackageManifest {
    /// Decode only a bounded, duplicate-key-free manifest. Callers must still
    /// validate it against the retained activation before reading entry bytes.
    pub fn from_bounded_json(bytes: &[u8]) -> Result<Self> {
        from_json_slice_strict(bytes, MAX_GUEST_STAGING_MANIFEST_BYTES)
    }

    /// Validate the complete package inventory against the retained activation
    /// and guest projection. `maximum_regular_bytes` comes from the admitted
    /// placement binding; transport framing bytes require a separate bound.
    pub fn validate_for(
        &self,
        inputs: &ExternalGuestInputProjection,
        activation_request_digest: &str,
        bootstrap_sha256: &str,
        supervisor_sha256: &str,
        launcher_sha256: &str,
        maximum_regular_bytes: u64,
    ) -> Result<()> {
        inputs.validate()?;
        ensure!(
            self.schema == GUEST_STAGING_PACKAGE_SCHEMA,
            "guest staging schema changed"
        );
        for (value, label) in [
            (&self.activation_request_digest, "activation request"),
            (&self.guest_input_identity, "guest input identity"),
            (&self.bootstrap_sha256, "bootstrap"),
            (&self.supervisor_sha256, "supervisor"),
            (&self.launcher_sha256, "launcher"),
        ] {
            digest(value, label)?;
        }
        ensure!(
            self.activation_request_digest == activation_request_digest
                && self.guest_input_identity == inputs.identity_digest()?
                && self.bootstrap_sha256 == bootstrap_sha256
                && self.supervisor_sha256 == supervisor_sha256
                && self.launcher_sha256 == launcher_sha256,
            "guest staging identity changed"
        );
        ensure!(
            !self.entries.is_empty() && self.entries.len() <= MAX_GUEST_STAGING_ENTRIES,
            "guest staging entry count exceeds its bound"
        );

        let mut roots = BTreeMap::from([
            ("base".to_owned(), RootKind::Directory),
            (
                "bootstrap".to_owned(),
                RootKind::File {
                    bytes: None,
                    hash: Some(&self.bootstrap_sha256),
                    mode: None,
                },
            ),
            (
                "supervisor".to_owned(),
                RootKind::File {
                    bytes: None,
                    hash: Some(&self.supervisor_sha256),
                    mode: None,
                },
            ),
            (
                "launcher".to_owned(),
                RootKind::File {
                    bytes: None,
                    hash: Some(&self.launcher_sha256),
                    mode: None,
                },
            ),
        ]);
        if let Some(output) = &inputs.workspace_outputs {
            roots.insert(
                "workspace_outputs".to_owned(),
                RootKind::File {
                    bytes: Some(output.bytes),
                    hash: Some(&output.authority_hash),
                    mode: None,
                },
            );
        }
        for (index, input) in inputs.inputs.iter().enumerate() {
            if matches!(
                &input.content_authority,
                GuestMountContentAuthority::PrivateScratch { .. }
            ) {
                continue;
            }
            let kind = match input.kind {
                GuestMountKind::Directory => RootKind::Directory,
                GuestMountKind::RegularFile => RootKind::File {
                    bytes: Some(input.bytes),
                    hash: match &input.content_authority {
                        GuestMountContentAuthority::RawFile { sha256 } => Some(sha256),
                        _ => None,
                    },
                    mode: input.normalized_mode,
                },
            };
            roots.insert(format!("input-{index:02}"), kind);
        }
        for (index, (_, hash, bytes)) in inputs.record_descriptors().enumerate() {
            roots.insert(
                format!("record-{index:02}"),
                RootKind::File {
                    bytes: Some(bytes),
                    hash: Some(hash),
                    mode: None,
                },
            );
        }

        let mut previous: Option<&str> = None;
        let mut directories = BTreeSet::new();
        let mut observed_roots = BTreeSet::new();
        let mut total = 0_u64;
        for entry in &self.entries {
            let path = entry.path();
            validate_package_path(path)?;
            ensure!(
                previous.is_none_or(|old| old < path),
                "guest staging entries are not uniquely sorted"
            );
            previous = Some(path);
            let (root, parent) = match path.rsplit_once('/') {
                Some((parent, _)) => (path.split('/').next().unwrap_or_default(), Some(parent)),
                None => (path, None),
            };
            let expected = roots
                .get(root)
                .ok_or_else(|| anyhow::anyhow!("guest staging has an undeclared root"))?;
            match parent {
                None => {
                    observed_roots.insert(root);
                    ensure!(
                        entry.is_directory() == matches!(expected, RootKind::Directory),
                        "guest staging root kind changed"
                    );
                    if let (
                        RootKind::File { bytes, hash, mode },
                        GuestStagingEntry::RegularFile {
                            bytes: actual_bytes,
                            sha256,
                            mode: actual_mode,
                            ..
                        },
                    ) = (expected, entry)
                    {
                        ensure!(
                            bytes.is_none_or(|value| value == *actual_bytes)
                                && hash.is_none_or(|value| value == sha256)
                                && mode.is_none_or(|value| value == *actual_mode),
                            "guest staging root bytes or mode changed"
                        );
                    }
                }
                Some(parent) => {
                    ensure!(
                        matches!(expected, RootKind::Directory),
                        "guest staging descends through a file root"
                    );
                    ensure!(
                        directories.contains(parent),
                        "guest staging parent directory is absent"
                    );
                }
            }
            match entry {
                GuestStagingEntry::Directory { mode, .. } => {
                    ensure!(
                        matches!(*mode, 0o700 | 0o755),
                        "guest staging directory mode is invalid"
                    );
                    directories.insert(path);
                }
                GuestStagingEntry::RegularFile {
                    mode,
                    bytes,
                    sha256,
                    ..
                } => {
                    ensure!(
                        matches!(*mode, 0o600 | 0o644 | 0o700 | 0o755),
                        "guest staging file mode is invalid"
                    );
                    digest(sha256, "guest staging file")?;
                    total = total
                        .checked_add(*bytes)
                        .ok_or_else(|| anyhow::anyhow!("guest staging byte count overflow"))?;
                    ensure!(
                        total <= maximum_regular_bytes,
                        "guest staging transfer budget exceeded"
                    );
                }
            }
        }
        ensure!(
            observed_roots.len() == roots.len()
                && roots
                    .keys()
                    .all(|root| observed_roots.contains(root.as_str())),
            "guest staging root inventory is incomplete"
        );
        ensure!(
            self.total_regular_bytes == total,
            "guest staging byte total changed"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_GUEST_STAGING_MANIFEST_BYTES,
            "guest staging manifest exceeds its encoded bound"
        );
        Ok(())
    }
}

fn validate_package_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 4096
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && path.split('/').count() <= 32
            && path.split('/').all(|part| {
                !part.is_empty() && part.len() <= 255 && part != "." && part != ".."
            }),
        "guest staging path is not normalized"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, GuestBaseSnapshotInput, GuestMountAccess,
        GuestMountInput, GuestMountRole,
    };

    fn hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn fixture() -> (ExternalGuestInputProjection, GuestStagingPackageManifest) {
        let inputs = ExternalGuestInputProjection {
            schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 56,
                snapshot_hash: hash('a'),
                closure_digest: hash('b'),
                object_count: 3,
                blob_count: 1,
                total_bytes: 2,
            },
            workspace_outputs: None,
            inputs: vec![GuestMountInput {
                role: GuestMountRole::Configuration,
                authority_id: "config".to_owned(),
                descriptor: 64,
                destination: "/runtime/config".to_owned(),
                kind: GuestMountKind::RegularFile,
                access: GuestMountAccess::ReadOnly,
                normalized_mode: Some(0o644),
                content_authority: GuestMountContentAuthority::RawFile { sha256: hash('c') },
                bytes: 3,
            }],
            executable_search: vec![],
            environment: BTreeMap::new(),
        };
        let entries = vec![
            GuestStagingEntry::Directory {
                path: "base".to_owned(),
                mode: 0o700,
            },
            GuestStagingEntry::RegularFile {
                path: "base/object".to_owned(),
                mode: 0o600,
                bytes: 2,
                sha256: hash('d'),
            },
            GuestStagingEntry::RegularFile {
                path: "bootstrap".to_owned(),
                mode: 0o755,
                bytes: 4,
                sha256: hash('e'),
            },
            GuestStagingEntry::RegularFile {
                path: "input-00".to_owned(),
                mode: 0o644,
                bytes: 3,
                sha256: hash('c'),
            },
            GuestStagingEntry::RegularFile {
                path: "launcher".to_owned(),
                mode: 0o755,
                bytes: 5,
                sha256: hash('f'),
            },
            GuestStagingEntry::RegularFile {
                path: "supervisor".to_owned(),
                mode: 0o755,
                bytes: 6,
                sha256: hash('0'),
            },
        ];
        let manifest = GuestStagingPackageManifest {
            schema: GUEST_STAGING_PACKAGE_SCHEMA,
            activation_request_digest: hash('1'),
            guest_input_identity: inputs.identity_digest().unwrap(),
            bootstrap_sha256: hash('e'),
            supervisor_sha256: hash('0'),
            launcher_sha256: hash('f'),
            total_regular_bytes: 20,
            entries,
        };
        (inputs, manifest)
    }

    fn validates(
        inputs: &ExternalGuestInputProjection,
        package: &GuestStagingPackageManifest,
    ) -> bool {
        package
            .validate_for(inputs, &hash('1'), &hash('e'), &hash('0'), &hash('f'), 20)
            .is_ok()
    }

    #[test]
    fn accepts_exact_inventory_and_refuses_mutation() {
        let (inputs, mut package) = fixture();
        assert!(validates(&inputs, &package));

        package.entries[3] = GuestStagingEntry::RegularFile {
            path: "input-00".to_owned(),
            mode: 0o755,
            bytes: 3,
            sha256: hash('c'),
        };
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.entries[3] = GuestStagingEntry::RegularFile {
            path: "input-00".to_owned(),
            mode: 0o644,
            bytes: 3,
            sha256: hash('d'),
        };
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.entries.remove(3);
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.entries.push(GuestStagingEntry::Directory {
            path: "undeclared".to_owned(),
            mode: 0o700,
        });
        assert!(!validates(&inputs, &package));
    }

    #[test]
    fn refuses_unsafe_paths_order_and_budget() {
        let (inputs, mut package) = fixture();
        package.entries[1] = GuestStagingEntry::RegularFile {
            path: "base/../object".to_owned(),
            mode: 0o600,
            bytes: 2,
            sha256: hash('d'),
        };
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.entries.swap(1, 2);
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.total_regular_bytes = 21;
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        assert!(
            package
                .validate_for(&inputs, &hash('1'), &hash('e'), &hash('0'), &hash('f'), 19)
                .is_err()
        );
        package = fixture().1;
        assert!(
            package
                .validate_for(&inputs, &hash('1'), &hash('d'), &hash('0'), &hash('f'), 20)
                .is_err()
        );
    }

    #[test]
    fn bounded_decoder_refuses_duplicate_keys() {
        let (_, package) = fixture();
        let encoded = canonical_json(&package).unwrap();
        assert_eq!(
            GuestStagingPackageManifest::from_bounded_json(&encoded).unwrap(),
            package
        );
        assert!(
            GuestStagingPackageManifest::from_bounded_json(b"{\"schema\":1,\"schema\":2}").is_err()
        );
    }
}
