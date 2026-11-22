//! Independent presentation documents.
//!
//! The core asks for a new presentation in its own document in two ways: it
//! prepares one under the existing seat owner (`OpenNewPresentationDocument`),
//! or it already has one prepared and acknowledged
//! (`ShowPreparedPresentationDocument`). Either way the host only offers an
//! explicit link. Nothing opens automatically, no producer is claimed here,
//! and the child document authenticates its membership and claims its own
//! seat when it loads.
//!
//! Preparation sends one frozen request; retrying repeats exactly those bytes,
//! so the daemon reconciles a retry with the first attempt. Dismissing a link
//! removes only the offer: a preparation the node retained stays retained.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;

use ryeos_client_base::ui::UiBindingAttachmentCoordinate;
use serde_json::{Value, json};

use crate::admission::session_attachment_coordinate;
use crate::api::{SEAT_OPEN_PATH, SESSION_CURRENT_PATH};
use crate::codec::decode_strict;
use crate::error::RuntimeError;
use crate::inbox::WakeFn;
use crate::platform::{Platform, Transport};
use crate::seat::{control_post, encode_control, random_uuid, unwrap_result};

/// Where one offered document stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentState {
    /// The preparation request is in flight.
    Preparing,
    /// Prepared: the host offers a link to this presentation.
    Ready { presentation_id: String },
    /// The preparation was not confirmed. Retrying repeats the same request.
    Unconfirmed { reason: String },
}

/// One offered document as the host draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentOffer {
    pub id: u64,
    pub state: DocumentState,
    /// Whether a retry is possible: only a preparation this runtime sent.
    pub retryable: bool,
}

struct Preparation {
    expected_session_id: String,
    root: UiBindingAttachmentCoordinate,
    encoded: String,
}

struct Entry {
    id: u64,
    state: DocumentState,
    preparation: Option<Rc<Preparation>>,
}

/// The documents offered to the operator, shared by the effect executor and
/// the host. Every change bumps [`DocumentOffers::generation`] and wakes the
/// host.
pub struct DocumentOffers {
    entries: RefCell<Vec<Entry>>,
    next_id: Cell<u64>,
    generation: Cell<u64>,
    wake: WakeFn,
}

impl DocumentOffers {
    pub fn new(wake: WakeFn) -> Rc<Self> {
        Rc::new(Self {
            entries: RefCell::new(Vec::new()),
            next_id: Cell::new(1),
            generation: Cell::new(0),
            wake,
        })
    }

    /// The offers, oldest first.
    pub fn offers(&self) -> Vec<DocumentOffer> {
        self.entries
            .borrow()
            .iter()
            .map(|entry| DocumentOffer {
                id: entry.id,
                state: entry.state.clone(),
                retryable: entry.preparation.is_some()
                    && matches!(entry.state, DocumentState::Unconfirmed { .. }),
            })
            .collect()
    }

    /// Bumped by every change to the offers.
    pub fn generation(&self) -> u64 {
        self.generation.get()
    }

    /// Withdraw one offer. A preparation still in flight finishes unseen.
    pub fn dismiss(&self, id: u64) {
        let mut entries = self.entries.borrow_mut();
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() != before {
            drop(entries);
            self.changed();
        }
    }

    /// Offer an already prepared presentation.
    pub(crate) fn offer_prepared(&self, presentation_id: &str) -> Result<(), RuntimeError> {
        require_presentation_id(presentation_id)?;
        self.push(
            DocumentState::Ready {
                presentation_id: presentation_id.to_owned(),
            },
            None,
        );
        Ok(())
    }

    /// Prepare a new presentation for `root` and offer it. Resolves once the
    /// first attempt has settled; its failure is shown on the offer, not
    /// returned, because the operator retries it from there.
    pub(crate) async fn prepare_new<P: Platform>(
        self: &Rc<Self>,
        platform: &P,
        expected_session_id: &str,
        root: &UiBindingAttachmentCoordinate,
    ) -> Result<(), RuntimeError> {
        let encoded = encode_control(&json!({
            "action": "prepare",
            "creation_request_id": random_uuid(platform),
            "binding_attachment_id": root.binding_attachment_id,
            "binding_generation": root.binding_generation,
            "binding_digest": root.binding_digest,
        }))?;
        let preparation = Rc::new(Preparation {
            expected_session_id: expected_session_id.to_owned(),
            root: root.clone(),
            encoded,
        });
        let id = self.push(DocumentState::Preparing, Some(preparation.clone()));
        let outcome = prepare(platform, &preparation).await;
        self.settle(id, outcome);
        Ok(())
    }

