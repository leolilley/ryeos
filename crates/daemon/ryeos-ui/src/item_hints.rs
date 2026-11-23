//! Scoped fan-out of item revisions to browser sessions.
//!
//! The substrate announces that an item of a project has a new revision. A
//! session hears about it only when one of its attachments names that item: the
//! surface it was compiled from, a view or view set the surface names, or a
//! member of the extends chain of any of those, in the same project. What a
//! session is told is `item.hint`, a prompt to look again, never the item. The
//! daemon holds no separate subscription table: the set of items a session
//! names is read from its live attachments each time, so it is always the set
//! the session's bindings were compiled from.
//!
//! The announcement hub may drop events when a subscriber lags. The relay
//! remembers the newest revision it has passed on for each item, and after a
//! lag it re-reads the revision chain heads of the items sessions name and
//! passes on whatever is newer, so a dropped announcement produces the same
//! hint the announcement would have.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use ryeos_app::item_revised::ItemRevised;
use ryeos_node_runtime::item_revision::{self, ChainItem};
use serde_json::json;

use crate::browser_session::AdmittedBindingAttachment;
use crate::state::UiState;

/// The event name item hints travel under on a session bus.
pub const ITEM_HINT_EVENT: &str = "item.hint";

#[derive(Default)]
pub struct ItemHintFanout {
    /// Newest revision passed on, per project and item.
    seen: Mutex<HashMap<(Option<String>, String), u64>>,
}

