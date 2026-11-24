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

/// A UI-plane service has no seat-policy entry, so the only thing shaping its
/// answer is the service item's own tier. The seat's key is not the operator's,
/// so a field the tier withholds must stay withheld in the dispatch envelope's
/// `result`, the path a client reads every bound source through.
#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn a_field_the_service_tier_withholds_stays_withheld_through_dispatch() {
    let (_tmp, _owner, state) = build_test_state_with_live_bundles();
    let (_id, ctx, producer, coordinate) =
        live_presentation_with_surface(&state, NODE_SURFACE).await;
    let state = Arc::new(state);
    // The handler omits `state` and `recent_events` when the node has neither
    // sidecar nor log, so seed both: the answer then really holds the fields
    // the tier withholds.
    let gc_dir = state.config.runtime_state_dir();
    std::fs::create_dir_all(gc_dir.join("logs")).expect("gc log directory");
    std::fs::write(
        gc_dir.join("gc.state.json"),
        br#"{"phase":"idle","note":"/srv/private/gc"}"#,
    )
    .expect("gc state sidecar");
    std::fs::write(
        gc_dir.join("logs").join("gc.jsonl"),
        b"{\"event\":\"swept\",\"path\":\"/srv/private/x\"}\n",
    )
    .expect("gc log");
    // Positive control: the configured operator's own call of the handler,
    // which no tier shapes, holds the fields the tier withholds from a seat.
    let operator = ryeos_ui::test_support::local_operator_context(&state, vec!["*".into()]);
    let raw = (ryeos_ui::handlers::ui_gc::DESCRIPTOR.handler)(json!({}), operator, state.clone())
        .await
        .unwrap_or_else(|error| panic!("gc handler: {}", refusal_text(&error)));
    assert!(raw.get("state").is_some(), "{raw}");
    assert!(raw.get("recent_events").is_some(), "{raw}");
    assert!(raw.get("schema_version").is_some(), "{raw}");
    let gc = dispatch_source(
        &state,
        &ctx,
        &producer,
        &coordinate,
        "view:ryeos/node/gc",
        json!({}),
    )
    .await
    .unwrap_or_else(|error| panic!("gc status: {}", refusal_text(&error)));
    let shown = &gc["result"]["result"];
    assert!(shown.is_object(), "{shown}");
    assert!(shown.get("schema_version").is_some(), "{shown}");
    // The service item names `state` and `recent_events` as free text: raw
    // sidecar and log JSON that only the operator may read.
    assert!(shown.get("state").is_none(), "{shown}");
    assert!(shown.get("recent_events").is_none(), "{shown}");
    let text = serde_json::to_string(shown).unwrap();
    assert!(!text.contains("/srv/private"), "{text}");
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

