//! Exact character-device observation and descriptor authority.
//!
//! Callers supply node-owned role/path declarations. Lillux owns every OS
//! operation: no-follow open, type/identity validation, descriptor retention,
//! and the stable binding digest consumed by higher layers.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAX_CHARACTER_DEVICES: usize = 32;
const MAX_ROLE_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CharacterDeviceAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CharacterDeviceSpec {
    pub role: String,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub access: CharacterDeviceAccess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CharacterDeviceIdentity {
    pub role: String,
    pub destination: PathBuf,
    pub access: CharacterDeviceAccess,
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug, Clone)]
pub struct CharacterDeviceAuthority {
    identity: CharacterDeviceIdentity,
    descriptor: crate::InheritedDescriptorAuthority,
}

#[derive(Debug, Clone)]
pub struct CharacterDeviceSet {
    identities: Vec<CharacterDeviceIdentity>,
    authorities: Vec<CharacterDeviceAuthority>,
    binding_digest: String,
}

impl CharacterDeviceSpec {
    pub fn validate(&self) -> Result<(), String> {
        validate_role(&self.role)?;
        validate_absolute_normal_path(&self.source, "character-device source")?;
        validate_absolute_normal_path(&self.destination, "character-device destination")?;
        if !self.destination.starts_with("/dev") || self.destination == Path::new("/dev") {
            return Err("character-device destination must be beneath /dev".to_owned());
        }
        if self.access == CharacterDeviceAccess::ReadOnly {
            return Err(
                "read-only character-device access has no qualified enforcement backend".to_owned(),
            );
        }
        Ok(())
    }
}

