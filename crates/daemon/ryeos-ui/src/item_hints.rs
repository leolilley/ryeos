//! Scoped fan-out of item revisions to browser sessions.
//!
//! The substrate announces that an item of a project has a new revision, and
//! names the project by the identity the node established for it. A session
//! hears about it only when (1) the node says now that the session's key may
//! read that project, and (2) one of its attachments names that item (the
//! surface it was compiled from, a view or view set the surface names, or a
//! member of the extends chain of any of those) in a checkout the node's
//! registry holds as that project. What a session is told is `item.hint`, a
//! prompt to look again, never the item. The daemon holds no separate
//! subscription table: the set of items a session names is read from its live
//! attachments each time, so it is always the set the session's bindings were
//! compiled from, and who may read a project is asked at every delivery.
//!
//! The announcement hub may drop events when a subscriber lags. After a lag
//! the relay re-reads the revision chain heads of the items sessions name and
//! passes on whatever is newer than the newest revision an earlier catch-up
//! passed on, so a dropped announcement produces the same hint the
//! announcement would have.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use ryeos_app::item_revised::ItemRevised;
use ryeos_node_runtime::item_revision::{self, ChainItem};
use ryeos_state::project_identity::ProjectIdentity;
use serde_json::json;

use crate::browser_session::AdmittedBindingAttachment;
use crate::state::UiState;

/// The event name item hints travel under on a session bus.
pub const ITEM_HINT_EVENT: &str = "item.hint";

/// What the node says now about projects and who may read them. The fan-out
/// asks it at every delivery: it holds no answer from when a session opened.
pub trait ItemHintAuthority: Send + Sync {
    /// The identity the node's registry holds for the checkout at `checkout`,
    /// or `None` when it holds none.
    fn checkout_identity(&self, checkout: &Path) -> Option<ProjectIdentity>;

    /// Whether the session holding `key` may read the project `project`
    /// right now. `None` is a revision in a checkout that states no identity:
    /// only a key in the operator disclosure class may read it. The class is
    /// the node's one disclosure-class decision, made from the key and its
    /// grant, never a flag a session carries.
    fn may_read(&self, key: &str, project: Option<&ProjectIdentity>) -> bool;
}

#[derive(Default)]
pub struct ItemHintFanout {
    /// Newest revision a lag catch-up passed on, per checkout and item. Only
    /// the catch-up reads it; a live announcement is always passed on.
    seen: Mutex<HashMap<(String, String), u64>>,
}

/// The canonical root of the checkout an attachment is bound to, as the node
/// names checkouts. The retained directory authority is read, not the spelling
/// the launcher used.
fn attachment_checkout(attachment: &AdmittedBindingAttachment) -> Option<String> {
    let root = match &attachment.project_authority {
        Some(authority) => authority.descriptor_path().ok()?,
        None => Path::new(
            attachment
                .compiled_binding
                .binding
                .project_root
                .as_deref()?,
        )
        .to_owned(),
    };
    std::fs::canonicalize(root)
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
}

/// Whether `attachment` names `item_ref` in a checkout the node's registry
/// holds as `project` (or holds as none, when `project` is `None`).
fn names(
    authority: &dyn ItemHintAuthority,
    attachment: &AdmittedBindingAttachment,
    project: Option<&ProjectIdentity>,
    item_ref: &str,
) -> bool {
    if !attachment
        .compiled_binding
        .binding
        .named_item_refs()
        .contains(item_ref)
    {
        return false;
    }
    let Some(checkout) = attachment_checkout(attachment) else {
        return false;
    };
    authority.checkout_identity(Path::new(&checkout)).as_ref() == project
}

