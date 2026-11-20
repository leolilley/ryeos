// A UI seat reads only what the node's signed `ui_seat_reads` policy admits:
// mint attenuates a source the policy does not admit, dispatch pins and
// checks parameters and shapes what is disclosed, and the policy exempts the
// UI plane's own services and nothing else.

use std::sync::Arc;

use serde_json::{Value, json};

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_node_runtime::node_config_source::BundleRecord;
use ryeos_ui::state::get_ui_state;
use ryeos_ui::test_support::{
    self as test_state, build_test_state_with_live_bundles, live_presentation_with_surface,
    set_seat_reads,
};

const NODE_SURFACE: &str = "surface:ryeos/ui/node";
const POLICY_SURFACE: &str = "surface:test/seat-read-policy";

const BUNDLES_ATTENUATION: &str = "source:view:ryeos/node/bundles:default";

fn entry(target: &str, extra: Value) -> Value {
    let mut base = json!({
        "target": target,
        "classes": ["local_operator", "hosted_principal"],
        "keep": ["/bundles/*/name"],
        "max_result_bytes": 65536,
        "max_items": 64
    });
    for (key, value) in extra.as_object().expect("entry fields") {
        base[key] = value.clone();
    }
    base
}

fn policy(entries: Vec<Value>) -> Value {
    json!({"schema": 1, "reads": entries})
}

fn add_bundles(state: &mut AppState, names: &[&str]) {
    let mut snapshot = (*state.node_config).clone();
    for name in names {
        snapshot.bundles.push(BundleRecord {
            name: (*name).to_owned(),
            path: format!("/srv/private/{name}").into(),
            source_file: format!("/srv/private/{name}.yaml").into(),
        });
    }
    state.node_config = Arc::new(snapshot);
}

/// The compiled binding of the Surface a session admitted.
fn binding_for(
    state: &AppState,
    session_id: &str,
    surface_ref: &str,
) -> ryeos_ui::compiled_binding::CompiledUiBinding {
    let session = get_ui_state(state)
        .unwrap()
        .browser_sessions
        .get_session(session_id)
        .unwrap();
    session
        .attachments
        .values()
        .map(|attachment| attachment.compiled_binding.binding.clone())
        .find(|binding| binding.surface.canonical_ref == surface_ref)
        .unwrap_or_else(|| panic!("no attachment of {surface_ref}"))
}

fn attenuation<'a>(
    binding: &'a ryeos_ui::compiled_binding::CompiledUiBinding,
    coordinate: &str,
) -> Option<&'a str> {
    binding
        .attenuated
        .iter()
        .find(|entry| entry.coordinate == coordinate)
        .map(|entry| entry.reason.as_str())
}