/// Every service a shipped view or Surface binds as a source is either a UI
/// plane service or has a policy entry in every shipped init profile, so no
/// shipped source attenuates for want of an entry.
#[test]
fn every_shipped_source_has_an_entry_in_every_profile() {
    use ryeos_node_runtime::node_policy::NodePolicyContext;
    use ryeos_node_runtime::node_policy::NodePolicySection as _;
    use ryeos_node_runtime::node_policy::sections::ui_seat_reads::{
        SECTION_NAME, UiSeatReadsPolicy, UiSeatReadsPolicySection,
    };

    fn collect(value: &serde_yaml::Value, in_sources: bool, found: &mut Vec<String>) {
        match value {
            serde_yaml::Value::Mapping(map) => {
                for (key, child) in map {
                    let key = key.as_str().unwrap_or_default();
                    if in_sources
                        && key == "ref"
                        && let Some(reference) = child.as_str()
                    {
                        found.push(reference.to_owned());
                    }
                    collect(child, in_sources || key == "sources", found);
                }
            }
            serde_yaml::Value::Sequence(items) => {
                for item in items {
                    collect(item, in_sources, found);
                }
            }
            _ => {}
        }
    }

    fn yaml_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                yaml_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "yaml") {
                out.push(path);
            }
        }
    }

    let root = ryeos_engine::test_support::workspace_root();
    let ai = root.join("bundles/ryeos-ui/.ai");
    let mut files = Vec::new();
    yaml_files(&ai.join("views"), &mut files);
    yaml_files(&ai.join("surfaces"), &mut files);
    let mut sources = Vec::new();
    for file in &files {
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        collect(&parsed, false, &mut sources);
    }
    sources.retain(|reference| {
        reference.starts_with("service:")
            && !ryeos_ui::seat_read_policy::is_ui_plane_service(reference)
    });
    sources.sort();
    sources.dedup();
    assert!(
        sources.iter().any(|source| source == "service:node/status"),
        "the sweep found no shipped sources: {sources:?}"
    );

    let mut profiles = Vec::new();
    yaml_files(&root.join("bundles/.ai/node/init/profiles"), &mut profiles);
    assert_eq!(profiles.len(), 15);
    for path in profiles {
        let profile: serde_json::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let record = UiSeatReadsPolicySection
            .parse(
                &NodePolicyContext {
                    section: SECTION_NAME.to_owned(),
                    source_file: path.clone(),
                    signer_fingerprint: "test".to_owned(),
                },
                &profile["policies"][SECTION_NAME],
            )
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        let policy = record.as_any().downcast_ref::<UiSeatReadsPolicy>().unwrap();
        for source in &sources {
            assert!(
                !policy.entries_for(source).is_empty(),
                "{} has no entry for shipped source {source}",
                path.display()
            );
        }
    }
}

// ── signed revision history ──────────────────────────────────────────────

mod revisions {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use ryeos_node_runtime::node_policy::NodePolicyContext;
    use ryeos_node_runtime::node_policy::NodePolicySection as _;
    use ryeos_node_runtime::node_policy::sections::ui_seat_reads::{
        SECTION_NAME, SeatClass, UiSeatReadsPolicy, UiSeatReadsPolicySection,
    };
    use ryeos_ui::seat_read_policy::{RequestFacts, SeatReadRefusal, SessionFacts, decide};

    const LIST: &str = "service:revisions/list";
    const GET: &str = "service:revisions/get";
    const OWN_PROJECT: &str = "/srv/projects/own";
    const OTHER_PROJECT: &str = "/srv/projects/other";

    fn profile_paths() -> Vec<std::path::PathBuf> {
        let root = ryeos_engine::test_support::workspace_root();
        let dir = root.join("bundles/.ai/node/init/profiles");
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
            .collect();
        paths.sort();
        paths
    }

    fn policy_of(path: &std::path::Path) -> UiSeatReadsPolicy {
        let profile: Value = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let record = UiSeatReadsPolicySection
            .parse(
                &NodePolicyContext {
                    section: SECTION_NAME.to_owned(),
                    source_file: path.to_owned(),
                    signer_fingerprint: "test".to_owned(),
                },
                &profile["policies"][SECTION_NAME],
            )
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        record
            .as_any()
            .downcast_ref::<UiSeatReadsPolicy>()
            .unwrap()
            .clone()
    }

    fn standard() -> UiSeatReadsPolicy {
        policy_of(
            &ryeos_engine::test_support::workspace_root()
                .join("bundles/.ai/node/init/profiles/standard.yaml"),
        )
    }

