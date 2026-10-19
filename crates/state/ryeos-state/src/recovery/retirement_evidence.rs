//! Retained evidence about retired chains: identity fences, closure releases
//! and evidence obligations.
//!
//! A fence is a small record proving a chain identity can never append again.
//! A release record proves the chain's retained closure has been let go. An
//! obligation records that a holder chain depends on an owner chain's retained
//! prefix, so the owner's closure is not released while the holder is unfenced.
//! The records carry identity, head and transition facts only. Their meaning to
//! any consumer is supplied by the caller.
//!
//! Fences and releases live until execution-history reset; obligations live
//! until the owner's release or reset. No age-out exists: dropping a fence
//! could pin another chain's unreleased closure forever.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{CasMutationGuard, RecoveryStore, validate_chain_root_id};

pub(crate) const RETIRED_CHAINS_DIRECTORY: &str = "retired-chains";
pub(crate) const EVIDENCE_OBLIGATIONS_DIRECTORY: &str = "obligations";
const FENCE_SCHEMA: u32 = 1;
const FENCE_KIND: &str = "retired_chain_identity";
const RELEASE_KIND: &str = "retired_chain_release";
const OBLIGATION_KIND: &str = "evidence_obligation";
const RELEASE_SUFFIX: &str = ".released.json";
const MAX_FENCE_RECORD_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FenceRecord {
    schema: u32,
    kind: String,
    chain_root_id: String,
    manifest_hash: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ObligationRecord {
    schema: u32,
    kind: String,
    owner_chain_root_id: String,
    holder_chain_root_id: String,
}

/// What the fence store knows about one chain identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetiredChainFence {
    /// No fence record exists. Absence proves nothing about the chain.
    Absent,
    /// The chain can never append again; its closure is retained.
    Retired { manifest_hash: String },
    /// The chain can never append again and its closure was released.
    Released { manifest_hash: String },
}

/// Records removed (or, on a dry run, found) by an execution-history reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetirementEvidenceDiscard {
    pub retirement_fences: usize,
    pub evidence_obligations: usize,
}

fn identity_digest(chain_root_id: &str) -> Result<String> {
    validate_chain_root_id(chain_root_id)?;
    Ok(lillux::sha256_hex(chain_root_id.as_bytes()))
}

fn fence_name(digest: &str) -> String {
    format!("{digest}.json")
}

fn release_name(digest: &str) -> String {
    format!("{digest}{RELEASE_SUFFIX}")
}

fn read_record_bytes(mut file: std::fs::File, what: &str) -> Result<Vec<u8>> {
    let length = file
        .metadata()
        .with_context(|| format!("inspect {what}"))?
        .len();
    anyhow::ensure!(length <= MAX_FENCE_RECORD_BYTES, "{what} is oversized");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read {what}"))?;
    Ok(bytes)
}

fn encode<T: Serialize>(record: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(record).context("serialize retirement evidence")
}

fn ensure_state_name(name: &std::ffi::OsStr) -> Result<()> {
    match lillux::classify_secure_temp_name(name, crate::STATE_TEMP_PURPOSE) {
        lillux::SecureTempOwnership::NotTemp => Ok(()),
        _ => anyhow::bail!(
            "retirement evidence directory holds an interrupted publication: {}; \
             restart the node so the startup sweep removes it",
            name.to_string_lossy()
        ),
    }
}

impl RecoveryStore {
    fn fence_directory(&self) -> Result<Option<lillux::PinnedDirectory>> {
        self.open_child_directory(RETIRED_CHAINS_DIRECTORY)
    }

    fn chain_head_present(&self, chain_root_id: &str) -> Result<bool> {
        validate_chain_root_id(chain_root_id)?;
        let Some(refs) = self
            .runtime_directory
            .open_child_directory(std::ffi::OsStr::new("refs"))?
        else {
            return Ok(false);
        };
        let Some(generic) = refs.open_child_directory(std::ffi::OsStr::new("generic"))? else {
            return Ok(false);
        };
        let Some(chains) = generic.open_child_directory(std::ffi::OsStr::new("chains"))? else {
            return Ok(false);
        };
        let Some(chain) = chains.open_child_directory(std::ffi::OsStr::new(chain_root_id))? else {
            return Ok(false);
        };
        Ok(chain
            .entry_no_follow(std::ffi::OsStr::new("head"))?
            .is_some())
    }

    fn read_fence_record(&self, chain_root_id: &str) -> Result<Option<FenceRecord>> {
        let digest = identity_digest(chain_root_id)?;
        let Some(directory) = self.fence_directory()? else {
            return Ok(None);
        };
        let name = fence_name(&digest);
        let Some(file) = directory.open_regular(std::ffi::OsStr::new(&name), false)? else {
            return Ok(None);
        };
        let bytes = read_record_bytes(file, "retired chain fence")?;
        let record: FenceRecord =
            serde_json::from_slice(&bytes).context("decode retired chain fence")?;
        anyhow::ensure!(
            record.schema == FENCE_SCHEMA
                && record.kind == FENCE_KIND
                && record.chain_root_id == chain_root_id,
            "retired chain fence {name} does not describe {chain_root_id:?}"
        );
        Ok(Some(record))
    }