impl ItemHintFanout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pass one revision on to each session that names the item in a
    /// checkout of the event's project and may read that project now. Returns
    /// the sessions that were sent a hint.
    pub fn publish(
        &self,
        ui: &UiState,
        authority: &dyn ItemHintAuthority,
        event: &ItemRevised,
    ) -> Vec<String> {
        let mut told = Vec::new();
        for (session_id, attachments) in ui.browser_sessions.hint_audiences() {
            let Some(key) = ui.browser_sessions.live_session_principal(&session_id) else {
                continue;
            };
            if !authority.may_read(&key, event.project.as_ref()) {
                continue;
            }
            if attachments.iter().any(|attachment| {
                names(
                    authority,
                    attachment,
                    event.project.as_ref(),
                    &event.item_ref,
                )
            }) {
                ui.session_bus.publish(
                    &session_id,
                    ITEM_HINT_EVENT,
                    json!({
                        "item_ref": event.item_ref,
                        "digest": event.digest,
                        "revision": event.revision,
                        "change_set": event.change_set,
                    }),
                );
                told.push(session_id);
            }
        }
        told
    }

    /// After announcements were dropped: re-read the chain head of every item
    /// the sessions name and pass on each revision newer than the newest one a
    /// catch-up passed on before. A revision a live announcement already
    /// delivered may be passed on again; a repeat only tells the reader to
    /// look again. Returns the revisions passed on.
    pub fn catch_up(&self, ui: &UiState, authority: &dyn ItemHintAuthority) -> Vec<ItemRevised> {
        let mut named: BTreeSet<(String, String)> = BTreeSet::new();
        for (_, attachments) in ui.browser_sessions.hint_audiences() {
            for attachment in attachments {
                let Some(checkout) = attachment_checkout(&attachment) else {
                    continue;
                };
                for item_ref in attachment.compiled_binding.binding.named_item_refs() {
                    named.insert((checkout.clone(), item_ref));
                }
            }
        }
        let mut passed = Vec::new();
        for (checkout, item_ref) in named {
            let Ok(chain) = ChainItem::parse(&item_ref) else {
                continue;
            };
            let Ok(Some(tip)) = item_revision::tip_summary(Path::new(&checkout), &chain) else {
                continue;
            };
            let key = (checkout.clone(), item_ref.clone());
            {
                let mut seen = self.seen.lock().unwrap_or_else(|error| error.into_inner());
                let entry = seen.entry(key).or_insert(0);
                if tip.seq <= *entry {
                    continue;
                }
                *entry = tip.seq;
            }
            let event = ItemRevised {
                project: authority.checkout_identity(Path::new(&checkout)),
                item_ref,
                digest: tip.after_digest,
                revision: tip.seq,
                change_set: None,
            };
            self.publish(ui, authority, &event);
            passed.push(event);
        }
        passed
    }
}

