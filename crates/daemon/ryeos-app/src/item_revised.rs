//! The `item_revised` event: one project item has a new revision.
//!
//! The durable form of the event is the item's revision record, written
//! before anything is announced. The live form is a broadcast hub beside the
//! thread event hub, with the same capacity and the same lag rule: a
//! subscriber that falls behind loses broadcasts, and recovers by re-reading
//! the revision chain heads (see [`reconcile_published_heads`] and the
//! subscribers' own catch-up). Loss is harmless because an announcement only
//! tells a reader to look again.
//!
//! The event names no consumer and no kind of item. `project` is the
//! identity the node established for the project the item lives in (the
//! owner of the registration that admitted the checkout, and the name the
//! checkout declares); it is absent when the checkout states no identity.
//! The event never carries a checkout path. The cursor below is keyed by the
//! checkout, as private storage, because one project can have several
//! checkouts whose revision chains number separately.
//!
//! The node keeps, per project, the newest revision it has announced for each
//! item (a cursor, not authority). Startup compares every chain head with it
//! and announces what is newer, so a change made while the node was stopped,
//! or whose announcement was lost, is still announced once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use anyhow::{Context, Result};
use ryeos_node_runtime::item_revision::{self, ChainItem, RevisionHeads};
use ryeos_state::project_identity::ProjectIdentity;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::event_stream::DEFAULT_EVENT_STREAM_CAPACITY;

/// Capacity of the live broadcast channel: the thread hub's capacity, so the
/// two hubs lag under the same load.
pub const ITEM_REVISED_CAPACITY: usize = DEFAULT_EVENT_STREAM_CAPACITY;

/// Schema string of the announcement cursor document.
const CURSOR_SCHEMA: &str = "ryeos.item_revised_cursor.v1";
/// Largest cursor document read.
const MAX_CURSOR_BYTES: u64 = 4 * 1024 * 1024;

/// One item has a new revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemRevised {
    /// The project identity the node established for the checkout the item
    /// lives in; absent when the checkout states none.
    pub project: Option<ProjectIdentity>,
    /// Canonical ref of the item.
    pub item_ref: String,
    /// Digest of the item as of this revision; absent when the item was
    /// removed.
    pub digest: Option<String>,
    /// Position of the revision in the item's chain.
    pub revision: u64,
    /// The change set the revision belongs to, when it was made as part of
    /// one.
    pub change_set: Option<String>,
}

/// A recorded change as its producer knows it: the checkout it happened in,
/// named as the node's revision heads name checkouts. The checkout is for the
/// announcement cursor only; [`announce`] turns this into an [`ItemRevised`]
/// carrying the identity the producer's admission established, and no path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisedAt {
    /// Canonical root path of the checkout (the revision heads' project key).
    pub checkout: String,
    /// Canonical ref of the item.
    pub item_ref: String,
    /// Digest of the item as of this revision; absent when removed.
    pub digest: Option<String>,
    /// Position of the revision in the item's chain.
    pub revision: u64,
    /// The change set the revision belongs to, when it was made as part of one.
    pub change_set: Option<String>,
}

/// Live broadcast of [`ItemRevised`] events.
pub struct ItemRevisedHub {
    sender: broadcast::Sender<ItemRevised>,
}