    /// Repeat an unconfirmed preparation with its original bytes. Returns
    /// the work to run, which the caller keeps in a retained task: the
    /// request is idempotent on the node and its answer lands on the offer.
    pub(crate) fn retry<P: Platform + 'static>(
        self: &Rc<Self>,
        platform: &Rc<P>,
        id: u64,
    ) -> Option<impl Future<Output = ()> + 'static> {
        let preparation = {
            let mut entries = self.entries.borrow_mut();
            let entry = entries.iter_mut().find(|entry| entry.id == id)?;
            if !matches!(entry.state, DocumentState::Unconfirmed { .. }) {
                return None;
            }
            let preparation = entry.preparation.clone()?;
            entry.state = DocumentState::Preparing;
            preparation
        };
        self.changed();
        let offers = self.clone();
        let platform = platform.clone();
        Some(async move {
            let outcome = prepare(&*platform, &preparation).await;
            offers.settle(id, outcome);
        })
    }

    fn push(&self, state: DocumentState, preparation: Option<Rc<Preparation>>) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.entries.borrow_mut().push(Entry {
            id,
            state,
            preparation,
        });
        self.changed();
        id
    }

    fn settle(&self, id: u64, outcome: Result<String, RuntimeError>) {
        let mut entries = self.entries.borrow_mut();
        // A dismissed offer stays dismissed; the node keeps what it prepared.
        let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        entry.state = match outcome {
            Ok(presentation_id) => DocumentState::Ready { presentation_id },
            Err(error) => DocumentState::Unconfirmed {
                reason: error.to_string(),
            },
        };
        drop(entries);
        self.changed();
    }

    fn changed(&self) {
        self.generation.set(self.generation.get() + 1);
        (self.wake)();
    }
}

/// The page that opens `presentation_id`, relative to the UI root.
pub fn document_path(presentation_id: &str) -> Result<String, RuntimeError> {
    require_presentation_id(presentation_id)?;
    Ok(format!("/ui/?presentation_id={presentation_id}"))
}