    /// The parameters the service's own descriptor declares, as a bound
    /// target reports them.
    fn declared(name: &str) -> BTreeMap<String, String> {
        let root = ryeos_engine::test_support::workspace_root();
        let path = root.join(format!("bundles/core/.ai/services/revisions/{name}.yaml"));
        let service: Value = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        service["schema"]
            .as_object()
            .expect("the service declares its parameters")
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned()))
            .collect()
    }

    fn call(
        policy: &UiSeatReadsPolicy,
        target: &str,
        params: Value,
        project_root: Option<&str>,
    ) -> Result<(Value, ryeos_ui::seat_read_policy::DetachedShaping), SeatReadRefusal> {
        let name = target.rsplit('/').next().unwrap();
        let admitted = decide(
            policy,
            SeatClass::HostedPrincipal,
            target,
            &declared(name),
            Some(RequestFacts {
                params,
                session: SessionFacts {
                    principal_id: "fp:7e577e577e577e577e577e577e577e577e577e577e577e577e577e577e577e57",
                    project_root,
                },
            }),
        )?;
        Ok((
            admitted.params.expect("a call returns its parameters"),
            admitted
                .shaping
                .expect("a policy read has shaping")
                .detach(),
        ))
    }

    const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// One list row as the service writes it, with the fields a seat must
    /// not see: failure detail and a provenance entry that names a path.
    fn list_row(seq: u64) -> Value {
        json!({
            "seq": seq,
            "hash": DIGEST_B,
            "verify": {
                "status": "verified",
                "scheme": "signed_revision_record",
                "signer": "fp:abc",
                "reason": null,
                "detail": "/home/operator/project/.ai/revisions/view/a/00000001.yaml"
            },
            "unrecorded_change_before": false,
            "tombstone": false,
            "recorded_at": "2026-11-22T00:00:00Z",
            "actor": {"fingerprint": "fp:abc", "role": "authoring"},
            "before_digest": DIGEST_A,
            "after_digest": DIGEST_B,
            "file_ext": ".yaml",
            "provenance": {
                "authored_by": "runtime",
                "thread_id": "T-1",
                "path": "/home/operator/project/.ai/views/a.yaml",
                "acting_principal": "fp:someone",
                "authoring_capability": "ryeos.author.view"
            }
        })
    }

    fn list_answer(rows: usize) -> Value {
        json!({
            "item_ref": "view:a",
            "total": rows,
            "sound": true,
            "head": {"status": "head_invalid", "reason": "/var/lib/node/heads.json is not readable"},
            "file": "matches",
            "records": (1..=rows as u64).map(list_row).collect::<Vec<_>>(),
            "next_cursor": null
        })
    }

    fn get_answer(content: &str) -> Value {
        let mut answer = list_answer(1);
        let mut record = list_row(1);
        record["body"] = json!({
            "schema": "ryeos.revision.v1",
            "item_ref": "view:a",
            "seq": 1,
            "previous": "genesis:view:a",
            "before_digest": DIGEST_A,
            "after_digest": DIGEST_B,
            "file_ext": ".yaml",
            "content": content,
            "recorded_at": "2026-11-22T00:00:00Z",
            "actor": {"fingerprint": "fp:abc", "role": "authoring"},
            "provenance": {"path": "/home/operator/project/.ai/views/a.yaml"}
        });
        answer["record"] = record;
        answer.as_object_mut().unwrap().remove("records");
        answer.as_object_mut().unwrap().remove("next_cursor");
        answer
    }

    #[test]
    fn every_profile_admits_both_reads_for_both_classes_and_pins_the_project() {
        let paths = profile_paths();
        assert_eq!(paths.len(), 15);
        for path in paths {
            let policy = policy_of(&path);
            for target in [LIST, GET] {
                let entries = policy.entries_for(target);
                assert_eq!(entries.len(), 1, "{} {target}", path.display());
                let entry = entries[0];
                for class in [SeatClass::LocalOperator, SeatClass::HostedPrincipal] {
                    assert!(
                        entry.classes.contains(&class),
                        "{} {target}",
                        path.display()
                    );
                }
                assert_eq!(
                    entry.pin.get("project_path"),
                    Some(&json!("@session:project_root")),
                    "{} {target}",
                    path.display()
                );
                // Every parameter the entry names is one the service declares.
                for class in [SeatClass::LocalOperator, SeatClass::HostedPrincipal] {
                    decide(
                        &policy,
                        class,
                        target,
                        &declared(target.rsplit('/').next().unwrap()),
                        None,
                    )
                    .unwrap_or_else(|error| panic!("{} {target}: {error}", path.display()));
                }
            }
        }
    }

    #[test]
    fn a_seat_reads_the_revisions_of_its_own_project_and_the_kept_fields_arrive() {
        let policy = standard();
        let (params, shaping) = call(
            &policy,
            LIST,
            json!({"item_ref": "view:a", "limit": 50}),
            Some(OWN_PROJECT),
        )
        .expect("a project-bound seat reads its own history");
        assert_eq!(params["project_path"], OWN_PROJECT);
        assert_eq!(params["item_ref"], "view:a");
        assert_eq!(params["limit"], 50);

        let shown = shaping.apply(list_answer(2)).expect("within bounds");
        assert_eq!(shown["item_ref"], "view:a");
        assert_eq!(shown["total"], 2);
        assert_eq!(shown["sound"], true);
        assert_eq!(shown["file"], "matches");
        assert_eq!(shown["head"]["status"], "head_invalid");
        let row = &shown["records"][1];
        assert_eq!(row["seq"], 2);
        assert_eq!(row["hash"], DIGEST_B);
        assert_eq!(row["before_digest"], DIGEST_A);
        assert_eq!(row["after_digest"], DIGEST_B);
        assert_eq!(row["recorded_at"], "2026-11-22T00:00:00Z");
        assert_eq!(
            row["actor"],
            json!({"fingerprint": "fp:abc", "role": "authoring"})
        );
        assert_eq!(row["verify"]["status"], "verified");
        assert_eq!(row["verify"]["signer"], "fp:abc");
        assert_eq!(row["provenance"]["thread_id"], "T-1");
        assert_eq!(
            row["provenance"]["authoring_capability"],
            "ryeos.author.view"
        );
    }

    #[test]
    fn the_record_read_keeps_the_signed_item_bytes() {
        let policy = standard();
        let (_, shaping) = call(
            &policy,
            GET,
            json!({"item_ref": "view:a", "seq": 1}),
            Some(OWN_PROJECT),
        )
        .unwrap();
        let shown = shaping.apply(get_answer("name: a\n")).unwrap();
        assert_eq!(shown["record"]["body"]["content"], "name: a\n");
        assert_eq!(shown["record"]["body"]["previous"], "genesis:view:a");
        assert_eq!(shown["record"]["seq"], 1);
        assert_eq!(shown["record"]["after_digest"], DIGEST_B);
    }

    #[test]
    fn a_seat_cannot_read_another_projects_revisions() {
        let policy = standard();
        for target in [LIST, GET] {
            // Whatever path the client names, the call carries the session's.
            let (params, _) = call(
                &policy,
                target,
                json!({"item_ref": "view:a", "seq": 1, "project_path": OTHER_PROJECT}),
                Some(OWN_PROJECT),
            )
            .unwrap();
            assert_eq!(params["project_path"], OWN_PROJECT, "{target}");
            assert!(!params.to_string().contains(OTHER_PROJECT), "{target}");

            // A session with no project has nothing to read.
            let refusal = call(
                &policy,
                target,
                json!({"item_ref": "view:a", "seq": 1, "project_path": OTHER_PROJECT}),
                None,
            )
            .expect_err("no bound project, no read");
            assert_eq!(refusal.code(), "seat_read_marker_unavailable", "{target}");
        }
    }

    #[test]
    fn a_seat_cannot_ask_for_unverified_records() {
        let refusal = call(
            &standard(),
            GET,
            json!({"item_ref": "view:a", "seq": 1, "include_unverified": false}),
            Some(OWN_PROJECT),
        )
        .expect_err("the parameter is denied whatever its value");
        assert_eq!(refusal.code(), "seat_read_parameter_denied");
    }

    #[test]
    fn dropped_fields_are_absent() {
        let policy = standard();
        let (_, list_shaping) = call(
            &policy,
            LIST,
            json!({"item_ref": "view:a"}),
            Some(OWN_PROJECT),
        )
        .unwrap();
        let mut answer = list_answer(1);
        answer["a_later_field"] = json!("x");
        answer["records"][0]["a_later_field"] = json!("x");
        let shown = list_shaping.apply(answer).unwrap();
        let text = shown.to_string();
        for forbidden in [
            "/home/operator",
            "/var/lib",
            "acting_principal",
            "a_later_field",
        ] {
            assert!(!text.contains(forbidden), "{forbidden} leaked: {text}");
        }
        assert!(shown["head"].get("reason").is_none());
        assert!(shown["records"][0]["verify"].get("detail").is_none());
        assert!(shown["records"][0].get("body").is_none());
        assert!(shown["records"][0]["provenance"].get("path").is_none());

        let (_, get_shaping) = call(
            &policy,
            GET,
            json!({"item_ref": "view:a", "seq": 1}),
            Some(OWN_PROJECT),
        )
        .unwrap();
        let shown = get_shaping.apply(get_answer("name: a\n")).unwrap();
        let body = &shown["record"]["body"];
        assert!(body.get("provenance").is_none());
        assert!(body.get("schema").is_none());
        assert!(!shown.to_string().contains("/home/operator"));
    }

    #[test]
    fn an_oversized_result_is_refused_not_cut() {
        let policy = standard();
        let (_, list_shaping) = call(
            &policy,
            LIST,
            json!({"item_ref": "view:a"}),
            Some(OWN_PROJECT),
        )
        .unwrap();
        // A page over the service's own largest page holds more rows than
        // the entry allows.
        let refusal = list_shaping
            .apply(list_answer(201))
            .expect_err("more rows than max_items");
        assert_eq!(refusal.code(), "seat_read_too_many_items");
        list_shaping
            .apply(list_answer(200))
            .expect("the service's largest page fits");

        let (_, get_shaping) = call(
            &policy,
            GET,
            json!({"item_ref": "view:a", "seq": 1}),
            Some(OWN_PROJECT),
        )
        .unwrap();
        get_shaping
            .apply(get_answer(&"x".repeat(512 * 1024)))
            .expect("a large authored item fits");
        let refusal = get_shaping
            .apply(get_answer(&"x".repeat(2 * 1024 * 1024)))
            .expect_err("signed bytes over the bound");
        assert_eq!(refusal.code(), "seat_read_result_too_large");
    }
}

