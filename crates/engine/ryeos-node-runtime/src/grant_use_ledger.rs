//! The node-signed ledger of uses of constrained grants.
//!
//! A grant whose offline scope carries `max_uses` may be applied only that
//! many times. Every use is recorded as a node-signed file named by the
//! digest of the request, inside a directory named by the digest of the
//! grant, under the `grant-uses` sibling of the grant directory. The ledger
//! is append-only and lives outside runtime SQLite and outside execution
//! history. The claim is written before the effect, so a crash between claim
//! and effect still counts as spent; a request recovered after stopping
//! half-way finds its own record and resumes without spending another use.
//!
//! Records resist forgery (they are node-signed and verified on every read)
//! and not deletion: removing one needs the node user's filesystem rights.
//! The ledger is identity-class state: no execution-history reset, cut or
//! epoch may remove it.
//!
//! A grant binds its node because the grant file is node-signed and each
//! ledger record is verified under the same node key. A reinstalled node
//! with a new key refuses the old records and the old grants alike.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::identity::NodeIdentity;

/// Schema of a use record body.
pub const GRANT_USE_SCHEMA_VERSION: u32 = 1;

/// Mode of the ledger directories.
const LEDGER_DIRECTORY_MODE: u32 = 0o700;
/// Mode of a use record.
const LEDGER_RECORD_MODE: u32 = 0o600;

/// Where a use stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UseState {
    Claimed,
    Completed,
}

/// One use of a grant: the evidence that one exact request was admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantUseRecord {
    pub schema_version: u32,
    /// Digest of the exact signed grant file.
    pub grant_digest: String,
    /// Digest of the exact request.
    pub request_digest: String,
    /// The offline scope the use was made under.
    pub scope: String,
    /// RFC 3339 UTC time of the claim.
    pub claimed_at: String,
    pub state: UseState,
    /// RFC 3339 UTC time the effect finished, once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

/// Why a use was not admitted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrantUseError {
    #[error("grant {grant_digest} has no uses left (max_uses {max_uses})")]
    Exhausted { grant_digest: String, max_uses: u32 },
}

impl GrantUseError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Exhausted { .. } => "grant_uses_exhausted",
        }
    }
}

/// What a claim did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// A new record was written: this request spent a use.
    Claimed,
    /// A verified record for this request already existed: it counts once.
    Resumed,
}

/// The ledger directory that belongs to the grant directory `auth_dir`.
pub fn grant_uses_dir(auth_dir: &Path) -> PathBuf {
    auth_dir.with_file_name("grant-uses")
}

fn check_digest(label: &str, digest: &str) -> Result<()> {
    if !lillux::valid_hash(digest) {
        bail!("{label} is not a lowercase SHA-256 digest");
    }
    Ok(())
}

fn record_name(request_digest: &str) -> String {
    format!("{request_digest}.toml")
}

fn decode(body: &str, name: &str, grant_digest: &str) -> Result<GrantUseRecord> {
    let record: GrantUseRecord =
        toml::from_str(body).with_context(|| format!("grant use record {name} is malformed"))?;
    if record.schema_version != GRANT_USE_SCHEMA_VERSION {
        bail!(
            "grant use record {name} has schema_version {}, expected {GRANT_USE_SCHEMA_VERSION}",
            record.schema_version
        );
    }
    if record_name(&record.request_digest) != name {
        bail!("grant use record {name} names a different request digest");
    }
    if record.grant_digest != grant_digest {
        bail!("grant use record {name} names a different grant digest");
    }
    check_digest("record request digest", &record.request_digest)?;
    Ok(record)
}

fn read_record(
    directory: &lillux::PinnedDirectory,
    request_digest: &str,
    grant_digest: &str,
    node: &NodeIdentity,
) -> Result<Option<GrantUseRecord>> {
    let name = record_name(request_digest);
    crate::remote_delegation::read_node_signed(directory, &name, node)?
        .map(|body| decode(&body, &name, grant_digest))
        .transpose()
}

/// Write one record. The caller holds the directory's exclusive lock, so this
/// does not take it again.
fn write_record_locked(
    directory: &lillux::PinnedDirectory,
    record: &GrantUseRecord,
    node: &NodeIdentity,
) -> Result<()> {
    let name = record_name(&record.request_digest);
    let body = toml::to_string(record).context("serialize grant use record")?;
    let signed = lillux::signature::sign_content(&body, node.signing_key(), "#", None);
    directory.ensure_path_binding()?;
    let incumbent = directory.open_pinned_regular(OsStr::new(&name), false)?;
    directory
        .replace_pinned_bytes_if_matches_atomic(
            OsStr::new(&name),
            incumbent.as_ref(),
            |_| Ok(()),
            signed.as_bytes(),
            LEDGER_RECORD_MODE,
        )
        .map_err(anyhow::Error::from)?;
    directory.sync()
}