fn require_presentation_id(presentation_id: &str) -> Result<(), RuntimeError> {
    let shape = [8, 4, 4, 4, 12];
    let groups: Vec<&str> = presentation_id.split('-').collect();
    let valid = groups.len() == shape.len()
        && groups.iter().zip(shape).all(|(group, length)| {
            group.len() == length
                && group
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    if valid {
        Ok(())
    } else {
        Err(RuntimeError::BindingRequest(
            "prepared presentation identity is invalid".to_owned(),
        ))
    }
}

/// Re-read the session (the root must still be the one the core named, in
/// the session the core belongs to), then send the frozen preparation.
async fn prepare<P: Platform>(
    platform: &P,
    preparation: &Preparation,
) -> Result<String, RuntimeError> {
    let refused = |message: &str| RuntimeError::BindingRequest(message.to_owned());
    let text = platform.transport().get(SESSION_CURRENT_PATH).await?;
    let value = decode_strict(&text)?;
    let session = value.get("result").unwrap_or(&value);
    let (session_id, current) =
        session_attachment_coordinate(session, &preparation.root.binding_attachment_id)?;
    if session_id != preparation.expected_session_id {
        return Err(refused(
            "the new presentation belongs to another authenticated session",
        ));
    }
    if current != preparation.root {
        return Err(refused("the new presentation's root attachment changed"));
    }
    let response = control_post(platform, SEAT_OPEN_PATH, &preparation.encoded).await?;
    let prepared = unwrap_result(&response).map_err(|error| refused(&error.0))?;
    if prepared.get("root_binding") != Some(&serde_json::to_value(&preparation.root)?) {
        return Err(refused("the prepared presentation's root changed"));
    }
    let text = |field: &str| {
        prepared
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| refused(&format!("the prepared presentation has no {field}")))
    };
    let presentation_id = text("presentation_id")?;
    text("thread_id")?;
    text("chain_root_id")?;
    require_presentation_id(&presentation_id)?;
    Ok(presentation_id)
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use serde_json::json;

    use super::*;
    use crate::platform::Spawner;
    use crate::sim::{FakePlatform, Reply};

    const PRESENTATION: &str = "33333333-3333-4333-8333-333333333333";

    fn root() -> UiBindingAttachmentCoordinate {
        UiBindingAttachmentCoordinate {
            binding_attachment_id: "surface".to_owned(),
            binding_generation: 1,
            binding_digest: "digest".to_owned(),
        }
    }

    fn session(session_id: &str, generation: u64) -> String {
        json!({"result": {
            "session_id": session_id,
            "root_attachment_id": "surface",
            "binding_attachments": [{
                "binding_attachment_id": "surface",
                "binding_generation": generation,
                "binding_digest": "digest",
                "project_path": null,
                "subject": {"kind": "projectless"},
            }],
        }})
        .to_string()
    }

    fn prepared() -> String {
        json!({"result": {
            "presentation_id": PRESENTATION,
            "thread_id": "T-child",
            "chain_root_id": "T-child",
            "root_binding": root(),
        }})
        .to_string()
    }

    fn offers() -> Rc<DocumentOffers> {
        DocumentOffers::new(Rc::new(|| {}))
    }

    #[test]
    fn a_prepared_document_is_offered_as_a_link_to_its_presentation() {
        let platform = FakePlatform::new();
        platform
            .transport
            .script(SESSION_CURRENT_PATH, vec![Reply::Ok(session("S-1", 1))]);
        platform
            .transport
            .script(SEAT_OPEN_PATH, vec![Reply::Ok(prepared())]);
        let documents = offers();
        platform
            .sim()
            .run(documents.prepare_new(&*platform, "S-1", &root()))
            .unwrap();
        let [offer] = documents.offers().try_into().unwrap();
        assert_eq!(
            offer.state,
            DocumentState::Ready {
                presentation_id: PRESENTATION.to_owned()
            }
        );
        assert!(!offer.retryable);
        assert_eq!(
            document_path(PRESENTATION).unwrap(),
            format!("/ui/?presentation_id={PRESENTATION}")
        );
        let [body] = platform
            .transport
            .calls_to(SEAT_OPEN_PATH)
            .try_into()
            .unwrap();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["action"], "prepare");
        assert_eq!(body["binding_attachment_id"], "surface");
    }

    #[test]
    fn a_session_or_root_that_changed_is_refused_before_anything_is_prepared() {
        for (session_id, generation, reason) in [
            ("S-other", 1, "another authenticated session"),
            ("S-1", 2, "root attachment changed"),
        ] {
            let platform = FakePlatform::new();
            platform.transport.script(
                SESSION_CURRENT_PATH,
                vec![Reply::Ok(session(session_id, generation))],
            );
            let documents = offers();
            platform
                .sim()
                .run(documents.prepare_new(&*platform, "S-1", &root()))
                .unwrap();
            let [offer] = documents.offers().try_into().unwrap();
            assert!(
                matches!(&offer.state, DocumentState::Unconfirmed { reason: shown } if shown.contains(reason)),
                "{offer:?}"
            );
            assert!(offer.retryable);
            assert!(platform.transport.calls_to(SEAT_OPEN_PATH).is_empty());
        }
    }

    #[test]
    fn a_retry_repeats_the_exact_preparation_bytes() {
        let platform = FakePlatform::new();
        platform
            .transport
            .script(SESSION_CURRENT_PATH, vec![Reply::Ok(session("S-1", 1))]);
        platform.transport.script(
            SEAT_OPEN_PATH,
            vec![Reply::Status(409), Reply::Ok(prepared())],
        );
        let documents = offers();
        let sim = platform.sim();
        sim.run(documents.prepare_new(&*platform, "S-1", &root()))
            .unwrap();
        let [offer] = documents.offers().try_into().unwrap();
        assert!(offer.retryable, "{offer:?}");
        let retry = documents.retry(&platform, offer.id).expect("retryable");
        let _task = platform.spawner().spawn(retry);
        assert_eq!(documents.offers()[0].state, DocumentState::Preparing);
        sim.run_for(1_000);
        assert!(matches!(
            documents.offers()[0].state,
            DocumentState::Ready { .. }
        ));
        let calls = platform.transport.calls_to(SEAT_OPEN_PATH);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
    }

    #[test]
    fn dismissing_withdraws_only_the_offer() {
        let documents = offers();
        documents.offer_prepared(PRESENTATION).unwrap();
        documents.offer_prepared(PRESENTATION).unwrap();
        let first = documents.offers()[0].id;
        let generation = documents.generation();
        documents.dismiss(first);
        assert_eq!(documents.offers().len(), 1);
        assert!(documents.generation() > generation);
        assert!(documents.offer_prepared("not-a-presentation").is_err());
    }
}