    fn read_release_record(&self, chain_root_id: &str) -> Result<Option<FenceRecord>> {
        let digest = identity_digest(chain_root_id)?;
        let Some(directory) = self.fence_directory()? else {
            return Ok(None);
        };
        let name = release_name(&digest);
        let Some(file) = directory.open_regular(std::ffi::OsStr::new(&name), false)? else {
            return Ok(None);
        };
        let bytes = read_record_bytes(file, "retired chain release")?;
        let record: FenceRecord =
            serde_json::from_slice(&bytes).context("decode retired chain release")?;
        anyhow::ensure!(
            record.schema == FENCE_SCHEMA
                && record.kind == RELEASE_KIND
                && record.chain_root_id == chain_root_id,
            "retired chain release {name} does not describe {chain_root_id:?}"
        );
        Ok(Some(record))
    }

    /// What the fence store records for this chain identity. A release without
    /// its fence is corruption and refuses.
    pub fn retired_chain_fence(&self, chain_root_id: &str) -> Result<RetiredChainFence> {
        let fence = self.read_fence_record(chain_root_id)?;
        let release = self.read_release_record(chain_root_id)?;
        match (fence, release) {
            (None, None) => Ok(RetiredChainFence::Absent),
            (Some(fence), None) => Ok(RetiredChainFence::Retired {
                manifest_hash: fence.manifest_hash,
            }),
            (Some(fence), Some(release)) => {
                anyhow::ensure!(
                    fence.manifest_hash == release.manifest_hash,
                    "retired chain release names a different closure than its fence"
                );
                Ok(RetiredChainFence::Released {
                    manifest_hash: release.manifest_hash,
                })
            }
            (None, Some(_)) => {
                anyhow::bail!("retired chain release exists without its fence: {chain_root_id:?}")
            }
        }
    }

    /// Closure manifests that must stay rooted: every fence without a release.
    pub fn unreleased_retirement_manifest_roots(&self) -> Result<BTreeSet<String>> {
        Ok(self.unreleased_retired_chains()?.into_values().collect())
    }

    /// Every fenced chain whose closure is not yet released, with the manifest
    /// its fence binds. The release pass walks these owners.
    pub fn unreleased_retired_chains(&self) -> Result<BTreeMap<String, String>> {
        let Some(directory) = self.fence_directory()? else {
            return Ok(BTreeMap::new());
        };
        let mut roots = BTreeMap::new();
        let mut fences = Vec::new();
        let mut released = BTreeSet::new();
        for name in directory.entry_names()? {
            ensure_state_name(&name)?;
            let text = name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 retirement fence name"))?
                .to_string();
            if let Some(digest) = text.strip_suffix(RELEASE_SUFFIX) {
                released.insert(digest.to_string());
            } else if let Some(digest) = text.strip_suffix(".json") {
                let file = directory
                    .open_regular(&name, false)?
                    .ok_or_else(|| anyhow::anyhow!("retirement fence disappeared: {text}"))?;
                let record: FenceRecord =
                    serde_json::from_slice(&read_record_bytes(file, "retired chain fence")?)
                        .context("decode retired chain fence")?;
                anyhow::ensure!(
                    record.schema == FENCE_SCHEMA
                        && record.kind == FENCE_KIND
                        && identity_digest(&record.chain_root_id)? == digest,
                    "retired chain fence {text} is not self-consistent"
                );
                fences.push((
                    digest.to_string(),
                    record.chain_root_id,
                    record.manifest_hash,
                ));
            } else {
                anyhow::bail!("unexpected retirement fence entry: {text}");
            }
        }
        let fenced: BTreeSet<&String> = fences.iter().map(|(digest, _, _)| digest).collect();
        if let Some(orphan) = released.iter().find(|digest| !fenced.contains(digest)) {
            anyhow::bail!("retired chain release {orphan} exists without its fence");
        }
        for (digest, chain_root_id, manifest_hash) in &fences {
            if !released.contains(digest) {
                roots.insert(chain_root_id.clone(), manifest_hash.clone());
            }
        }
        Ok(roots)
    }