/// Count every record of the grant directory, verifying each. A stray entry
/// or a record that fails verification is an error, never an ignored file.
fn count_verified(
    directory: &lillux::PinnedDirectory,
    grant_digest: &str,
    node: &NodeIdentity,
) -> Result<u32> {
    let mut count: u32 = 0;
    for entry in directory.entries_no_follow()? {
        let name = entry.name.to_string_lossy().into_owned();
        let Some(request_digest) = name.strip_suffix(".toml") else {
            bail!("grant use ledger holds `{name}`, which is not a `<request digest>.toml` record");
        };
        check_digest("ledger entry name", request_digest)?;
        if read_record(directory, request_digest, grant_digest, node)?.is_none() {
            bail!("grant use record {name} vanished while counting");
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("grant use ledger count overflows"))?;
    }
    Ok(count)
}

fn grant_directory(
    ledger_root: &Path,
    grant_digest: &str,
    create: bool,
) -> Result<Option<lillux::PinnedDirectory>> {
    if create {
        let root = lillux::PinnedDirectory::open_or_create(ledger_root)?;
        // The creation mode must not depend on the process umask.
        root.set_mode(LEDGER_DIRECTORY_MODE)?;
        Ok(Some(root.open_or_create_child(
            OsStr::new(grant_digest),
            LEDGER_DIRECTORY_MODE,
        )?))
    } else {
        lillux::PinnedDirectory::open(&ledger_root.join(grant_digest))
    }
}

/// Spend one use of `grant_digest` for `request_digest` before the effect.
/// A request that already holds a verified record is `Resumed` and spends
/// nothing more. Otherwise, with `max_uses` records already present, the
/// claim fails with [`GrantUseError::Exhausted`].
pub fn claim_use(
    ledger_root: &Path,
    grant_digest: &str,
    request_digest: &str,
    scope: &str,
    max_uses: u32,
    node: &NodeIdentity,
) -> Result<Claim> {
    check_digest("grant digest", grant_digest)?;
    check_digest("request digest", request_digest)?;
    let directory = grant_directory(ledger_root, grant_digest, true)?
        .expect("creating open returns a directory");
    let _lock = directory.lock_exclusive()?;
    directory.ensure_path_binding()?;
    if read_record(&directory, request_digest, grant_digest, node)?.is_some() {
        return Ok(Claim::Resumed);
    }
    if count_verified(&directory, grant_digest, node)? >= max_uses {
        return Err(GrantUseError::Exhausted {
            grant_digest: grant_digest.to_owned(),
            max_uses,
        }
        .into());
    }
    let record = GrantUseRecord {
        schema_version: GRANT_USE_SCHEMA_VERSION,
        grant_digest: grant_digest.to_owned(),
        request_digest: request_digest.to_owned(),
        scope: scope.to_owned(),
        claimed_at: lillux::time::iso8601_now(),
        state: UseState::Claimed,
        completed_at: None,
    };
    write_record_locked(&directory, &record, node)?;
    Ok(Claim::Claimed)
}

/// Mark the use for `request_digest` completed. Idempotent; an error when no
/// record exists.
pub fn complete_use(
    ledger_root: &Path,
    grant_digest: &str,
    request_digest: &str,
    node: &NodeIdentity,
) -> Result<()> {
    check_digest("grant digest", grant_digest)?;
    check_digest("request digest", request_digest)?;
    let Some(directory) = grant_directory(ledger_root, grant_digest, false)? else {
        bail!("no claimed use of grant {grant_digest} for request {request_digest}");
    };
    let _lock = directory.lock_exclusive()?;
    directory.ensure_path_binding()?;
    let Some(mut record) = read_record(&directory, request_digest, grant_digest, node)? else {
        bail!("no claimed use of grant {grant_digest} for request {request_digest}");
    };
    if record.state == UseState::Completed {
        return Ok(());
    }
    record.state = UseState::Completed;
    record.completed_at = Some(lillux::time::iso8601_now());
    write_record_locked(&directory, &record, node)
}

