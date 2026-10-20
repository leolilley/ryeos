//! Authored discovery renders content; only the existing acknowledged admission
//! operation can request a new Surface. These are Core tests, not grant proofs.

use super::content::{AffordanceInvoke, Payload, Producer, ViewBinding};
use super::effect::RyeOsEffectKind;
use super::reducer::test_support;
use super::seat::SeatEventKind;
use crate::presentation::{SharedPresentationConfig, UiCompositionDefinition};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const VIEW_REF: &str = "view:ryeos/surfaces/list";

fn authored(relative: &str) -> Value {
    let path = ryeos_engine::test_support::workspace_root()
        .join("bundles/ryeos-ui/.ai")
        .join(relative);
    serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn catalogue_core() -> super::RyeOsCore {
    let mut session = test_support::shared_session();
    let root = &mut session.binding_attachments[0];
    root.posture = super::binding::UiEffectivePosture::Interactive;
    root.project_path = None;
    root.subject = super::binding::UiBindingSubject::Projectless;
    let UiCompositionDefinition::SharedPresentation { config } = &mut root.presentation.definition
    else {
        panic!("actual shared Config fixture required");
    };
    config.shared_presentation.dialogs.push(
        serde_json::from_value(json!({
            "id":"surfaces", "title":"Surfaces", "view":VIEW_REF
        }))
        .unwrap(),
    );
    root.presentation
        .views
        .insert(VIEW_REF.into(), authored("views/ryeos/surfaces/list.yaml"));
    let mut core = test_support::core(session);
    test_support::claim_fixture_producer(&mut core);
    core.open_dialog("surfaces");
    core
}

#[test]
fn authored_shared_configs_offer_discovery_without_changing_their_surface_seeds() {
    for (file, seed) in [
        ("presentation.yaml", "surface:ryeos/ui/base-observe"),
        (
            "terminal-presentation.yaml",
            "surface:ryeos/ui/lens-observe",
        ),
        ("atlas-presentation.yaml", "surface:ryeos/ui/atlas-observe"),
    ] {
        let config: SharedPresentationConfig =
            serde_json::from_value(authored(&format!("config/ryeos/ui/{file}"))).unwrap();
        let spec = &config.shared_presentation;
        assert_eq!(spec.initial_surfaces.len(), 1, "{file}");
        assert_eq!(spec.initial_surfaces[0].surface, seed, "{file}");
        assert!(spec.referenced_views().contains(VIEW_REF), "{file}");
        let declared_sources: BTreeSet<_> = spec
            .capabilities
            .sources
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(
            declared_sources,
            BTreeSet::from([
                "ryeos.execute.service.node/status",
                "ryeos.execute.service.projects/list",
                "ryeos.execute.service.ui/ryeos-ui/config/get",
                "ryeos.execute.service.ui/ryeos-ui/items/list",
            ]),
            "{file}"
        );
        assert!(
            spec.capabilities
                .affordances
                .iter()
                .any(|cap| { cap == "ryeos.execute.service.ui/session/surfaces/open" }),
            "{file}"
        );
        assert!(spec.docks.top.is_none() && spec.docks.bottom.is_none());
        assert!(spec.docks.left.is_none() && spec.docks.right.is_none());
    }
    let view: ViewBinding =
        serde_json::from_value(authored("views/ryeos/surfaces/list.yaml")).unwrap();
    let source = &view.sources["default"];
    assert_eq!(source.params, json!({"kind":"surface", "limit":200}));
    assert_eq!(source.coverage.as_deref(), Some("coverage"));
    assert!(!source.requires_project);
    assert!(source.params.get("project_path").is_none());
    let authored_view = authored("views/ryeos/surfaces/list.yaml");
    assert_eq!(
        authored_view["input"]["feeds"],
        json!({"param":"query", "debounce_ms":120})
    );
    assert!(authored_view["input"].get("local_filter").is_none());
    assert!(authored_view["input"].get("submit").is_none());
}

#[test]
fn catalogue_search_fetches_under_shared_root_without_surface_or_project() {
    let mut core = catalogue_core();
    let origin = core.presentation.as_ref().unwrap().dialogs()["surfaces"]
        .view()
        .instance_key()
        .clone();
    let before = core.seat.events().len();
    let effects = core.dispatch(super::event::RyeOsEvent::Ui {
        event: super::event::RyeOsUiEvent::SetInputText {
            text: "node".into(),
            cursor: 4,
        },
    });
    let requests: Vec<_> = effects
        .iter()
        .filter_map(|effect| match &effect.kind {
            RyeOsEffectKind::FetchSource { request, .. } => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 1);
    let request = requests[0];
    assert_eq!(request.binding_attachment_id, core.root_attachment_id);
    assert!(matches!(&request.coordinate,
        super::binding::UiBindingCoordinate::Source { view_ref, channel }
            if view_ref == VIEW_REF && channel == "default"));
    let super::binding::UiBindingPayload::SourceParameters { params } = &request.payload else {
        panic!("query is a compiled source parameter, not an invocation");
    };
    assert_eq!(
        params,
        &json!({"query":"node", "kind":"surface", "limit":200})
    );
    assert_eq!(
        core.seat.events().len(),
        before,
        "search does not prepare an admission"
    );
    assert!(core.active_surface().is_none());
    assert!(
        core.binding_attachment(&core.root_attachment_id)
            .unwrap()
            .project_path
            .is_none()
    );

    core.close_dialog(&origin);
    assert!(
        core.refresh_focused_feeds().is_empty(),
        "closed dialog cannot refetch its query"
    );
}

#[test]
fn catalogue_selection_waits_for_ack_and_assigns_its_exact_creation_identity() {
    let mut core = catalogue_core();
    assert!(core.active_surface().is_none());
    let origin = core.presentation.as_ref().unwrap().dialogs()["surfaces"]
        .view()
        .instance_key()
        .clone();
    let record = json!({
        "canonical_ref":"surface:example/node", "label":"Node",
        "trust":{"class":"trusted"}
    });
    assert!(
        core.invoke_affordance(&origin, VIEW_REF, "open", &record)
            .is_empty()
    );
    let SeatEventKind::PresentationOperationPrepared(prepared) =
        &core.seat.events().last().unwrap().kind
    else {
        panic!("opening must retain a preparation before dispatch");
    };
    let prepared = prepared.clone();
    prepared.validate().unwrap();
    let admission = prepared.admission().unwrap();
    assert_eq!(admission.invocation_origin, origin);
    assert_eq!(
        admission.request.binding_attachment_id,
        core.root_attachment_id
    );
    let super::binding::UiBindingPayload::Selection { record } = &admission.request.payload else {
        panic!("exact selection payload required");
    };
    assert_eq!(record["canonical_ref"], "surface:example/node");
    assert_eq!(record["creation_request_id"], prepared.operation_id);
    assert!(
        core.active_surface().is_none(),
        "discovery cannot install tabs"
    );

    let producer = core.presentation_operations.producer.clone().unwrap();
    let prefix = test_support::pending_prefix(&core);
    let effects = core.acknowledge_seat_prefix(&producer, prefix);
    assert!(effects.iter().any(|effect| matches!(
        &effect.kind,
        RyeOsEffectKind::InvokePresentationOperation { operation } if operation == &prepared
    )));
    let view: ViewBinding =
        serde_json::from_value(authored("views/ryeos/surfaces/list.yaml")).unwrap();
    let action = view
        .affordances
        .iter()
        .find(|action| action["id"] == "open")
        .unwrap();
    let Some(AffordanceInvoke::Service { item_ref, args, .. }) =
        super::content::resolve_affordance_invoke(
            action,
            Producer::Selection,
            &Payload::Selection(record),
        )
    else {
        panic!("acknowledged authored selection must resolve");
    };
    assert_eq!(item_ref, "service:ui/session/surfaces/open");
    assert_eq!(
        args["selection"],
        json!({"kind":"authored", "surface_ref":"surface:example/node"})
    );
    assert_eq!(args["creation_request_id"], prepared.operation_id);
    assert!(args.get("project_path").is_none());
}

#[test]
fn unavailable_trust_never_prepares_a_catalogue_open() {
    for class in ["untrusted", "unsigned", "unknown"] {
        let mut core = catalogue_core();
        let origin = core.presentation.as_ref().unwrap().dialogs()["surfaces"]
            .view()
            .instance_key()
            .clone();
        let before = core.seat.events().len();
        assert!(
            core.invoke_affordance(
                &origin,
                VIEW_REF,
                "open",
                &json!({
                    "canonical_ref":"surface:example/node", "trust":{"class":class}
                })
            )
            .is_empty()
        );
        assert_eq!(core.seat.events().len(), before, "{class}");
        assert!(core.active_surface().is_none());
    }
}

#[test]
fn closed_discovery_dialog_refuses_delayed_selection_and_reopens_without_surface() {
    let mut core = catalogue_core();
    let origin = core.presentation.as_ref().unwrap().dialogs()["surfaces"]
        .view()
        .instance_key()
        .clone();
    core.close_dialog(&origin);
    let before = core.seat.events().len();
    let record = json!({"canonical_ref":"surface:example/node", "trust":{"class":"trusted"}});
    assert!(
        core.invoke_affordance(&origin, VIEW_REF, "open", &record)
            .is_empty()
    );
    assert_eq!(core.seat.events().len(), before);
    assert!(core.active_surface().is_none());

    core.open_dialog("surfaces");
    assert!(core.presentation.as_ref().unwrap().dialogs()["surfaces"].is_open());
    assert!(
        core.invoke_affordance(&origin, VIEW_REF, "open", &record)
            .is_empty()
    );
    assert!(matches!(
        core.seat.events().last().unwrap().kind,
        SeatEventKind::PresentationOperationPrepared(_)
    ));
    assert!(core.active_surface().is_none());
}