    /// Holders whose obligations name this owner. The owner's chain lock
    /// serializes the obligation directory, so no holder can appear between
    /// this read and the fence publication made under the same lock.
    pub fn evidence_obligation_holders(
        &self,
        owner_lock: &crate::chain::ChainLock,
        owner_chain_root_id: &str,
    ) -> Result<BTreeSet<String>> {
        self.ensure_chain_lock(owner_lock, owner_chain_root_id)?;
        let owner_digest = identity_digest(owner_chain_root_id)?;
        let Some(root) = self.open_child_directory(EVIDENCE_OBLIGATIONS_DIRECTORY)? else {
            return Ok(BTreeSet::new());
        };
        let Some(directory) = root.open_child_directory(std::ffi::OsStr::new(&owner_digest))?
        else {
            return Ok(BTreeSet::new());
        };
        let mut holders = BTreeSet::new();
        for name in directory.entry_names()? {
            ensure_state_name(&name)?;
            let text = name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 evidence obligation name"))?;
            let file = directory
                .open_regular(&name, false)?
                .ok_or_else(|| anyhow::anyhow!("evidence obligation disappeared: {text}"))?;
            let record: ObligationRecord =
                serde_json::from_slice(&read_record_bytes(file, "evidence obligation")?)
                    .context("decode evidence obligation")?;
            anyhow::ensure!(
                record.schema == FENCE_SCHEMA
                    && record.kind == OBLIGATION_KIND
                    && record.owner_chain_root_id == owner_chain_root_id
                    && format!("{}.json", identity_digest(&record.holder_chain_root_id)?) == text,
                "evidence obligation {text} does not describe owner {owner_chain_root_id:?}"
            );
            holders.insert(record.holder_chain_root_id);
        }
        Ok(holders)
    }

    /// Publish the no-rebirth fence for a chain whose retirement manifest is
    /// already stored. Idempotent for the same manifest; a different manifest
    /// for a fenced identity refuses. The caller removes the head only after
    /// this returns, so a reader never sees a headless chain without a fence.
    pub fn publish_retired_chain_fence(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        owner_lock: &crate::chain::ChainLock,
        chain_root_id: &str,
        manifest_hash: &str,
    ) -> Result<()> {
        self.ensure_guard(cas_mutation_guard)?;
        self.ensure_chain_lock(owner_lock, chain_root_id)?;
        let digest = identity_digest(chain_root_id)?;
        anyhow::ensure!(
            manifest_hash.len() == 64 && manifest_hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "retirement manifest hash is not a sha256 digest"
        );
        match self.retired_chain_fence(chain_root_id)? {
            RetiredChainFence::Absent => {}
            RetiredChainFence::Retired {
                manifest_hash: held,
            }
            | RetiredChainFence::Released {
                manifest_hash: held,
            } => {
                anyhow::ensure!(
                    held == manifest_hash,
                    "chain {chain_root_id:?} is already fenced with a different closure"
                );
                return Ok(());
            }
        }
        let record = FenceRecord {
            schema: FENCE_SCHEMA,
            kind: FENCE_KIND.to_string(),
            chain_root_id: chain_root_id.to_string(),
            manifest_hash: manifest_hash.to_string(),
        };
        let directory = self.open_or_create_child_directory(RETIRED_CHAINS_DIRECTORY)?;
        let name = fence_name(&digest);
        if directory
            .atomic_create_regular(
                std::ffi::OsStr::new(&name),
                &encode(&record)?,
                0o600,
                crate::STATE_TEMP_PURPOSE,
            )?
            .is_none()
        {
            anyhow::ensure!(
                self.read_fence_record(chain_root_id)?.as_ref() == Some(&record),
                "retired chain fence appeared with different evidence"
            );
        }
        directory.sync().context("sync retired chain fence")
    }

    /// A chain is fenced when it has a fence record, no head and no pending
    /// transition.
    fn chain_is_fenced(&self, chain_root_id: &str) -> Result<bool> {
        if self.read_fence_record(chain_root_id)?.is_none() {
            return Ok(false);
        }
        if self.chain_head_present(chain_root_id)? {
            return Ok(false);
        }
        Ok(self.read_pending(chain_root_id)?.is_none())
    }