/// How many uses of `grant_digest` the ledger holds, claimed or completed.
/// A record that fails verification is an error, never a smaller count.
pub fn uses_spent(ledger_root: &Path, grant_digest: &str, node: &NodeIdentity) -> Result<u32> {
    check_digest("grant digest", grant_digest)?;
    let Some(directory) = grant_directory(ledger_root, grant_digest, false)? else {
        return Ok(0);
    };
    count_verified(&directory, grant_digest, node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn node(dir: &Path, file: &str) -> NodeIdentity {
        NodeIdentity::create(&dir.join(file)).unwrap()
    }

    fn d(c: char) -> String {
        c.to_string().repeat(64)
    }

    #[test]
    fn ledger_is_the_grant_uses_sibling_of_the_grant_directory() {
        assert_eq!(
            grant_uses_dir(Path::new("/n/.ai/node/auth/authorized_keys")),
            Path::new("/n/.ai/node/auth/grant-uses")
        );
    }

    #[test]
    fn second_distinct_request_is_exhausted_at_one_use() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path(), "node.pem");
        let root = tmp.path().join("grant-uses");
        let grant = d('a');
        assert_eq!(
            claim_use(&root, &grant, &d('1'), "s", 1, &node).unwrap(),
            Claim::Claimed
        );
        let error = claim_use(&root, &grant, &d('2'), "s", 1, &node).unwrap_err();
        let typed = error.downcast_ref::<GrantUseError>().unwrap();
        assert_eq!(typed.code(), "grant_uses_exhausted");
        assert_eq!(uses_spent(&root, &grant, &node).unwrap(), 1);
    }

    #[test]
    fn resuming_the_same_request_is_not_a_second_use() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path(), "node.pem");
        let root = tmp.path().join("grant-uses");
        let grant = d('a');
        claim_use(&root, &grant, &d('1'), "s", 1, &node).unwrap();
        assert_eq!(
            claim_use(&root, &grant, &d('1'), "s", 1, &node).unwrap(),
            Claim::Resumed
        );
        complete_use(&root, &grant, &d('1'), &node).unwrap();
        complete_use(&root, &grant, &d('1'), &node).unwrap();
        assert_eq!(
            claim_use(&root, &grant, &d('1'), "s", 1, &node).unwrap(),
            Claim::Resumed
        );
        assert_eq!(uses_spent(&root, &grant, &node).unwrap(), 1);
    }

    #[test]
    fn completing_an_unclaimed_request_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path(), "node.pem");
        let root = tmp.path().join("grant-uses");
        assert!(complete_use(&root, &d('a'), &d('1'), &node).is_err());
        claim_use(&root, &d('a'), &d('2'), "s", 1, &node).unwrap();
        assert!(complete_use(&root, &d('a'), &d('1'), &node).is_err());
    }

    #[test]
    fn concurrent_claims_on_one_use_admit_exactly_one() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path(), "node.pem");
        let root = tmp.path().join("grant-uses");
        let grant = d('a');
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let handles = ['1', '2'].map(|c| {
                let (root, grant, node, barrier) = (&root, &grant, &node, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    claim_use(root, grant, &d(c), "s", 1, node)
                })
            });
            handles.map(|handle| handle.join().unwrap())
        });
        let claimed = results
            .iter()
            .filter(|r| matches!(r, Ok(Claim::Claimed)))
            .count();
        let exhausted = results
            .iter()
            .filter(|r| {
                r.as_ref().err().is_some_and(|e| {
                    matches!(
                        e.downcast_ref::<GrantUseError>(),
                        Some(GrantUseError::Exhausted { .. })
                    )
                })
            })
            .count();
        assert_eq!((claimed, exhausted), (1, 1));
        assert_eq!(uses_spent(&root, &grant, &node).unwrap(), 1);
    }

    #[test]
    fn another_nodes_ledger_is_refused_not_counted_as_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let a = node(tmp.path(), "a.pem");
        let b = node(tmp.path(), "b.pem");
        let root = tmp.path().join("grant-uses");
        let grant = d('a');
        claim_use(&root, &grant, &d('1'), "s", 1, &a).unwrap();
        assert!(uses_spent(&root, &grant, &b).is_err());
        assert!(claim_use(&root, &grant, &d('2'), "s", 5, &b).is_err());
    }

    #[test]
    fn tampered_record_is_an_error_not_a_zero_count() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path(), "node.pem");
        let root = tmp.path().join("grant-uses");
        let grant = d('a');
        claim_use(&root, &grant, &d('1'), "scope-x", 3, &node).unwrap();
        let path = root.join(&grant).join(format!("{}.toml", d('1')));
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("scope-x", "scope-y")).unwrap();
        assert!(uses_spent(&root, &grant, &node).is_err());
        assert!(claim_use(&root, &grant, &d('2'), "s", 3, &node).is_err());
    }

    #[test]
    fn spent_use_is_counted_after_the_grant_file_is_put_back() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path(), "node.pem");
        let root = tmp.path().join("grant-uses");
        let grant = d('a');
        let grant_file = tmp.path().join("authorized_keys").join("g.toml");
        std::fs::create_dir_all(grant_file.parent().unwrap()).unwrap();
        std::fs::write(&grant_file, "grant").unwrap();
        claim_use(&root, &grant, &d('1'), "s", 1, &node).unwrap();
        std::fs::remove_file(&grant_file).unwrap();
        std::fs::write(&grant_file, "grant").unwrap();
        assert_eq!(uses_spent(&root, &grant, &node).unwrap(), 1);
        assert!(claim_use(&root, &grant, &d('2'), "s", 1, &node).is_err());
    }

    #[test]
    fn ledger_modes_are_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path(), "node.pem");
        let root = tmp.path().join("grant-uses");
        let grant = d('a');
        claim_use(&root, &grant, &d('1'), "s", 1, &node).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join(&grant)), 0o700);
        assert_eq!(
            mode(&root.join(&grant).join(format!("{}.toml", d('1')))),
            0o600
        );
    }
}
