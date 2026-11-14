//! The node-signed ledger of revoked authorized-key grants.
//!
//! Revoking a grant writes a tombstone named by the grant's digest (the
//! sha256 of the exact signed grant file) next to the grant directory, and
//! only then removes the grant file. The authorized-key funnel refuses any
//! grant whose digest has a tombstone, so a saved copy of a revoked grant
//! stays dead if it is put back. A fresh grant for the same key has new
//! bytes and therefore a new digest: re-granting is an explicit act that the
//! ledger does not block.
//!
//! The ledger is the `revoked` sibling of the grant directory, so a node
//! that relocates its grants relocates the ledger with them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::identity::NodeIdentity;

/// Schema of a tombstone body.
pub const GRANT_TOMBSTONE_SCHEMA_VERSION: u32 = 1;

/// Longest reason a tombstone records.
pub const MAX_REVOCATION_REASON_BYTES: usize = 1024;

/// A revocation record: the evidence that one exact grant was withdrawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantTombstone {
    pub schema_version: u32,
    /// Fingerprint of the key the revoked grant authorized.
    pub fingerprint: String,
    /// Digest of the revoked grant file (`source_file_hash`).
    pub grant_digest: String,
    /// RFC 3339 UTC time of the revocation.
    pub revoked_at: String,
    /// Principal that revoked the grant, as `fp:<hex>`.
    pub revoked_by: String,
    pub reason: String,
}

/// The ledger directory that belongs to the grant directory `auth_dir`.
pub fn revocations_dir(auth_dir: &Path) -> PathBuf {
    auth_dir.with_file_name("revoked")
}

fn tombstone_name(grant_digest: &str) -> Result<String> {
    if !lillux::valid_hash(grant_digest) {
        bail!("grant digest is not a lowercase SHA-256 digest");
    }
    Ok(format!("{grant_digest}.toml"))
}

fn decode(body: &str, name: &str) -> Result<GrantTombstone> {
    let tombstone: GrantTombstone = toml::from_str(body)
        .with_context(|| format!("revocation tombstone {name} is malformed"))?;
    if tombstone.schema_version != GRANT_TOMBSTONE_SCHEMA_VERSION {
        bail!(
            "revocation tombstone {name} has schema_version {}, expected {GRANT_TOMBSTONE_SCHEMA_VERSION}",
            tombstone.schema_version
        );
    }
    if format!("{}.toml", tombstone.grant_digest) != name {
        bail!("revocation tombstone {name} names a different grant digest");
    }
    Ok(tombstone)
}

/// The tombstone for `grant_digest`, if the ledger holds one. A tombstone
/// that cannot be verified is an error, never an absent one: a ledger that
/// cannot be trusted must not let a revoked grant authenticate.
pub fn read_tombstone(
    revocations_dir: &Path,
    grant_digest: &str,
    node: &NodeIdentity,
) -> Result<Option<GrantTombstone>> {
    let name = tombstone_name(grant_digest)?;
    let Some(directory) = lillux::PinnedDirectory::open(revocations_dir)? else {
        return Ok(None);
    };
    crate::remote_delegation::read_node_signed(&directory, &name, node)?
        .map(|body| decode(&body, &name))
        .transpose()
}

/// Record a tombstone for the grant `grant_digest` of `fingerprint`.
/// Idempotent: an existing tombstone is kept and returned, because the first
/// revocation is the one that counts.
pub fn write_tombstone(
    revocations_dir: &Path,
    fingerprint: &str,
    grant_digest: &str,
    revoked_by: &str,
    reason: &str,
    node: &NodeIdentity,
) -> Result<GrantTombstone> {
    let name = tombstone_name(grant_digest)?;
    let directory = lillux::PinnedDirectory::open_or_create(revocations_dir)?;
    if let Some(body) = crate::remote_delegation::read_node_signed(&directory, &name, node)? {
        return decode(&body, &name);
    }
    let tombstone = GrantTombstone {
        schema_version: GRANT_TOMBSTONE_SCHEMA_VERSION,
        fingerprint: fingerprint.to_owned(),
        grant_digest: grant_digest.to_owned(),
        revoked_at: lillux::time::iso8601_now(),
        revoked_by: revoked_by.to_owned(),
        reason: reason.to_owned(),
    };
    let body = toml::to_string(&tombstone).context("serialize revocation tombstone")?;
    crate::remote_delegation::write_node_signed(&directory, &name, &body, node)?;
    directory.sync()?;
    Ok(tombstone)
}