impl CharacterDeviceSet {
    pub fn observe(specs: &[CharacterDeviceSpec]) -> Result<Self, String> {
        if specs.is_empty() || specs.len() > MAX_CHARACTER_DEVICES {
            return Err(format!(
                "character-device set must contain between 1 and {MAX_CHARACTER_DEVICES} entries"
            ));
        }
        let mut previous_role: Option<&str> = None;
        let mut destinations = BTreeSet::new();
        let lease = crate::retain_fork_sensitive_descriptors();
        let mut identities = Vec::with_capacity(specs.len());
        let mut authorities = Vec::with_capacity(specs.len());
        for spec in specs {
            spec.validate()?;
            if previous_role.is_some_and(|previous| previous >= spec.role.as_str()) {
                return Err("character-device roles must be sorted and unique".to_owned());
            }
            if !destinations.insert(spec.destination.clone()) {
                return Err("character-device destinations must be unique".to_owned());
            }
            previous_role = Some(&spec.role);
            let encoded = CString::new(spec.source.as_os_str().as_encoded_bytes())
                .map_err(|_| "character-device source contains NUL".to_owned())?;
            let fd = unsafe {
                libc::open(
                    encoded.as_ptr(),
                    libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(format!(
                    "open character-device source {}: {}",
                    spec.source.display(),
                    std::io::Error::last_os_error()
                ));
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                return Err(format!(
                    "inspect character-device source {}: {}",
                    spec.source.display(),
                    std::io::Error::last_os_error()
                ));
            }
            let stat = unsafe { stat.assume_init() };
            if stat.st_mode & libc::S_IFMT != libc::S_IFCHR {
                return Err(format!(
                    "character-device source {} is not a character device",
                    spec.source.display()
                ));
            }
            let identity = CharacterDeviceIdentity {
                role: spec.role.clone(),
                destination: spec.destination.clone(),
                access: spec.access,
                major: libc::major(stat.st_rdev),
                minor: libc::minor(stat.st_rdev),
            };
            let descriptor =
                crate::exec::InheritedDescriptorAuthority::from_owned_file(file, &lease)?;
            identities.push(identity.clone());
            authorities.push(CharacterDeviceAuthority {
                identity,
                descriptor,
            });
        }
        let identity_value = serde_json::to_value(&identities)
            .map_err(|error| format!("encode character-device binding: {error}"))?;
        let canonical = crate::cas::canonical_json(&identity_value)
            .map_err(|error| format!("encode character-device binding: {error}"))?;
        let binding_digest = crate::cas::sha256_hex(canonical.as_bytes());
        Ok(Self {
            identities,
            authorities,
            binding_digest,
        })
    }

    pub fn identities(&self) -> &[CharacterDeviceIdentity] {
        &self.identities
    }

    /// Combine already-observed descriptor authorities without reopening any
    /// host path. Lillux owns uniqueness and binding construction because the
    /// result remains an OS-facing descriptor capability, not RyeOS data.
    pub fn combine(sets: &[&Self]) -> Result<Self, String> {
        let total = sets
            .iter()
            .try_fold(0_usize, |total, set| {
                total.checked_add(set.identities.len())
            })
            .ok_or_else(|| "combined character-device count overflow".to_owned())?;
        if total == 0 || total > MAX_CHARACTER_DEVICES {
            return Err(format!(
                "combined character-device set must contain between 1 and {MAX_CHARACTER_DEVICES} entries"
            ));
        }
        let mut entries = sets
            .iter()
            .flat_map(|set| {
                set.identities
                    .iter()
                    .cloned()
                    .zip(set.authorities.iter().cloned())
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.role.cmp(&right.0.role));
        let mut previous_role: Option<&str> = None;
        let mut destinations = BTreeSet::new();
        for (identity, _) in &entries {
            if previous_role.is_some_and(|previous| previous >= identity.role.as_str()) {
                return Err("combined character-device roles must be unique".to_owned());
            }
            if !destinations.insert(identity.destination.clone()) {
                return Err("combined character-device destinations must be unique".to_owned());
            }
            previous_role = Some(identity.role.as_str());
        }
        let (identities, authorities): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
        let identity_value = serde_json::to_value(&identities)
            .map_err(|error| format!("encode combined character-device binding: {error}"))?;
        let canonical = crate::cas::canonical_json(&identity_value)
            .map_err(|error| format!("encode combined character-device binding: {error}"))?;
        let binding_digest = crate::cas::sha256_hex(canonical.as_bytes());
        Ok(Self {
            identities,
            authorities,
            binding_digest,
        })
    }

    pub fn authorities(&self) -> &[CharacterDeviceAuthority] {
        &self.authorities
    }

    pub fn binding_digest(&self) -> &str {
        &self.binding_digest
    }
}

/// Bind one node-owned semantic observation to the exact host device
/// identities Lillux retained. The fact vocabulary remains data owned by the
/// observation contract; Lillux supplies canonicalization and the concrete
/// descriptor identities rather than interpreting vendor concepts.
pub fn resource_observation_contract_digest<T: Serialize>(
    stable_id: &str,
    class: &str,
    facts: &T,
    devices: &[CharacterDeviceIdentity],
) -> Result<String, String> {
    let value = serde_json::json!({
        "schema": 1,
        "stable_id": stable_id,
        "class": class,
        "facts": facts,
        "character_devices": devices,
    });
    let canonical = crate::cas::canonical_json(&value)
        .map_err(|error| format!("encode resource observation contract: {error}"))?;
    Ok(crate::cas::sha256_hex(canonical.as_bytes()))
}

impl CharacterDeviceAuthority {
    pub fn identity(&self) -> &CharacterDeviceIdentity {
        &self.identity
    }

    pub fn inherited_descriptor(&self) -> Result<u32, String> {
        self.descriptor.inherited_descriptor()
    }

    pub fn retain_for_child(&self, target: &mut Vec<crate::InheritedDescriptorAuthority>) {
        target.push(self.descriptor.clone());
    }
}

fn validate_role(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_ROLE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err("character-device role is not a bounded canonical token".to_owned());
    }
    Ok(())
}

fn validate_absolute_normal_path(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute()
        || path.as_os_str().as_encoded_bytes().len() > 4096
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(format!("{label} must be an absolute normalized path"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn descriptor_contract_rejects_non_device_and_non_dev_destination() {
        let non_device = CharacterDeviceSpec {
            role: "candidate".to_owned(),
            source: "/etc/hosts".into(),
            destination: "/dev/candidate".into(),
            access: CharacterDeviceAccess::ReadWrite,
        };
        assert!(CharacterDeviceSet::observe(&[non_device]).is_err());
        let outside = CharacterDeviceSpec {
            role: "null".to_owned(),
            source: "/dev/null".into(),
            destination: "/tmp/null".into(),
            access: CharacterDeviceAccess::ReadWrite,
        };
        assert!(outside.validate().is_err());
    }

    #[test]
    fn observed_identity_is_content_addressed_and_exact() {
        let spec = CharacterDeviceSpec {
            role: "null".to_owned(),
            source: "/dev/null".into(),
            destination: "/dev/test-null".into(),
            access: CharacterDeviceAccess::ReadWrite,
        };
        let observed = CharacterDeviceSet::observe(&[spec]).unwrap();
        assert_eq!(observed.identities()[0].major, 1);
        assert_eq!(observed.identities()[0].minor, 3);
        assert_eq!(observed.binding_digest().len(), 64);
    }

    #[test]
    fn observation_refuses_symlink_substitution_and_binds_access() {
        let directory = tempfile::tempdir().unwrap();
        let linked = directory.path().join("null-link");
        symlink("/dev/null", &linked).unwrap();
        assert!(
            CharacterDeviceSet::observe(&[CharacterDeviceSpec {
                role: "null".to_owned(),
                source: linked,
                destination: "/dev/test-null".into(),
                access: CharacterDeviceAccess::ReadWrite,
            }])
            .is_err()
        );

        assert!(
            CharacterDeviceSet::observe(&[CharacterDeviceSpec {
                role: "null".to_owned(),
                source: "/dev/null".into(),
                destination: "/dev/test-null".into(),
                access: CharacterDeviceAccess::ReadOnly,
            }])
            .is_err()
        );
        let read_write = CharacterDeviceSet::observe(&[CharacterDeviceSpec {
            role: "null".to_owned(),
            source: "/dev/null".into(),
            destination: "/dev/test-null".into(),
            access: CharacterDeviceAccess::ReadWrite,
        }])
        .unwrap();
        assert_eq!(
            read_write.identities()[0].access,
            CharacterDeviceAccess::ReadWrite
        );
    }

    #[test]
    fn observed_sets_combine_without_reopening_paths() {
        let first = CharacterDeviceSet::observe(&[CharacterDeviceSpec {
            role: "null-a".to_owned(),
            source: "/dev/null".into(),
            destination: "/dev/test-null-a".into(),
            access: CharacterDeviceAccess::ReadWrite,
        }])
        .unwrap();
        let second = CharacterDeviceSet::observe(&[CharacterDeviceSpec {
            role: "null-b".to_owned(),
            source: "/dev/null".into(),
            destination: "/dev/test-null-b".into(),
            access: CharacterDeviceAccess::ReadWrite,
        }])
        .unwrap();
        let combined = CharacterDeviceSet::combine(&[&second, &first]).unwrap();
        assert_eq!(combined.identities().len(), 2);
        assert_eq!(combined.identities()[0].role, "null-a");
        assert_eq!(combined.identities()[1].role, "null-b");
        assert_eq!(combined.binding_digest().len(), 64);
    }
}