    /// Record that `holder` depends on `owner`'s retained prefix, then run
    /// `append` while the obligation is durable and both chain locks are held.
    /// The obligation is visible before the holder's evidence is. Refuses
    /// unless the owner is live or fenced with its closure retained: absence of
    /// a fence is never proof that the owner never prepared anything.
    pub fn publish_evidence_obligation_then<T>(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        owner_lock: &crate::chain::ChainLock,
        holder_lock: &crate::chain::ChainLock,
        owner_chain_root_id: &str,
        holder_chain_root_id: &str,
        append: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.ensure_guard(cas_mutation_guard)?;
        self.ensure_chain_lock(owner_lock, owner_chain_root_id)?;
        self.ensure_chain_lock(holder_lock, holder_chain_root_id)?;
        anyhow::ensure!(
            owner_chain_root_id != holder_chain_root_id,
            "a chain cannot hold an evidence obligation against itself"
        );
        let owner_digest = identity_digest(owner_chain_root_id)?;
        let holder_digest = identity_digest(holder_chain_root_id)?;
        match self.retired_chain_fence(owner_chain_root_id)? {
            RetiredChainFence::Released { .. } => anyhow::bail!(
                "owner chain {owner_chain_root_id:?} was retired and its closure released; \
                 its retained prefix can no longer prove absence"
            ),
            RetiredChainFence::Retired { .. } => {}
            RetiredChainFence::Absent => anyhow::ensure!(
                self.chain_head_present(owner_chain_root_id)?,
                "owner chain {owner_chain_root_id:?} has neither a head nor a retirement \
                 fence; absence of a fence is not proof the chain never existed"
            ),
        }
        let record = ObligationRecord {
            schema: FENCE_SCHEMA,
            kind: OBLIGATION_KIND.to_string(),
            owner_chain_root_id: owner_chain_root_id.to_string(),
            holder_chain_root_id: holder_chain_root_id.to_string(),
        };
        let bytes = encode(&record)?;
        let root = self.open_or_create_child_directory(EVIDENCE_OBLIGATIONS_DIRECTORY)?;
        let directory = root.open_or_create_child(std::ffi::OsStr::new(&owner_digest), 0o700)?;
        let name = fence_name(&holder_digest);
        if directory
            .atomic_create_regular(
                std::ffi::OsStr::new(&name),
                &bytes,
                0o600,
                crate::STATE_TEMP_PURPOSE,
            )?
            .is_none()
        {
            let file = directory
                .open_regular(std::ffi::OsStr::new(&name), false)?
                .ok_or_else(|| anyhow::anyhow!("evidence obligation disappeared"))?;
            anyhow::ensure!(
                read_record_bytes(file, "evidence obligation")? == bytes,
                "evidence obligation appeared with different evidence"
            );
        }
        directory.sync().context("sync evidence obligation")?;
        root.sync().context("sync evidence obligation directory")?;
        append()
    }

    /// Release a retired chain's closure. `manifest_counterparts` are the
    /// counterparts the retained manifest lists; every obligation holder is
    /// added. Release requires each of them to be fenced. After the release is
    /// durable the owner's obligations are removed; a second call removes any
    /// that a crash left behind.
    pub fn publish_retired_chain_release(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        owner_lock: &crate::chain::ChainLock,
        owner_chain_root_id: &str,
        manifest_counterparts: &BTreeSet<String>,
    ) -> Result<()> {
        self.ensure_guard(cas_mutation_guard)?;
        let digest = identity_digest(owner_chain_root_id)?;
        let mut counterparts = manifest_counterparts.clone();
        counterparts.extend(self.evidence_obligation_holders(owner_lock, owner_chain_root_id)?);
        let manifest_hash = match self.retired_chain_fence(owner_chain_root_id)? {
            RetiredChainFence::Absent => {
                anyhow::bail!("chain {owner_chain_root_id:?} has no retirement fence to release")
            }
            RetiredChainFence::Released { .. } => {
                // Release already proves every holder fenced.
                return self.remove_owner_obligations(&digest);
            }
            RetiredChainFence::Retired { manifest_hash } => manifest_hash,
        };
        // A fence beside a live head or pending transition is the interrupted
        // retirement state: the closure must stay rooted until the unlink.
        anyhow::ensure!(
            self.chain_is_fenced(owner_chain_root_id)?,
            "cannot release {owner_chain_root_id:?}: its head or pending transition still exists"
        );
        for counterpart in &counterparts {
            anyhow::ensure!(
                self.chain_is_fenced(counterpart)?,
                "cannot release {owner_chain_root_id:?}: counterpart {counterpart:?} is not fenced"
            );
        }
        let record = FenceRecord {
            schema: FENCE_SCHEMA,
            kind: RELEASE_KIND.to_string(),
            chain_root_id: owner_chain_root_id.to_string(),
            manifest_hash,
        };
        let directory = self.open_or_create_child_directory(RETIRED_CHAINS_DIRECTORY)?;
        let name = release_name(&digest);
        // An existing release was published by an earlier pass for this fence.
        let _published = directory.atomic_create_regular(
            std::ffi::OsStr::new(&name),
            &encode(&record)?,
            0o600,
            crate::STATE_TEMP_PURPOSE,
        )?;
        directory.sync().context("sync retired chain release")?;
        self.remove_owner_obligations(&digest)
    }

    fn remove_owner_obligations(&self, owner_digest: &str) -> Result<()> {
        let Some(root) = self.open_child_directory(EVIDENCE_OBLIGATIONS_DIRECTORY)? else {
            return Ok(());
        };
        let name = std::ffi::OsStr::new(owner_digest);
        let Some(directory) = root.open_child_directory(name)? else {
            return Ok(());
        };
        remove_regular_children(&directory, false)?;
        anyhow::ensure!(
            root.remove_empty_child_if_same(name, &directory)?.is_some(),
            "evidence obligation directory changed during removal"
        );
        Ok(())
    }