/// What reading one ledger entry found.
#[derive(Debug, Clone)]
pub enum TombstoneState {
    Sound(Box<GrantTombstone>),
    /// Unsigned, tampered, signed by another key, or malformed.
    Invalid(String),
}

#[derive(Debug, Clone)]
pub struct TombstoneInspection {
    /// The ledger file name.
    pub name: String,
    pub state: TombstoneState,
}

/// Read every ledger entry without failing on the first bad one. The doctor
/// uses it; request authentication reads exactly one entry through
/// [`read_tombstone`].
pub fn inspect_tombstones(
    revocations_dir: &Path,
    node: &NodeIdentity,
) -> Result<Vec<TombstoneInspection>> {
    let Some(directory) = lillux::PinnedDirectory::open(revocations_dir)? else {
        return Ok(Vec::new());
    };
    let mut names = directory
        .entries_no_follow()?
        .into_iter()
        .map(|entry| entry.name.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    let mut inspections = Vec::with_capacity(names.len());
    for name in names {
        let state = match name.strip_suffix(".toml") {
            Some(digest) if lillux::valid_hash(digest) => {
                match crate::remote_delegation::read_node_signed(&directory, &name, node) {
                    Ok(Some(body)) => match decode(&body, &name) {
                        Ok(tombstone) => TombstoneState::Sound(Box::new(tombstone)),
                        Err(error) => TombstoneState::Invalid(format!("{error:#}")),
                    },
                    Ok(None) => TombstoneState::Invalid("the entry vanished while listing".into()),
                    Err(error) => TombstoneState::Invalid(format!("{error:#}")),
                }
            }
            _ => TombstoneState::Invalid(format!(
                "`{name}` is not a `<grant digest>.toml` tombstone"
            )),
        };
        inspections.push(TombstoneInspection { name, state });
    }
    Ok(inspections)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(dir: &Path) -> NodeIdentity {
        NodeIdentity::create(&dir.join("node.pem")).unwrap()
    }

    #[test]
    fn ledger_is_the_revoked_sibling_of_the_grant_directory() {
        assert_eq!(
            revocations_dir(Path::new("/n/.ai/node/auth/authorized_keys")),
            Path::new("/n/.ai/node/auth/revoked")
        );
    }

    #[test]
    fn tombstone_round_trips_and_first_revocation_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path());
        let dir = tmp.path().join("revoked");
        let digest = "a".repeat(64);
        let first =
            write_tombstone(&dir, &"b".repeat(64), &digest, "fp:op", "stolen", &node).unwrap();
        let second =
            write_tombstone(&dir, &"b".repeat(64), &digest, "fp:other", "again", &node).unwrap();
        assert_eq!(first, second);
        assert_eq!(read_tombstone(&dir, &digest, &node).unwrap(), Some(first));
        assert_eq!(read_tombstone(&dir, &"c".repeat(64), &node).unwrap(), None);
    }

    #[test]
    fn tampered_tombstone_is_an_error_not_an_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path());
        let dir = tmp.path().join("revoked");
        let digest = "a".repeat(64);
        write_tombstone(&dir, &"b".repeat(64), &digest, "fp:op", "stolen", &node).unwrap();
        let path = dir.join(format!("{digest}.toml"));
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("stolen", "mislaid")).unwrap();
        assert!(read_tombstone(&dir, &digest, &node).is_err());
        let inspected = inspect_tombstones(&dir, &node).unwrap();
        assert!(matches!(inspected[0].state, TombstoneState::Invalid(_)));
    }
}