impl ItemRevisedHub {
    pub fn new(capacity: usize) -> Self {
        let (sender, _receiver) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Subscribe to events published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<ItemRevised> {
        self.sender.subscribe()
    }

    /// Broadcast an event. Best effort: with no subscriber the send drops,
    /// the revision record being the durable form.
    pub fn publish(&self, event: ItemRevised) {
        let _ = self.sender.send(event);
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorDocument {
    schema: String,
    project: String,
    items: BTreeMap<String, u64>,
}

static CURSOR_WRITE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// The newest revision the node has announced for each item, per project.
pub struct PublishedHeads {
    directory: PathBuf,
}

impl PublishedHeads {
    pub fn open(app_root: &Path) -> Self {
        Self {
            directory: ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf())
                .item_revised_cursor_dir(),
        }
    }

    fn path(&self, project: &str) -> PathBuf {
        self.directory
            .join(format!("{}.json", lillux::sha256_hex(project.as_bytes())))
    }

    /// What has been announced for `project`: item ref to revision.
    pub fn read(&self, project: &str) -> Result<BTreeMap<String, u64>> {
        let path = self.path(project);
        let Some(bytes) =
            lillux::read_optional_regular_file_bounded_no_follow(&path, MAX_CURSOR_BYTES)?
        else {
            return Ok(BTreeMap::new());
        };
        let document: CursorDocument = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse the announcement cursor {}", path.display()))?;
        anyhow::ensure!(
            document.schema == CURSOR_SCHEMA && document.project == project,
            "the announcement cursor {} is not this project's",
            path.display()
        );
        Ok(document.items)
    }

    /// Record that `revision` of `item_ref` has been announced. The cursor
    /// only moves forward.
    pub fn record(&self, project: &str, item_ref: &str, revision: u64) -> Result<()> {
        let _guard = CURSOR_WRITE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut items = self.read(project).unwrap_or_default();
        if items
            .get(item_ref)
            .is_some_and(|current| *current >= revision)
        {
            return Ok(());
        }
        items.insert(item_ref.to_owned(), revision);
        let document = CursorDocument {
            schema: CURSOR_SCHEMA.to_owned(),
            project: project.to_owned(),
            items,
        };
        let bytes = serde_json::to_vec(&document).context("render the announcement cursor")?;
        std::fs::create_dir_all(&self.directory)
            .with_context(|| format!("create {}", self.directory.display()))?;
        lillux::atomic_write_private(&self.path(project), &bytes)
            .map_err(anyhow::Error::from)
            .context("write the announcement cursor")
    }
}

/// Announce `change`: record it in the cursor (by checkout), then broadcast it
/// as an [`ItemRevised`] carrying `project`, the identity the caller's
/// admission established for the checkout, or `None` when it states none. A
/// cursor that cannot be written is logged, never raised: the change is
/// already durable and the next startup announces it again.
pub fn announce(
    hub: &ItemRevisedHub,
    cursor: &PublishedHeads,
    project: Option<ProjectIdentity>,
    change: RevisedAt,
) {
    if let Err(error) = cursor.record(&change.checkout, &change.item_ref, change.revision) {
        tracing::warn!(
            checkout = %change.checkout,
            item_ref = %change.item_ref,
            error = %format!("{error:#}"),
            "could not record an announced revision; startup will announce it again"
        );
    }
    hub.publish(ItemRevised {
        project,
        item_ref: change.item_ref,
        digest: change.digest,
        revision: change.revision,
        change_set: change.change_set,
    });
}

/// What one reconciliation pass found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub projects: usize,
    pub announced: usize,
    pub failures: usize,
}

/// Announce the revisions that are newer than the cursor says, for each of
/// `checkouts`. `identity_of` resolves a checkout to the identity the node's
/// registry holds for it, or `None` when the registry holds none (or more than
/// one owner claims the same path).
pub fn reconcile_projects(
    hub: &ItemRevisedHub,
    cursor: &PublishedHeads,
    checkouts: &[String],
    identity_of: &dyn Fn(&Path) -> Option<ProjectIdentity>,
) -> ReconcileReport {
    let mut report = ReconcileReport::default();
    for project in checkouts {
        report.projects += 1;
        let root = Path::new(project);
        let identity = identity_of(root);
        let announced = match cursor.read(project) {
            Ok(announced) => announced,
            Err(error) => {
                tracing::warn!(project, error = %format!("{error:#}"), "unreadable announcement cursor");
                report.failures += 1;
                continue;
            }
        };
        let chains = match item_revision::list_project_chains(root) {
            Ok(chains) => chains,
            Err(error) => {
                tracing::warn!(project, error = %format!("{error:#}"), "could not list revision chains");
                report.failures += 1;
                continue;
            }
        };
        for chain in chains {
            match chain_head_event(project, &chain) {
                Ok(Some(change)) => {
                    if announced.get(&change.item_ref).copied().unwrap_or(0) < change.revision {
                        announce(hub, cursor, identity.clone(), change);
                        report.announced += 1;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        project,
                        item_ref = %chain.item_ref(),
                        error = %format!("{error:#}"),
                        "could not read a revision chain head"
                    );
                    report.failures += 1;
                }
            }
        }
    }
    report
}

/// Announce what changed since the last run, for every project the node has
/// attested revisions for.
pub fn reconcile_published_heads(
    app_root: &Path,
    node_key: lillux::crypto::VerifyingKey,
    hub: &ItemRevisedHub,
    identity_of: &dyn Fn(&Path) -> Option<ProjectIdentity>,
) -> Result<ReconcileReport> {
    let heads = RevisionHeads::for_reader(app_root, node_key);
    let projects = heads.attested_projects()?;
    Ok(reconcile_projects(
        hub,
        &PublishedHeads::open(app_root),
        &projects,
        identity_of,
    ))
}

/// The change a chain's current head stands for, or `None` when the item has
/// no chain.
pub fn chain_head_event(checkout: &str, chain: &ChainItem) -> Result<Option<RevisedAt>> {
    Ok(
        item_revision::tip_summary(Path::new(checkout), chain)?.map(|tip| RevisedAt {
            checkout: checkout.to_owned(),
            item_ref: chain.item_ref(),
            digest: tip.after_digest,
            revision: tip.seq,
            change_set: None,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn event(_checkout: &str, item_ref: &str, revision: u64) -> ItemRevised {
        ItemRevised {
            project: None,
            item_ref: item_ref.to_owned(),
            digest: Some("d".repeat(64)),
            revision,
            change_set: None,
        }
    }

    #[test]
    fn hub_delivers_to_subscribers_and_drops_without_any() {
        let hub = ItemRevisedHub::new(4);
        hub.publish(event("/p", "view:a", 1));
        let mut rx = hub.subscribe();
        hub.publish(event("/p", "view:a", 2));
        assert_eq!(rx.try_recv().unwrap().revision, 2);
    }

    #[test]
    fn a_lagging_subscriber_is_told_it_lagged() {
        let hub = ItemRevisedHub::new(2);
        let mut rx = hub.subscribe();
        for revision in 1..=5 {
            hub.publish(event("/p", "view:a", revision));
        }
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
    }

    #[test]
    fn the_cursor_only_moves_forward() {
        let root = tempfile::tempdir().unwrap();
        let cursor = PublishedHeads::open(root.path());
        cursor.record("/p", "view:a", 3).unwrap();
        cursor.record("/p", "view:a", 2).unwrap();
        cursor.record("/p", "view:b", 1).unwrap();
        assert_eq!(
            cursor.read("/p").unwrap(),
            BTreeMap::from([("view:a".to_owned(), 3), ("view:b".to_owned(), 1)])
        );
        assert!(cursor.read("/other").unwrap().is_empty());
    }

    fn identity(owner_digit: char, name: &str) -> ProjectIdentity {
        format!("pid:v1:{}/{name}", owner_digit.to_string().repeat(64))
            .parse()
            .unwrap()
    }

    fn change(checkout: &str, item_ref: &str, revision: u64) -> RevisedAt {
        RevisedAt {
            checkout: checkout.to_owned(),
            item_ref: item_ref.to_owned(),
            digest: Some("d".repeat(64)),
            revision,
            change_set: None,
        }
    }

    #[test]
    fn an_announcement_carries_the_identity_it_is_given_and_the_cursor_keeps_the_checkout() {
        let root = tempfile::tempdir().unwrap();
        let hub = ItemRevisedHub::new(4);
        let cursor = PublishedHeads::open(root.path());
        let mut rx = hub.subscribe();
        let alice = identity('a', "acme");

        announce(
            &hub,
            &cursor,
            Some(alice.clone()),
            change("/p", "view:a", 3),
        );

        let event = rx.try_recv().unwrap();
        assert_eq!(event.project, Some(alice));
        assert_eq!(event.revision, 3);
        // The cursor is private storage keyed by the checkout.
        assert_eq!(
            cursor.read("/p").unwrap(),
            BTreeMap::from([("view:a".to_owned(), 3)])
        );
    }

    #[test]
    fn a_checkout_that_states_no_identity_announces_with_none() {
        let root = tempfile::tempdir().unwrap();
        let hub = ItemRevisedHub::new(4);
        let cursor = PublishedHeads::open(root.path());
        let mut rx = hub.subscribe();

        announce(&hub, &cursor, None, change("/p", "view:a", 1));

        assert_eq!(rx.try_recv().unwrap().project, None);
    }

    /// A checkout holding one recorded directive change, as the node's own
    /// revision chain reads it.
    fn checkout_with_one_revision() -> tempfile::TempDir {
        use ryeos_node_runtime::item_revision::{ActorRole, Change, append};
        let project = tempfile::tempdir().unwrap();
        let directory = project.path().join(".ai/directives");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("greet.yaml"), "value: 1\n").unwrap();
        let item = ChainItem::parse("directive:greet").unwrap();
        append(
            project.path(),
            &lillux::crypto::SigningKey::from_bytes(&[11; 32]),
            ActorRole::Authoring,
            Change {
                item: &item,
                file_ext: ".yaml",
                before: None,
                after: Some("value: 1\n"),
                provenance: BTreeMap::new(),
            },
        )
        .unwrap();
        project
    }

    #[test]
    fn startup_reconcile_announces_the_registry_identity_and_no_path() {
        let project = checkout_with_one_revision();
        let checkout = RevisionHeads::project_key(project.path()).unwrap();
        let state = tempfile::tempdir().unwrap();
        let hub = ItemRevisedHub::new(4);
        let cursor = PublishedHeads::open(state.path());
        let mut rx = hub.subscribe();
        let registered = identity('a', "acme");

        let report = reconcile_projects(&hub, &cursor, std::slice::from_ref(&checkout), &|_| {
            Some(registered.clone())
        });

        assert_eq!(report.announced, 1);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.project, Some(registered));
        assert_eq!(event.item_ref, "directive:greet");
        let wire = serde_json::to_string(&event).unwrap();
        assert!(
            !wire.contains(&checkout),
            "the announced event names the checkout path: {wire}"
        );
        // The cursor is keyed by the checkout, so a second run announces nothing.
        assert_eq!(cursor.read(&checkout).unwrap().len(), 1);
        let again = reconcile_projects(&hub, &cursor, &[checkout], &|_| None);
        assert_eq!(again.announced, 0);
    }

    #[test]
    fn startup_reconcile_of_an_unregistered_checkout_announces_with_none() {
        let project = checkout_with_one_revision();
        let checkout = RevisionHeads::project_key(project.path()).unwrap();
        let state = tempfile::tempdir().unwrap();
        let hub = ItemRevisedHub::new(4);
        let cursor = PublishedHeads::open(state.path());
        let mut rx = hub.subscribe();

        reconcile_projects(&hub, &cursor, &[checkout], &|_| None);

        assert_eq!(rx.try_recv().unwrap().project, None);
    }
}