async fn dispatch_source(
    state: &Arc<AppState>,
    ctx: &HandlerContext,
    producer: &ryeos_client_base::ui::UiPresentationProducer,
    coordinate: &ryeos_ui::browser_session::BindingAttachmentCoordinate,
    view_ref: &str,
    params: Value,
) -> anyhow::Result<Value> {
    (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        json!({
            "producer": producer,
            "binding": {
                "binding_attachment_id": coordinate.binding_attachment_id,
                "binding_generation": coordinate.binding_generation,
                "binding_digest": coordinate.binding_digest,
                "coordinate": {
                    "kind": "source",
                    "view_ref": view_ref,
                    "channel": "default"
                },
                "payload": { "kind": "source_parameters", "params": params }
            }
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
}

fn refusal_text(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

#[tokio::test]
async fn mint_attenuates_a_source_the_policy_has_no_entry_for_and_keeps_the_ui_plane() {
    let (_tmp, _owner, mut state) = build_test_state_with_live_bundles();
    // Entries for other services only: bundle/list has none.
    set_seat_reads(
        &mut state,
        &policy(vec![entry(
            "service:projects/list",
            json!({"keep": ["/version"]}),
        )]),
    );
    let (session_id, _, _, _) = live_presentation_with_surface(&state, NODE_SURFACE).await;
    let binding = binding_for(&state, &session_id, NODE_SURFACE);

    let reason = attenuation(&binding, BUNDLES_ATTENUATION).expect("bundle source attenuated");
    assert!(reason.contains("seat_read_no_entry"), "{reason}");
    assert!(
        binding
            .sources
            .get("view:ryeos/node/bundles")
            .is_none_or(|channels| !channels.contains_key("default"))
    );

    // The node Surface's UI-plane projections stay bound with no entry.
    let threads = binding
        .sources
        .get("view:ryeos/node/threads/history")
        .expect("UI-plane thread history stays bound");
    assert!(threads.contains_key("default"));
}

#[tokio::test]
async fn an_empty_policy_admits_no_non_ui_plane_source() {
    let (_tmp, _owner, mut state) = build_test_state_with_live_bundles();
    set_seat_reads(&mut state, &policy(vec![]));
    let (session_id, _, _, _) = live_presentation_with_surface(&state, NODE_SURFACE).await;
    let binding = binding_for(&state, &session_id, NODE_SURFACE);

    for channels in binding.sources.values() {
        for source in channels.values() {
            assert!(
                ryeos_ui::seat_read_policy::is_ui_plane_service(
                    &source.target.identity.canonical_ref
                ),
                "{} is bound with no policy entry",
                source.target.identity.canonical_ref
            );
        }
    }
    assert!(attenuation(&binding, BUNDLES_ATTENUATION).is_some());
}

#[tokio::test]
async fn mint_attenuates_an_entry_naming_a_parameter_the_service_does_not_declare() {
    let (_tmp, _owner, mut state) = build_test_state_with_live_bundles();
    set_seat_reads(
        &mut state,
        &policy(vec![entry(
            "service:bundle/list",
            json!({"pin": {"holder": "@session:principal_id"}}),
        )]),
    );
    let (session_id, _, _, _) = live_presentation_with_surface(&state, NODE_SURFACE).await;
    let binding = binding_for(&state, &session_id, NODE_SURFACE);
    let reason = attenuation(&binding, BUNDLES_ATTENUATION).expect("attenuated");
    assert!(
        reason.contains("seat_read_undeclared_parameter"),
        "{reason}"
    );
}

#[tokio::test]
async fn mint_admits_a_class_only_an_entry_names() {
    // A hosted principal is the class every live fixture session carries.
    let (_tmp, _owner, mut state) = build_test_state_with_live_bundles();
    set_seat_reads(
        &mut state,
        &policy(vec![entry(
            "service:bundle/list",
            json!({"classes": ["local_operator"]}),
        )]),
    );
    let (session_id, _, _, _) = live_presentation_with_surface(&state, NODE_SURFACE).await;
    let binding = binding_for(&state, &session_id, NODE_SURFACE);
    let reason = attenuation(&binding, BUNDLES_ATTENUATION).expect("attenuated for hosted class");
    assert!(reason.contains("seat_read_no_entry"), "{reason}");
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn the_shipped_policy_withholds_bundle_paths_from_a_seat() {
    let (_tmp, _owner, mut state) = build_test_state_with_live_bundles();
    add_bundles(&mut state, &["alpha", "beta"]);
    let (session_id, ctx, producer, coordinate) =
        live_presentation_with_surface(&state, NODE_SURFACE).await;
    let binding = binding_for(&state, &session_id, NODE_SURFACE);
    assert!(
        attenuation(&binding, BUNDLES_ATTENUATION).is_none(),
        "the shipped policy admits the bundle list"
    );
    let state = Arc::new(state);

    let listed = dispatch_source(
        &state,
        &ctx,
        &producer,
        &coordinate,
        "view:ryeos/node/bundles",
        json!({}),
    )
    .await
    .unwrap_or_else(|error| panic!("bundle list: {}", refusal_text(&error)));
    assert_eq!(
        listed["result"]["result"],
        json!({"bundles": [{"name": "alpha"}, {"name": "beta"}]})
    );
    let text = serde_json::to_string(&listed).unwrap();
    assert!(!text.contains("/srv/private"), "{text}");
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn the_shipped_policy_keeps_node_status_without_its_filesystem_paths() {
    let (_tmp, _owner, state) = build_test_state_with_live_bundles();
    let (_id, ctx, producer, coordinate) =
        live_presentation_with_surface(&state, NODE_SURFACE).await;
    let state = Arc::new(state);
    let status = dispatch_source(
        &state,
        &ctx,
        &producer,
        &coordinate,
        "view:ryeos/node/status",
        json!({}),
    )
    .await
    .unwrap_or_else(|error| panic!("node status: {}", refusal_text(&error)));
    let shown = &status["result"]["result"];
    assert!(shown["node_id"].is_string());
    assert!(shown["version"].is_string());
    assert!(shown.get("uds_path").is_none());
    assert!(shown.get("db_path").is_none());
    assert!(shown["isolation"].get("source").is_none());
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn dispatch_refuses_a_read_whose_entry_is_gone_even_with_a_live_binding() {
    let (_tmp, _owner, mut state) = build_test_state_with_live_bundles();
    let (_id, ctx, producer, coordinate) =
        live_presentation_with_surface(&state, NODE_SURFACE).await;
    // The binding was minted under the shipped policy; the live snapshot now
    // has no entry for the service, and the decision is made on the snapshot.
    set_seat_reads(&mut state, &policy(vec![]));
    let state = Arc::new(state);
    let error = dispatch_source(
        &state,
        &ctx,
        &producer,
        &coordinate,
        "view:ryeos/node/bundles",
        json!({}),
    )
    .await
    .expect_err("no entry, no read");
    assert!(
        refusal_text(&error).contains("seat_read_no_entry"),
        "{}",
        refusal_text(&error)
    );
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn bounds_refuse_a_result_and_never_truncate_it() {
    let (_tmp, _owner, mut state) = build_test_state_with_live_bundles();
    add_bundles(&mut state, &["alpha", "beta", "gamma"]);
    set_seat_reads(
        &mut state,
        &policy(vec![entry("service:bundle/list", json!({"max_items": 2}))]),
    );
    let (_id, ctx, producer, coordinate) =
        live_presentation_with_surface(&state, NODE_SURFACE).await;
    let mut tight = state.clone();
    let state = Arc::new(state);
    let error = dispatch_source(
        &state,
        &ctx,
        &producer,
        &coordinate,
        "view:ryeos/node/bundles",
        json!({}),
    )
    .await
    .expect_err("three bundles exceed two items");
    assert!(
        refusal_text(&error).contains("seat_read_too_many_items"),
        "{}",
        refusal_text(&error)
    );

    set_seat_reads(
        &mut tight,
        &policy(vec![entry(
            "service:bundle/list",
            json!({"max_result_bytes": 16}),
        )]),
    );
    let state = Arc::new(tight);
    let error = dispatch_source(
        &state,
        &ctx,
        &producer,
        &coordinate,
        "view:ryeos/node/bundles",
        json!({}),
    )
    .await
    .expect_err("the kept result exceeds sixteen bytes");
    assert!(
        refusal_text(&error).contains("seat_read_result_too_large"),
        "{}",
        refusal_text(&error)
    );
}

/// A Surface and View of the fixture bundle that bind the core thread list
/// with a client-supplied `sort`, to exercise parameter policy.
fn install_thread_list_fixture(state: &AppState) {
    test_state::write_fixture_bundle_item(
        state,
        "views/test/seat-read-policy.yaml",
        "name: seat-read-policy\nversion: \"1.0.0\"\ndescription: Fixture view binding the core thread list with client-supplied parameters\n\nwidget: rows\nsources:\n  default:\n    ref: service:threads/list\n    params:\n      sort: \"@facet:selection.sort\"\n    collection: threads\nprojections:\n  primary: item_ref\n",
    );
    test_state::write_fixture_bundle_item(
        state,
        "surfaces/test/seat-read-policy.yaml",
        "name: seat-read-policy\ntitle: Seat read policy\nversion: \"1.0.0\"\ndescription: Fixture Surface\n\ncapabilities:\n  sources:\n    - ryeos.execute.service.threads/list\n  affordances: []\n\nsources: {}\naffordances: []\n\ntiling:\n  mode: master_stack\n  master: { side: left, arrange: vertical, ratio: 0.5, count: 1 }\n  stack: { arrange: vertical }\n  insert: end\n\ntiles: []\n\nstyle:\n  border: thin\n\nlibrary:\n  - group: Test\n    views:\n      - view:test/seat-read-policy\n",
    );
}

fn thread_list_entry(extra: Value) -> Value {
    let mut base = entry("service:threads/list", extra);
    base["keep"] = json!(["/next_cursor", "/threads/*/thread_id"]);
    base
}

const THREADS_VIEW: &str = "view:test/seat-read-policy";

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn a_pin_replaces_the_clients_parameter_before_the_condition_is_judged() {
    let (_tmp, _owner, mut state) = build_test_state_with_live_bundles();
    install_thread_list_fixture(&state);
    set_seat_reads(
        &mut state,
        &policy(vec![thread_list_entry(json!({
            "when": {"sort": ["watch"]},
            "pin": {"sort": "watch"}
        }))]),
    );
    let (id, ctx, producer, coordinate) =
        live_presentation_with_surface(&state, POLICY_SURFACE).await;
    let binding = binding_for(&state, &id, POLICY_SURFACE);
    assert!(
        attenuation(&binding, &format!("source:{THREADS_VIEW}:default")).is_none(),
        "{:?}",
        binding.attenuated
    );
    let state = Arc::new(state);

    // The client asks for `newest`; the pin makes it `watch`, which holds.
    let ok = dispatch_source(
        &state,
        &ctx,
        &producer,
        &coordinate,
        THREADS_VIEW,
        json!({"sort": "newest"}),
    )
    .await
    .unwrap_or_else(|error| panic!("pinned read: {}", refusal_text(&error)));
    assert!(ok["result"]["result"]["threads"].is_array());
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn a_denied_parameter_is_refused_whatever_its_value() {
    let (_tmp, _owner, mut state) = build_test_state_with_live_bundles();
    install_thread_list_fixture(&state);
    set_seat_reads(
        &mut state,
        &policy(vec![thread_list_entry(json!({"deny_params": ["sort"]}))]),
    );
    let (_id, ctx, producer, coordinate) =
        live_presentation_with_surface(&state, POLICY_SURFACE).await;
    let state = Arc::new(state);
    for sort in ["watch", "newest", "default"] {
        let error = dispatch_source(
            &state,
            &ctx,
            &producer,
            &coordinate,
            THREADS_VIEW,
            json!({"sort": sort}),
        )
        .await
        .expect_err("sort is denied");
        assert!(
            refusal_text(&error).contains("seat_read_parameter_denied"),
            "{}",
            refusal_text(&error)
        );
    }
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn a_condition_the_call_does_not_meet_is_refused() {
    let (_tmp, _owner, mut state) = build_test_state_with_live_bundles();
    install_thread_list_fixture(&state);
    set_seat_reads(
        &mut state,
        &policy(vec![thread_list_entry(
            json!({"when": {"sort": ["watch"]}}),
        )]),
    );
    let (_id, ctx, producer, coordinate) =
        live_presentation_with_surface(&state, POLICY_SURFACE).await;
    let state = Arc::new(state);

    let error = dispatch_source(
        &state,
        &ctx,
        &producer,
        &coordinate,
        THREADS_VIEW,
        json!({"sort": "newest"}),
    )
    .await
    .expect_err("sort newest is outside the entry's condition");
    assert!(
        refusal_text(&error).contains("seat_read_no_matching_entry"),
        "{}",
        refusal_text(&error)
    );

    let ok = dispatch_source(
        &state,
        &ctx,
        &producer,
        &coordinate,
        THREADS_VIEW,
        json!({"sort": "watch"}),
    )
    .await
    .unwrap_or_else(|error| panic!("matching read: {}", refusal_text(&error)));
    assert!(ok["result"]["result"]["threads"].is_array());
}