    /// Execution-history reset: remove every fence, release and obligation.
    /// Runs inside the reset's Active operation after every chain head and
    /// pending transition is gone, so no live holder can still need a fence.
    /// Removing absent records is a no-op, so a resumed reset repeats it.
    pub(crate) fn discard_retirement_evidence(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        dry_run: bool,
    ) -> Result<RetirementEvidenceDiscard> {
        anyhow::ensure!(
            cas_mutation_guard.is_exclusive(),
            "thread-history discard requires the exclusive CAS guard"
        );
        self.ensure_guard(cas_mutation_guard)?;
        let mut discard = RetirementEvidenceDiscard::default();
        if let Some(directory) = self.fence_directory()? {
            for name in directory.entry_names()? {
                ensure_state_name(&name)?;
                let text = name.to_string_lossy();
                anyhow::ensure!(
                    text.ends_with(".json"),
                    "unexpected retirement fence entry: {text}"
                );
                if !text.ends_with(RELEASE_SUFFIX) {
                    discard.retirement_fences += 1;
                }
            }
            remove_regular_children(&directory, dry_run)?;
            if !dry_run {
                self.directory
                    .remove_empty_child_if_same(
                        std::ffi::OsStr::new(RETIRED_CHAINS_DIRECTORY),
                        &directory,
                    )?
                    .ok_or_else(|| anyhow::anyhow!("retirement fence directory changed"))?;
            }
        }
        if let Some(root) = self.open_child_directory(EVIDENCE_OBLIGATIONS_DIRECTORY)? {
            for name in root.entry_names()? {
                let Some(lillux::PinnedDirectoryEntry::Directory(child)) =
                    root.open_entry(&name, false)?
                else {
                    anyhow::bail!(
                        "unexpected evidence obligation entry: {}",
                        name.to_string_lossy()
                    );
                };
                discard.evidence_obligations += remove_regular_children(&child, dry_run)?;
                if !dry_run {
                    root.remove_empty_child_if_same(&name, &child)?
                        .ok_or_else(|| anyhow::anyhow!("obligation directory changed"))?;
                }
            }
            if !dry_run {
                self.directory
                    .remove_empty_child_if_same(
                        std::ffi::OsStr::new(EVIDENCE_OBLIGATIONS_DIRECTORY),
                        &root,
                    )?
                    .ok_or_else(|| anyhow::anyhow!("obligation root changed"))?;
            }
        }
        if !dry_run {
            self.directory
                .sync()
                .context("sync retirement evidence reset")?;
        }
        Ok(discard)
    }

    /// Directories holding temporaries the startup sweep must cover: the fence
    /// directory and one level of the obligation tree.
    pub(crate) fn retirement_evidence_sweep_directories(
        thread_projection: &lillux::PinnedDirectory,
    ) -> Result<Vec<lillux::PinnedDirectory>> {
        let mut directories = Vec::new();
        if let Some(fences) = thread_projection
            .open_child_directory(std::ffi::OsStr::new(RETIRED_CHAINS_DIRECTORY))?
        {
            directories.push(fences);
        }
        if let Some(root) = thread_projection
            .open_child_directory(std::ffi::OsStr::new(EVIDENCE_OBLIGATIONS_DIRECTORY))?
        {
            for name in root.entry_names()? {
                // Regular names directly under the root are swept as the
                // root's own debris; readers refuse any that remain.
                if let Some(lillux::PinnedDirectoryEntry::Directory(child)) =
                    root.open_entry(&name, false)?
                {
                    directories.push(child);
                }
            }
            directories.push(root);
        }
        Ok(directories)
    }
}