/// The canonical root of the project an attachment is bound to, as the node
/// names projects. The retained directory authority is read, not the spelling
/// the launcher used.
fn attachment_project(attachment: &AdmittedBindingAttachment) -> Option<String> {
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

fn names(attachment: &AdmittedBindingAttachment, project: &Option<String>, item_ref: &str) -> bool {
    if !attachment
        .compiled_binding
        .binding
        .named_item_refs()
        .contains(item_ref)
    {
        return false;
    }
    match project {
        Some(project) => attachment_project(attachment).as_deref() == Some(project.as_str()),
        None => true,
    }
}

impl ItemHintFanout {
    pub fn new() -> Self {
        Self::default()
    }

    fn remember(&self, event: &ItemRevised) {
        let mut seen = self.seen.lock().unwrap_or_else(|error| error.into_inner());
        let entry = seen
            .entry((event.project.clone(), event.item_ref.clone()))
            .or_insert(0);
        *entry = (*entry).max(event.revision);
    }

    fn seen_revision(&self, project: &Option<String>, item_ref: &str) -> u64 {
        self.seen
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(project.clone(), item_ref.to_owned()))
            .copied()
            .unwrap_or(0)
    }

    /// Pass one revision on to each session that names the item. Returns the
    /// sessions that were sent a hint.
    pub fn publish(&self, ui: &UiState, event: &ItemRevised) -> Vec<String> {
        self.remember(event);
        let mut told = Vec::new();
        for (session_id, attachments) in ui.browser_sessions.hint_audiences() {
            if attachments
                .iter()
                .any(|attachment| names(attachment, &event.project, &event.item_ref))
            {
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
    /// the sessions name and pass on each revision newer than the newest one
    /// seen. Returns the revisions passed on.
    pub fn catch_up(&self, ui: &UiState) -> Vec<ItemRevised> {
        let mut named: BTreeSet<(String, String)> = BTreeSet::new();
        for (_, attachments) in ui.browser_sessions.hint_audiences() {
            for attachment in attachments {
                let Some(project) = attachment_project(&attachment) else {
                    continue;
                };
                for item_ref in attachment.compiled_binding.binding.named_item_refs() {
                    named.insert((project.clone(), item_ref));
                }
            }
        }
        let mut passed = Vec::new();
        for (project, item_ref) in named {
            let Ok(chain) = ChainItem::parse(&item_ref) else {
                continue;
            };
            let Ok(Some(tip)) = item_revision::tip_summary(Path::new(&project), &chain) else {
                continue;
            };
            let project = Some(project);
            if tip.seq <= self.seen_revision(&project, &item_ref) {
                continue;
            }
            let event = ItemRevised {
                project,
                item_ref,
                digest: tip.after_digest,
                revision: tip.seq,
                change_set: None,
            };
            self.publish(ui, &event);
            passed.push(event);
        }
        passed
    }
}

/// Run [`ItemHintFanout::catch_up`] off the async executor, which reads files.
pub async fn catch_up_blocking(fanout: Arc<ItemHintFanout>, ui: Arc<UiState>) -> usize {
    tokio::task::spawn_blocking(move || fanout.catch_up(&ui).len())
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

    fn revised(project: Option<&str>, item_ref: &str, revision: u64) -> ItemRevised {
        ItemRevised {
            project: project.map(str::to_owned),
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
        let (_dir, project_key) = project();
        let (_other_dir, other_key) = project();
        let fanout = ItemHintFanout::new();
        let (naming, mut naming_rx) =
            session_naming(&ui, Some(&project_key), &["view:acme/a"], &[]);
        let (unrelated, mut unrelated_rx) =
            session_naming(&ui, Some(&project_key), &["view:acme/other"], &[]);
        let (elsewhere, mut elsewhere_rx) =
            session_naming(&ui, Some(&other_key), &["view:acme/a"], &[]);

        let told = fanout.publish(&ui, &revised(Some(&project_key), "view:acme/a", 4));

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
    fn a_member_of_an_extends_chain_is_named_by_the_session_that_shows_what_extends_it() {
        let ui = UiState::new();
        let (_dir, project_key) = project();
        let fanout = ItemHintFanout::new();
        let (extending, mut rx) = session_naming(
            &ui,
            Some(&project_key),
            &["view:acme/child"],
            &["view:acme/base"],
        );
        let told = fanout.publish(&ui, &revised(Some(&project_key), "view:acme/base", 2));
        assert_eq!(told, vec![extending]);
        assert_eq!(drain(&mut rx).len(), 1);
    }

    #[test]
    fn a_view_that_failed_to_resolve_is_still_named() {
        let ui = UiState::new();
        let (_dir, project_key) = project();
        let mut context = test_context();
        let binding = &mut Arc::make_mut(&mut context.compiled_binding).binding;
        binding.project_root = Some(project_key.clone());
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
        let told = ItemHintFanout::new()
            .publish(&ui, &revised(Some(&project_key), "view:acme/missing", 1));
        assert_eq!(told, vec![session_id]);
        assert_eq!(drain(&mut rx).len(), 1);
    }

    #[test]
    fn an_announcement_with_no_project_reaches_every_session_naming_the_item() {
        let ui = UiState::new();
        let (_dir, project_key) = project();
        let (first, _first_rx) = session_naming(&ui, Some(&project_key), &["view:acme/a"], &[]);
        let (second, _second_rx) = session_naming(&ui, None, &["view:acme/a"], &[]);
        let mut told = ItemHintFanout::new().publish(&ui, &revised(None, "view:acme/a", 1));
        told.sort();
        let mut expected = vec![first, second];
        expected.sort();
        assert_eq!(told, expected);
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
        let (_dir, project_key) = project();
        record_revisions(&project_key, "view:acme/a", 3);
        let (session, mut rx) = session_naming(&ui, Some(&project_key), &["view:acme/a"], &[]);
        let fanout = ItemHintFanout::new();
        // The relay saw revision 1, then announcements 2 and 3 were dropped.
        fanout.publish(&ui, &revised(Some(&project_key), "view:acme/a", 1));
        assert_eq!(drain(&mut rx).len(), 1);

        let passed = fanout.catch_up(&ui);

        assert_eq!(passed.len(), 1);
        assert_eq!(passed[0].revision, 3);
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
        assert!(fanout.catch_up(&ui).is_empty());
        assert!(drain(&mut rx).is_empty());
    }
}