/// Run [`ItemHintFanout::catch_up`] off the async executor, which reads files.
pub async fn catch_up_blocking(
    fanout: Arc<ItemHintFanout>,
    ui: Arc<UiState>,
    authority: Arc<dyn ItemHintAuthority>,
) -> usize {
    tokio::task::spawn_blocking(move || fanout.catch_up(&ui, authority.as_ref()).len())
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(error = %error, "item hint catch-up task failed");
            0
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ryeos_api::surface_views::{EffectiveUiItemIdentity, EmbeddedViewIdentity};
    use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};
    use ryeos_node_runtime::item_revision::{ActorRole, Change, append};
    use tokio::sync::broadcast::error::TryRecvError;

    use super::*;
    use crate::browser_session::tests::test_context;

    fn identity(item_ref: &str, chain: &[&str]) -> EffectiveUiItemIdentity {
        EffectiveUiItemIdentity {
            canonical_ref: item_ref.to_owned(),
            chain: chain.iter().map(|member| (*member).to_owned()).collect(),
            effective_definition_digest: EffectiveDefinitionDigest::parse("44".repeat(32))
                .expect("fixture digest"),
            effective_trust_class: TrustClass::TrustedProject,
        }
    }

    fn project() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ai")).unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let key = canonical.to_str().unwrap().to_owned();
        (dir, key)
    }

    /// A live session bound to `project` whose surface names `views`, one of
    /// which extends `chain`.
    fn session_naming(
        ui: &UiState,
        principal: &str,
        project: Option<&str>,
        views: &[&str],
        chain: &[&str],
    ) -> (
        String,
        tokio::sync::broadcast::Receiver<ryeos_app::stream_envelope::RouteStreamEnvelope>,
    ) {
        let mut context = test_context();
        let binding = &mut Arc::make_mut(&mut context.compiled_binding).binding;
        binding.project_root = project.map(str::to_owned);
        binding.principal_id = principal.to_owned();
        for view in views {
            binding.views.insert(
                (*view).to_owned(),
                EmbeddedViewIdentity::Resolved {
                    identity: identity(view, chain),
                },
            );
        }
        context.project_authority = project.map(|root| {
            Arc::new(
                lillux::PinnedDirectory::open(Path::new(root))
                    .unwrap()
                    .unwrap(),
            )
        });
        let (session_id, token, _) = ui
            .browser_sessions
            .mint_token(context, 4, &"22".repeat(32))
            .unwrap();
        ui.browser_sessions.consume_launch_token(&token).unwrap();
        let rx = ui.session_bus.subscribe(&session_id);
        (session_id, rx)
    }

    const ALICE_KEY: &str = "fp:aaaa";
    const BOB_KEY: &str = "fp:bbbb";
    const OPERATOR_KEY: &str = "fp:0000";

    fn identity_of(owner_digit: char, name: &str) -> ProjectIdentity {
        format!("pid:v1:{}/{name}", owner_digit.to_string().repeat(64))
            .parse()
            .unwrap()
    }

    /// What the node says: which identity each checkout is registered as, and
    /// which key may read which owner's projects. The operator key reads all.
    #[derive(Default)]
    struct FakeAuthority {
        checkouts: Mutex<HashMap<String, ProjectIdentity>>,
        owners: Mutex<HashMap<String, ProjectIdentity>>,
        revoked: Mutex<BTreeSet<String>>,
    }

    impl FakeAuthority {
        fn register(&self, checkout: &str, identity: &ProjectIdentity) {
            self.checkouts
                .lock()
                .unwrap()
                .insert(checkout.to_owned(), identity.clone());
        }

        fn key_owns(&self, key: &str, identity: &ProjectIdentity) {
            self.owners
                .lock()
                .unwrap()
                .insert(key.to_owned(), identity.clone());
        }

        fn withdraw(&self, key: &str) {
            self.revoked.lock().unwrap().insert(key.to_owned());
        }
    }

    impl ItemHintAuthority for FakeAuthority {
        fn checkout_identity(&self, checkout: &Path) -> Option<ProjectIdentity> {
            self.checkouts
                .lock()
                .unwrap()
                .get(checkout.to_str()?)
                .cloned()
        }

        fn may_read(&self, key: &str, project: Option<&ProjectIdentity>) -> bool {
            if self.revoked.lock().unwrap().contains(key) {
                return false;
            }
            if key == OPERATOR_KEY {
                return true;
            }
            match project {
                None => false,
                Some(project) => self
                    .owners
                    .lock()
                    .unwrap()
                    .get(key)
                    .is_some_and(|owned| owned.owner() == project.owner()),
            }
        }
    }

    fn revised(project: Option<&ProjectIdentity>, item_ref: &str, revision: u64) -> ItemRevised {
        ItemRevised {
            project: project.cloned(),
            item_ref: item_ref.to_owned(),
            digest: Some("ab".repeat(32)),
            revision,
            change_set: Some("set-1".to_owned()),
        }
    }

    fn drain(
        rx: &mut tokio::sync::broadcast::Receiver<ryeos_app::stream_envelope::RouteStreamEnvelope>,
    ) -> Vec<(String, serde_json::Value)> {
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(envelope) => out.push((envelope.event_type, envelope.payload)),
                Err(TryRecvError::Empty) => return out,
                Err(other) => panic!("session bus: {other:?}"),
            }
        }
    }

    #[test]
    fn a_revision_hints_exactly_the_sessions_that_name_the_item() {
        let ui = UiState::new();
        let authority = FakeAuthority::default();
        let alice = identity_of('a', "acme");
        let other = identity_of('a', "other");
        let (_dir, project_key) = project();
        let (_other_dir, other_key) = project();
        authority.register(&project_key, &alice);
        authority.register(&other_key, &other);
        authority.key_owns(ALICE_KEY, &alice);
        let fanout = ItemHintFanout::new();
        let (naming, mut naming_rx) =
            session_naming(&ui, ALICE_KEY, Some(&project_key), &["view:acme/a"], &[]);
        let (unrelated, mut unrelated_rx) = session_naming(
            &ui,
            ALICE_KEY,
            Some(&project_key),
            &["view:acme/other"],
            &[],
        );
        let (elsewhere, mut elsewhere_rx) =
            session_naming(&ui, ALICE_KEY, Some(&other_key), &["view:acme/a"], &[]);

        let told = fanout.publish(&ui, &authority, &revised(Some(&alice), "view:acme/a", 4));

        assert_eq!(told, vec![naming]);
        let received = drain(&mut naming_rx);
        assert_eq!(received.len(), 1, "{received:?}");
        assert_eq!(received[0].0, ITEM_HINT_EVENT);
        assert_eq!(
            received[0].1,
            json!({
                "item_ref": "view:acme/a",
                "digest": "ab".repeat(32),
                "revision": 4,
                "change_set": "set-1",
            })
        );
        assert!(drain(&mut unrelated_rx).is_empty(), "{unrelated}");
        assert!(drain(&mut elsewhere_rx).is_empty(), "{elsewhere}");
    }

    #[test]
    fn one_principals_revision_reaches_that_principal_and_not_another() {
        let ui = UiState::new();
        let authority = FakeAuthority::default();
        let alice = identity_of('a', "acme");
        let bob = identity_of('b', "acme");
        let (_alice_dir, alice_checkout) = project();
        let (_bob_dir, bob_checkout) = project();
        authority.register(&alice_checkout, &alice);
        authority.register(&bob_checkout, &bob);
        authority.key_owns(ALICE_KEY, &alice);
        authority.key_owns(BOB_KEY, &bob);
        let (alice_session, mut alice_rx) =
            session_naming(&ui, ALICE_KEY, Some(&alice_checkout), &["view:acme/a"], &[]);
        let (bob_session, mut bob_rx) =
            session_naming(&ui, BOB_KEY, Some(&bob_checkout), &["view:acme/a"], &[]);

        let told = ItemHintFanout::new().publish(
            &ui,
            &authority,
            &revised(Some(&alice), "view:acme/a", 1),
        );

        assert_eq!(told, vec![alice_session]);
        assert_eq!(drain(&mut alice_rx).len(), 1);
        assert!(drain(&mut bob_rx).is_empty(), "{bob_session}");
    }

    #[test]
    fn a_withdrawn_read_stops_the_hints_with_the_session_still_open() {
        let ui = UiState::new();
        let authority = FakeAuthority::default();
        let bob = identity_of('b', "acme");
        let (_dir, checkout) = project();
        authority.register(&checkout, &bob);
        authority.key_owns(BOB_KEY, &bob);
        let (session, mut rx) =
            session_naming(&ui, BOB_KEY, Some(&checkout), &["view:acme/a"], &[]);
        let fanout = ItemHintFanout::new();
        assert_eq!(
            fanout.publish(&ui, &authority, &revised(Some(&bob), "view:acme/a", 1)),
            vec![session.clone()]
        );
        assert_eq!(drain(&mut rx).len(), 1);

        authority.withdraw(BOB_KEY);

        assert!(
            fanout
                .publish(&ui, &authority, &revised(Some(&bob), "view:acme/a", 2))
                .is_empty()
        );
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn two_checkouts_of_one_identity_each_get_their_hint() {
        let ui = UiState::new();
        let authority = FakeAuthority::default();
        let alice = identity_of('a', "acme");
        let (_first_dir, first_checkout) = project();
        let (_second_dir, second_checkout) = project();
        authority.register(&first_checkout, &alice);
        authority.register(&second_checkout, &alice);
        authority.key_owns(ALICE_KEY, &alice);
        let (first, mut first_rx) =
            session_naming(&ui, ALICE_KEY, Some(&first_checkout), &["view:acme/a"], &[]);
        let (second, mut second_rx) = session_naming(
            &ui,
            ALICE_KEY,
            Some(&second_checkout),
            &["view:acme/a"],
            &[],
        );
        let fanout = ItemHintFanout::new();
        // Revision numbers of the two checkouts' chains differ; neither
        // suppresses the other.
        let mut told = fanout.publish(&ui, &authority, &revised(Some(&alice), "view:acme/a", 7));
        told.extend(fanout.publish(&ui, &authority, &revised(Some(&alice), "view:acme/a", 2)));
        told.sort();
        let mut expected = vec![first.clone(), first, second.clone(), second];
        expected.sort();
        assert_eq!(told, expected);
        assert_eq!(drain(&mut first_rx).len(), 2);
        assert_eq!(drain(&mut second_rx).len(), 2);
    }

    #[test]
    fn a_member_of_an_extends_chain_is_named_by_the_session_that_shows_what_extends_it() {
        let ui = UiState::new();
        let authority = FakeAuthority::default();
        let alice = identity_of('a', "acme");
        let (_dir, project_key) = project();
        authority.register(&project_key, &alice);
        authority.key_owns(ALICE_KEY, &alice);
        let fanout = ItemHintFanout::new();
        let (extending, mut rx) = session_naming(
            &ui,
            ALICE_KEY,
            Some(&project_key),
            &["view:acme/child"],
            &["view:acme/base"],
        );
        let told = fanout.publish(&ui, &authority, &revised(Some(&alice), "view:acme/base", 2));
        assert_eq!(told, vec![extending]);
        assert_eq!(drain(&mut rx).len(), 1);
    }

    #[test]
    fn a_view_that_failed_to_resolve_is_still_named() {
        let ui = UiState::new();
        let authority = FakeAuthority::default();
        let alice = identity_of('a', "acme");
        let (_dir, project_key) = project();
        authority.register(&project_key, &alice);
        authority.key_owns(ALICE_KEY, &alice);
        let mut context = test_context();
        let binding = &mut Arc::make_mut(&mut context.compiled_binding).binding;
        binding.project_root = Some(project_key.clone());
        binding.principal_id = ALICE_KEY.to_owned();
        binding.views.insert(
            "view:acme/missing".to_owned(),
            EmbeddedViewIdentity::Degraded {
                reason: "not found".to_owned(),
            },
        );
        context.project_authority = Some(Arc::new(
            lillux::PinnedDirectory::open(Path::new(&project_key))
                .unwrap()
                .unwrap(),
        ));
        let (session_id, token, _) = ui
            .browser_sessions
            .mint_token(context, 4, &"22".repeat(32))
            .unwrap();
        ui.browser_sessions.consume_launch_token(&token).unwrap();
        let mut rx = ui.session_bus.subscribe(&session_id);
        let told = ItemHintFanout::new().publish(
            &ui,
            &authority,
            &revised(Some(&alice), "view:acme/missing", 1),
        );
        assert_eq!(told, vec![session_id]);
        assert_eq!(drain(&mut rx).len(), 1);
    }

    #[test]
    fn an_announcement_with_no_identity_reaches_only_the_operator_and_never_another_principal() {
        let ui = UiState::new();
        let authority = FakeAuthority::default();
        let alice = identity_of('a', "acme");
        authority.key_owns(ALICE_KEY, &alice);
        let (_dir, unregistered) = project();
        let (operator, mut operator_rx) = session_naming(
            &ui,
            OPERATOR_KEY,
            Some(&unregistered),
            &["view:acme/a"],
            &[],
        );
        let (_alice_session, mut alice_rx) =
            session_naming(&ui, ALICE_KEY, Some(&unregistered), &["view:acme/a"], &[]);
        let told = ItemHintFanout::new().publish(&ui, &authority, &revised(None, "view:acme/a", 1));
        assert_eq!(told, vec![operator]);
        assert_eq!(drain(&mut operator_rx).len(), 1);
        assert!(drain(&mut alice_rx).is_empty());
    }

    fn record_revisions(project_key: &str, item_ref: &str, count: u64) {
        let signer = lillux::crypto::SigningKey::from_bytes(&[7; 32]);
        let item = ChainItem::parse(item_ref).unwrap();
        for index in 1..=count {
            let content = format!("version: {index}\n");
            append(
                Path::new(project_key),
                &signer,
                ActorRole::Authoring,
                Change {
                    item: &item,
                    file_ext: ".yaml",
                    before: None,
                    after: Some(&content),
                    provenance: BTreeMap::new(),
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn after_a_lag_the_chain_head_gives_the_hint_the_dropped_announcement_would_have() {
        let ui = UiState::new();
        let authority = FakeAuthority::default();
        let alice = identity_of('a', "acme");
        let (_dir, project_key) = project();
        authority.register(&project_key, &alice);
        authority.key_owns(ALICE_KEY, &alice);
        record_revisions(&project_key, "view:acme/a", 3);
        let (session, mut rx) =
            session_naming(&ui, ALICE_KEY, Some(&project_key), &["view:acme/a"], &[]);
        let fanout = ItemHintFanout::new();

        let passed = fanout.catch_up(&ui, &authority);

        assert_eq!(passed.len(), 1);
        assert_eq!(passed[0].revision, 3);
        assert_eq!(passed[0].project.as_ref(), Some(&alice));
        let received = drain(&mut rx);
        assert_eq!(received.len(), 1, "{session}");
        let tip = item_revision::tip_summary(
            Path::new(&project_key),
            &ChainItem::parse("view:acme/a").unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            received[0].1,
            json!({
                "item_ref": "view:acme/a",
                "digest": tip.after_digest,
                "revision": 3,
                "change_set": null,
            })
        );
        // Nothing is newer now, so a second catch-up passes nothing on.
        assert!(fanout.catch_up(&ui, &authority).is_empty());
        assert!(drain(&mut rx).is_empty());
    }
}