/// `items/revised` is a write the operator's own process makes after it has
/// changed an item; no seat may call it and no seat may read it. No shipped
/// profile grants it to a browser session, lists it as a seat read, or grants
/// it through a pattern.
#[test]
fn no_shipped_profile_lets_a_seat_reach_the_item_announcement_service() {
    const CAPABILITY: &str = "ryeos.execute.service.items/revised";
    fn find<'a>(value: &'a serde_yaml::Value, key: &str, out: &mut Vec<&'a serde_yaml::Value>) {
        match value {
            serde_yaml::Value::Mapping(map) => {
                for (name, inner) in map {
                    if name.as_str() == Some(key) {
                        out.push(inner);
                    }
                    find(inner, key, out);
                }
            }
            serde_yaml::Value::Sequence(items) => {
                for inner in items {
                    find(inner, key, out);
                }
            }
            _ => {}
        }
    }
    let profiles = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../bundles/.ai/node/init/profiles");
    let mut checked = 0;
    for entry in std::fs::read_dir(&profiles).expect("profiles directory") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let document: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        let mut scopes = Vec::new();
        find(&document, "session_scopes", &mut scopes);
        for list in scopes {
            for scope in list.as_sequence().expect("scope list") {
                let scope = scope.as_str().expect("scope text");
                let covers = match scope.strip_suffix('*') {
                    Some(prefix) => CAPABILITY.starts_with(prefix),
                    None => scope == CAPABILITY,
                };
                assert!(!covers, "{}: session scope `{scope}`", path.display());
            }
        }
        let mut reads = Vec::new();
        find(&document, "ui_seat_reads", &mut reads);
        for policy in reads {
            let mut targets = Vec::new();
            find(policy, "target", &mut targets);
            for target in targets {
                assert_ne!(
                    target.as_str(),
                    Some("service:items/revised"),
                    "{}",
                    path.display()
                );
            }
        }
        checked += 1;
    }
    assert!(checked >= 10, "profiles were not found under {profiles:?}");
}