/// Remove (or count) every regular child of a flat directory.
fn remove_regular_children(directory: &lillux::PinnedDirectory, dry_run: bool) -> Result<usize> {
    let mut count = 0;
    for name in directory.entry_names()? {
        let Some(lillux::PinnedDirectoryEntry::Regular(file)) =
            directory.open_entry(&name, false)?
        else {
            anyhow::bail!(
                "unexpected non-file retirement evidence entry: {}",
                name.to_string_lossy()
            );
        };
        if !dry_run {
            directory.remove_if_same(&name, &file)?;
        }
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::ChainLock;
    use crate::recovery::sweep_interrupted_state_publications;
    use std::path::{Path, PathBuf};

    const MANIFEST: &str = "ab";

    fn manifest() -> String {
        MANIFEST.repeat(32)
    }

    struct Fixture {
        temp: tempfile::TempDir,
        store: RecoveryStore,
        guard: CasMutationGuard,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        Fixture { temp, store, guard }
    }

    impl Fixture {
        fn lock(&self, id: &str) -> ChainLock {
            ChainLock::acquire(&self.temp.path().join("refs"), id).unwrap()
        }

        fn head_path(&self, id: &str) -> PathBuf {
            self.temp
                .path()
                .join("refs/generic/chains")
                .join(id)
                .join("head")
        }

        fn set_head(&self, id: &str) {
            drop(self.lock(id));
            std::fs::write(self.head_path(id), b"head").unwrap();
        }

        fn unlink_head(&self, id: &str) {
            std::fs::remove_file(self.head_path(id)).unwrap();
        }

        fn fences(&self) -> PathBuf {
            self.store.root().join(RETIRED_CHAINS_DIRECTORY)
        }

        fn obligations(&self, owner: &str) -> PathBuf {
            self.store
                .root()
                .join(EVIDENCE_OBLIGATIONS_DIRECTORY)
                .join(identity_digest(owner).unwrap())
        }

        fn fence(&self, id: &str) {
            let lock = self.lock(id);
            self.store
                .publish_retired_chain_fence(&self.guard, &lock, id, &manifest())
                .unwrap();
        }

        fn obligate(&self, owner: &str, holder: &str, appended: &mut bool) -> Result<()> {
            let (first, second) = if owner < holder {
                (owner, holder)
            } else {
                (holder, owner)
            };
            let first_lock = self.lock(first);
            let second_lock = self.lock(second);
            let (owner_lock, holder_lock) = if owner < holder {
                (&first_lock, &second_lock)
            } else {
                (&second_lock, &first_lock)
            };
            self.store.publish_evidence_obligation_then(
                &self.guard,
                owner_lock,
                holder_lock,
                owner,
                holder,
                || {
                    *appended = true;
                    Ok(())
                },
            )
        }

        fn release(&self, owner: &str, manifest_counterparts: &[&str]) -> Result<()> {
            let lock = self.lock(owner);
            let counterparts = manifest_counterparts
                .iter()
                .map(|c| c.to_string())
                .collect();
            self.store
                .publish_retired_chain_release(&self.guard, &lock, owner, &counterparts)
        }

        fn holders(&self, owner: &str) -> BTreeSet<String> {
            let lock = self.lock(owner);
            self.store
                .evidence_obligation_holders(&lock, owner)
                .unwrap()
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn fence_lifecycle_is_idempotent_and_released_once() {
        let f = fixture();
        assert_eq!(
            f.store.retired_chain_fence("X").unwrap(),
            RetiredChainFence::Absent
        );
        f.set_head("X");
        f.unlink_head("X");
        f.fence("X");
        f.fence("X");
        assert_eq!(
            f.store.retired_chain_fence("X").unwrap(),
            RetiredChainFence::Retired {
                manifest_hash: manifest()
            }
        );
        assert_eq!(
            f.store.unreleased_retirement_manifest_roots().unwrap(),
            BTreeSet::from([manifest()])
        );
        let lock = f.lock("X");
        let other = "cd".repeat(32);
        assert!(
            f.store
                .publish_retired_chain_fence(&f.guard, &lock, "X", &other)
                .unwrap_err()
                .to_string()
                .contains("different closure")
        );
        drop(lock);
        f.release("X", &[]).unwrap();
        f.release("X", &[]).unwrap();
        assert_eq!(
            f.store.retired_chain_fence("X").unwrap(),
            RetiredChainFence::Released {
                manifest_hash: manifest()
            }
        );
        assert!(
            f.store
                .unreleased_retirement_manifest_roots()
                .unwrap()
                .is_empty()
        );
        assert!(f.release("Y", &[]).is_err(), "no fence, nothing to release");
    }

    #[test]
    fn absence_evidence_obligation_is_durable_before_the_append_runs() {
        let f = fixture();
        f.set_head("D");
        f.set_head("X");
        f.set_head("E");
        let observed = std::cell::Cell::new(false);
        {
            let lock_x = f.lock("X");
            let lock_d = f.lock("D");
            f.store
                .publish_evidence_obligation_then(&f.guard, &lock_x, &lock_d, "X", "D", || {
                    observed.set(
                        f.obligations("X")
                            .join(format!("{}.json", identity_digest("D").unwrap()))
                            .is_file(),
                    );
                    Ok(())
                })
                .unwrap();
        }
        assert!(observed.get(), "obligation must exist when the append runs");
        assert_eq!(f.holders("X"), BTreeSet::from(["D".to_string()]));

        // An append failure leaves the obligation in place (inert retention).
        {
            let lock_x = f.lock("X");
            let lock_e = f.lock("E");
            let err = f
                .store
                .publish_evidence_obligation_then::<()>(
                    &f.guard,
                    &lock_x,
                    &lock_e,
                    "X",
                    "E",
                    || anyhow::bail!("append refused"),
                )
                .unwrap_err();
            assert!(err.to_string().contains("append refused"));
        }
        assert_eq!(
            f.holders("X"),
            BTreeSet::from(["D".to_string(), "E".to_string()])
        );
    }

    #[test]
    fn obligation_classification_refuses_unknown_and_released_owners() {
        let f = fixture();
        f.set_head("D");
        let mut appended = false;
        // No fence and no head: absence is not proof.
        let err = f.obligate("X", "D", &mut appended).unwrap_err();
        assert!(err.to_string().contains("absence of a fence"), "{err:#}");
        assert!(!appended);
        assert!(!f.obligations("X").exists());

        // A fenced owner whose closure is still retained is allowed.
        f.set_head("X");
        f.unlink_head("X");
        f.fence("X");
        f.obligate("X", "D", &mut appended).unwrap();
        assert!(appended);

        // Once D is fenced the owner releases and a late inbound is refused.
        f.unlink_head("D");
        f.fence("D");
        f.release("X", &[]).unwrap();
        let mut late = false;
        f.set_head("Z");
        let err = f.obligate("X", "Z", &mut late).unwrap_err();
        assert!(err.to_string().contains("closure released"), "{err:#}");
        assert!(!late);
    }

    #[test]
    fn release_waits_for_every_obligation_holder_and_removes_them() {
        let f = fixture();
        f.set_head("X");
        f.set_head("D");
        let mut appended = false;
        f.obligate("X", "D", &mut appended).unwrap();
        f.unlink_head("X");
        f.fence("X");

        // I3: the obligation is inert extra retention while D is unfenced.
        let err = f.release("X", &[]).unwrap_err();
        assert!(err.to_string().contains("not fenced"), "{err:#}");
        assert!(matches!(
            f.store.retired_chain_fence("X").unwrap(),
            RetiredChainFence::Retired { .. }
        ));

        // A head-less holder without a fence still does not count as fenced.
        f.unlink_head("D");
        assert!(f.release("X", &[]).is_err());
        f.fence("D");
        f.release("X", &[]).unwrap();
        assert!(!f.obligations("X").exists());
    }

    #[test]
    fn manifest_counterparts_must_be_fenced_too() {
        let f = fixture();
        f.set_head("X");
        f.unlink_head("X");
        f.fence("X");
        f.set_head("M");
        assert!(f.release("X", &["M"]).is_err());
        f.unlink_head("M");
        f.fence("M");
        f.release("X", &["M"]).unwrap();
    }

    #[test]
    fn crash_after_release_before_obligation_removal_is_finished_by_the_next_pass() {
        let f = fixture();
        f.set_head("X");
        f.set_head("D");
        let mut appended = false;
        f.obligate("X", "D", &mut appended).unwrap();
        f.unlink_head("X");
        f.fence("X");
        f.unlink_head("D");
        f.fence("D");
        // R7 reached: the release is durable but the obligation directory
        // survived the crash. Rebuild that state by hand.
        let leftover = names(&f.obligations("X"));
        assert_eq!(leftover.len(), 1);
        let leftover_bytes = std::fs::read(f.obligations("X").join(&leftover[0])).unwrap();
        f.release("X", &[]).unwrap();
        let backup = f.obligations("X");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(backup.join(&leftover[0]), leftover_bytes).unwrap();
        f.release("X", &[]).unwrap();
        assert!(!f.obligations("X").exists(), "R8: next pass deletes them");
        assert!(matches!(
            f.store.retired_chain_fence("X").unwrap(),
            RetiredChainFence::Released { .. }
        ));
    }

    #[test]
    fn crash_with_fence_present_and_head_present_refuses_release_and_is_idempotent() {
        let f = fixture();
        f.set_head("X");
        f.fence("X"); // R4: fence published, head not yet unlinked
        f.fence("X");
        f.set_head("D");
        let mut appended = false;
        f.obligate("X", "D", &mut appended).unwrap();
        f.unlink_head("D");
        f.fence("D");
        // The owner's head still exists, so its manifest must stay rooted.
        let err = f.release("X", &[]).unwrap_err();
        assert!(err.to_string().contains("still exists"), "{err:#}");
        assert_eq!(
            f.store
                .unreleased_retirement_manifest_roots()
                .unwrap()
                .len(),
            1
        );
        f.unlink_head("X");
        f.release("X", &[]).unwrap();
    }

    #[test]
    fn counterpart_with_a_pending_transition_is_not_fenced() {
        let f = fixture();
        f.set_head("X");
        f.unlink_head("X");
        f.fence("X");
        f.set_head("M");
        f.unlink_head("M");
        f.fence("M");
        {
            let lock = f.lock("M");
            f.store
                .prepare_remove(&lock, "M", &"ab".repeat(32))
                .unwrap();
        }
        let err = f.release("X", &["M"]).unwrap_err();
        assert!(err.to_string().contains("not fenced"), "{err:#}");
    }

    #[test]
    fn release_without_a_fence_refuses_in_the_root_source() {
        let f = fixture();
        f.set_head("X");
        f.fence("X");
        let digest = identity_digest("Z").unwrap();
        std::fs::write(f.fences().join(release_name(&digest)), b"{}").unwrap();
        assert!(f.store.unreleased_retirement_manifest_roots().is_err());
    }

    #[test]
    fn reset_dry_run_refuses_temp_debris_instead_of_counting_it() {
        let f = fixture();
        f.set_head("X");
        f.fence("X");
        let own = format!(".secure.tmp.{}.4242.1", crate::STATE_TEMP_PURPOSE.as_str());
        std::fs::write(f.fences().join(own), b"x").unwrap();
        assert!(f.store.discard_retirement_evidence(&f.guard, true).is_err());
    }

    #[test]
    fn corrupt_or_foreign_records_refuse_rather_than_read_as_absent() {
        let f = fixture();
        f.set_head("X");
        f.fence("X");
        let fence = f
            .fences()
            .join(format!("{}.json", identity_digest("X").unwrap()));
        std::fs::write(&fence, b"{\"schema\":1}").unwrap();
        assert!(f.store.retired_chain_fence("X").is_err());
        std::fs::write(&fence, b"not json").unwrap();
        assert!(f.store.unreleased_retirement_manifest_roots().is_err());
        // A release with no fence is corruption.
        std::fs::remove_file(&fence).unwrap();
        std::fs::write(
            f.fences()
                .join(release_name(&identity_digest("X").unwrap())),
            b"{}",
        )
        .unwrap();
        assert!(f.store.retired_chain_fence("X").is_err());
    }

    #[test]
    fn startup_sweep_covers_fence_and_obligation_directories_only_for_own_debris() {
        let f = fixture();
        f.set_head("X");
        f.set_head("D");
        let mut appended = false;
        f.obligate("X", "D", &mut appended).unwrap();
        f.fence("X");
        let own = format!(".secure.tmp.{}.4242.1", crate::STATE_TEMP_PURPOSE.as_str());
        let foreign = ".secure.tmp.scheduler.4242.1";
        let owner_dir = f.obligations("X");
        let root_dir = f.store.root().join(EVIDENCE_OBLIGATIONS_DIRECTORY);
        for dir in [f.fences(), owner_dir.clone(), root_dir.clone()] {
            std::fs::write(dir.join(&own), b"x").unwrap();
            std::fs::write(dir.join(foreign), b"x").unwrap();
        }
        let parent = lillux::PinnedDirectory::open(f.store.root().parent().unwrap())
            .unwrap()
            .unwrap();
        let projection = lillux::PinnedDirectory::open(f.store.root())
            .unwrap()
            .unwrap();
        // Debris blocks reads until swept.
        assert!(f.store.unreleased_retirement_manifest_roots().is_err());
        assert_eq!(
            sweep_interrupted_state_publications(&parent, &projection).unwrap(),
            3
        );
        for dir in [f.fences(), owner_dir, root_dir] {
            assert!(!dir.join(&own).exists());
            assert!(dir.join(foreign).is_file());
            std::fs::remove_file(dir.join(foreign)).unwrap();
        }
        assert_eq!(
            f.store
                .unreleased_retirement_manifest_roots()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            f.holders("X").len(),
            1,
            "records themselves are never swept"
        );

        let malformed = f.fences().join(".secure.tmp.9.1");
        std::fs::write(&malformed, b"x").unwrap();
        let err = sweep_interrupted_state_publications(&parent, &projection).unwrap_err();
        assert!(err.to_string().contains("without an exact owner token"));
    }

    #[test]
    fn reset_removes_the_fence_namespace_and_a_pre_reset_inbound_is_refused() {
        let f = fixture();
        f.set_head("X");
        f.set_head("D");
        let mut appended = false;
        f.obligate("X", "D", &mut appended).unwrap();
        f.unlink_head("X");
        f.fence("X");
        f.unlink_head("D");
        f.fence("D");
        f.release("X", &[]).unwrap();
        f.set_head("W");
        let preview = f.store.discard_retirement_evidence(&f.guard, true).unwrap();
        assert_eq!(
            preview,
            RetirementEvidenceDiscard {
                retirement_fences: 2,
                evidence_obligations: 0,
            }
        );
        assert!(f.fences().is_dir(), "dry run removes nothing");

        // Rebuild an obligation to count it, then discard for real.
        f.set_head("Q");
        f.set_head("R");
        f.obligate("Q", "R", &mut appended).unwrap();
        let preview = f.store.discard_retirement_evidence(&f.guard, true).unwrap();
        assert_eq!(preview.evidence_obligations, 1);
        let done = f
            .store
            .discard_retirement_evidence(&f.guard, false)
            .unwrap();
        assert_eq!(done, preview);
        assert!(!f.fences().exists());
        assert!(!f.store.root().join(EVIDENCE_OBLIGATIONS_DIRECTORY).exists());
        // A resumed reset repeats the step as a no-op.
        assert_eq!(
            f.store
                .discard_retirement_evidence(&f.guard, false)
                .unwrap(),
            RetirementEvidenceDiscard::default()
        );
        // After reset a retired identity reads as unknown, so an inbound
        // naming it is refused and never orphan-cancelled.
        assert_eq!(
            f.store.retired_chain_fence("X").unwrap(),
            RetiredChainFence::Absent
        );
        let mut late = false;
        assert!(f.obligate("X", "W", &mut late).is_err());
        assert!(!late);
    }

    #[test]
    fn reset_requires_the_exclusive_guard() {
        let f = fixture();
        drop(f.guard);
        let shared = CasMutationGuard::acquire_shared(f.temp.path()).unwrap();
        assert!(
            f.store
                .discard_retirement_evidence(&shared, false)
                .unwrap_err()
                .to_string()
                .contains("exclusive")
        );
    }
}
