//! Synthetic visual qualification only. Uses the production shared model;
//! no daemon, credentials, installed state or execution requests are involved.
use ryeos_client_base::ui::{
    BrowserSession, BrowserViewport, RyeOsCore, RyeOsEvent, RyeOsSourceInstanceKey, RyeOsUiEvent,
    RyeOsUiIntent,
};
use serde_json::{Value, json};

fn dispatch(core: &mut RyeOsCore, intent: RyeOsUiIntent) {
    let _ = core.dispatch(RyeOsEvent::Ui {
        event: RyeOsUiEvent::Activate { intent },
    });
}

fn populate(core: &mut RyeOsCore, data: &Value) {
    let workspace = &core.workspaces[core.active_workspace];
    let mut mounts: Vec<_> = workspace
        .tiles
        .values()
        .map(|tile| (tile.instance_key.clone(), tile.view.view_ref.clone()))
        .collect();
    mounts.extend(core.visible_dock_views());
    for (instance, view) in mounts {
        if let Some(value) = data.get(&view) {
            core.data.sources.insert(
                RyeOsSourceInstanceKey::named(instance, "default").encode(),
                value.clone(),
            );
        }
    }
}

fn main() {
    let source = json!({"default": {"ref": "service:fixture/read", "collection": "rows"}});
    let rows = |title: &str| {
        json!({"widget":"rows", "title":title, "sources":source,
        "projections":{"primary":"title", "secondary":"detail", "meta":"state"}})
    };
    let mut conversation = json!({"widget":"timeline", "title":"Conversation", "sources":source,
        "input":{"id":"message", "placeholder":"Message the selected work", "submit":"route"},
        "projections":{"default":{"primary":"text","meta":"label","role":"boundary"},
            "event_kinds":{"prose":{"primary":"text","role":"flow"}}}});
    conversation["description"] = json!("Sample conversation — no live execution");
    conversation["body"] = json!({"heading":{
        "eyebrow":"Development / 02", "title":"A clearer view of work.",
        "summary":"Refine the workspace header and keep the current view when switching between remote sessions.",
        "metadata":["Codex", "Dev machine ↗", "started 14:28 · sample"]
    }});
    conversation["body"]["supplement"] =
        json!({"frame_label":"Worker","frame_detail":"/ ryeos-next"});
    conversation["input"]["placeholder"] = json!("Continue the conversation…");
    conversation["projections"]["event_kinds"]["author"] =
        json!({"primary":"text","meta":"label","role":"line"});
    let surface = json!({
        "name":"visual-qualification", "style":{"workspace_tabs":true,"border":"thin"},
        "library":[{"group":"Work","views":["view:fixture/conversation","view:fixture/changes","view:fixture/execution"]},
            {"group":"Explore","views":["view:fixture/projects","view:fixture/overview"]}],
        "workspaces":[
            {"id":"overview","title":"Overview","root":{"type":"group","views":["view:fixture/overview","view:fixture/projects"],"active":0}},
            {"id":"development","title":"Development","slots":{"left":{"content":"view:fixture/projects","open":true,"size":26}},
             "root":{"type":"split","axis":"horizontal","ratio":0.72,
                "first":{"type":"group","views":["view:fixture/conversation","view:fixture/activity","view:fixture/evidence"],"active":0},
                "second":{"type":"split","axis":"vertical","ratio":0.525,
                    "first":{"type":"group","views":["view:fixture/changes"],"active":0},
                    "second":{"type":"group","views":["view:fixture/execution"],"active":0}}}},
            {"id":"review","title":"Review","root":{"type":"group","views":["view:fixture/changes","view:fixture/evidence"],"active":0}}
        ],
        "views":{
            "view:fixture/conversation":conversation,
            "view:fixture/activity":rows("Activity"),
            "view:fixture/evidence":rows("Evidence"),
            "view:fixture/projects":{"widget":"sections","title":"Explorer","sources":source,
                "body":{"supplement":{"frame_detail":"01","footer":"CONNECTED SITES","footer_rows":[
                    {"field":"●  Local","value":"this node"},
                    {"field":"●  Dev machine","value":"remote"},
                    {"field":"Visual fixture","value":"sample sites"}
                ]}},
                "sections":[
                    {"title":"Projects","source_channel":"default","collection":"rows","projection":{"primary":"title","secondary":"detail","glyph":"glyph"}},
                    {"title":"Open views","source_channel":"default","collection":"views","projection":{"primary":"title","glyph":"glyph"}},
                    {"title":"View sets","source_channel":"default","collection":"sets","projection":{"primary":"title","glyph":"glyph"}}
                ]},
            "view:fixture/overview":rows("Your work"),
            "view:fixture/execution":{
                "widget":"rows","title":"Execution","sources":source,
                "body":{"heading":{"title":"Work in motion"},
                    "supplement":{"frame_detail":"SAMPLE","footer":"Turn in progress · candidate not yet captured (sample)"},
                    "scene":{"objects":[
                        {"position":[46,-58,0],"to":[154,-58,0],"color":"#504945"},
                        {"position":[154,-58,0],"to":[322,-25,0],"color":"#504945"},
                        {"position":[154,-58,0],"to":[322,-90,0],"color":"#504945"},
                        {"position":[46,-58,0],"scale":[11,11,1],"glyph":"square","color":"#a89984"},
                        {"position":[154,-58,0],"scale":[15,15,1],"glyph":"diamond","color":"#fabd2f"},
                        {"position":[322,-25,0],"scale":[5,5,1],"color":"#8ec07c"},
                        {"position":[322,-90,0],"scale":[5,5,1],"color":"#8ec07c"},
                        {"kind":"text","label":"SOURCE","position":[29,-91,0],"color":"#a89984"},
                        {"kind":"text","label":"WORKER","position":[134,-106,0],"color":"#a89984"},
                        {"kind":"text","label":"FORMAT","position":[267,-14,0],"color":"#a89984"},
                        {"kind":"text","label":"CHECK","position":[267,-112,0],"color":"#a89984"}
                    ]}},
                "projections":{"primary":"title","meta":"state","glyph":"glyph"}},
            "view:fixture/changes":{"widget":"rows","title":"Changes","sources":source,
                "body":{"heading":{"title":"Working changes","summary":"Private workspace · not published"},
                    "supplement":{"excerpt_title":"workspace.rs · +14 −4","excerpt":[
                        {"field":"218","value":"  // Keep the selected view attached"},
                        {"field":"219","value":"  // to its existing workspace."},
                        {"field":"220","value":"+ let active = workspace.active_view();","tone":"good"},
                        {"field":"221","value":"+ header.set_selected(active);","tone":"good"},
                        {"field":"222","value":"  preserve_draft(&workspace);"}
                    ],"footer":"Sample changes · not an executed candidate"}},
                "projections":{"primary":"file","secondary":"path","meta":"change","glyph":"glyph"}}
        }
    });
    let data = json!({
        "view:fixture/projects":{"rows":[
            {"title":"RyeOS","detail":"next · local + 1 remote","glyph":"◇"},
            {"title":"ARC experiments","detail":"2 workspaces","glyph":"◇"},
            {"title":"Farm","detail":"1 workspace","glyph":"◇"}],
            "views":[{"title":"Worker conversation","glyph":"◧"},{"title":"Changes","glyph":"≋"},{"title":"Execution","glyph":"⌁"},{"title":"Project files","glyph":"◇"}],
            "sets":[{"title":"Development","glyph":"⊞"},{"title":"Overview","glyph":"⊞"},{"title":"Open a view…","glyph":"＋"}]},
        "view:fixture/overview":{"rows":[
            {"title":"A clearer view of work.","detail":"Projects, conversations and evidence — arranged around the work at hand.","state":""},
            {"title":"Refine the workspace header","detail":"RyeOS / next · conversation and candidate available","state":"IN REVIEW"},
            {"title":"Explore the next experiment","detail":"ARC experiments · research notes","state":"READY"},
            {"title":"Review the latest simulation","detail":"Farm · evaluation evidence","state":"READY"}]},
        "view:fixture/conversation":{"rows":[
            {"event_type":"author","label":"14:28","text":"You"},
            {"event_type":"prose","text":"Keep the existing tiling behaviour. Make the active workspace easier to identify, and check that switching views preserves the draft."},
            {"event_type":"author","label":"14:29","text":"Codex"},
            {"event_type":"prose","text":"I’ve traced workspace selection and draft ownership. The state is already retained; the header needs to make that relationship visible."},
            {"event_type":"author","label":"✓","text":"Read workspace and input bindings"},
            {"event_type":"author","label":"✓","text":"Refine the active workspace header"},
            {"event_type":"prose","text":"I’ve kept the change within the current presentation model. I’m checking keyboard focus and restoring a draft after a view switch."}]},
        "view:fixture/changes":{"rows":[
            {"file":"workspace.rs","path":"clients/base/src/ui","change":"+14","glyph":"▧"},{"file":"view_model.rs","path":"clients/base/src/ui","change":"+6","glyph":"▧"},{"file":"web-shell.css","path":"clients/web/pkg","change":"+4","glyph":"▧"}]},
        "view:fixture/execution":{"rows":[
            {"title":"Format","state":"✓ Passed   0.8s","glyph":""},
            {"title":"Focused checks","state":"✓ Passed   2.1s","glyph":""},
            {"title":"Draft restoration","state":"◈ Running  24s","glyph":""}]},
        "view:fixture/evidence":{"rows":[{"title":"Sample evidence only","detail":"No execution or qualification is claimed by this preview","state":"FIXTURE"}]}
    });
    let mut core = RyeOsCore::new(
        BrowserSession {
            ui_binding_contract_revision: ryeos_client_base::UI_BINDING_CONTRACT_REVISION.into(),
            session_id: "visual-fixture".into(),
            binding_digest: "fixture-not-authority".into(),
            surface_ref: "surface:fixture/visual".into(),
            user_principal_id: Some("fixture".into()),
            effective_surface: Some(surface),
            ..Default::default()
        },
        BrowserViewport {
            width: 1600,
            height: 1000,
            device_pixel_ratio: 1.0,
        },
        0,
    );
    core.notice(
        "VISUAL PREVIEW · sample data · not connected to a node",
        ryeos_client_base::ui::view_model::RyeOsTone::Warn,
    );
    populate(&mut core, &data);
    let overview = core.envelope(vec![]);
    dispatch(&mut core, RyeOsUiIntent::SwitchTab { index: 1 });
    core.workspaces[core.active_workspace].dock_local.insert(
        ryeos_client_base::ui::model::dock_view_instance_key(
            ryeos_client_base::ui::model::RyeOsDockEdge::Left,
        ),
        ryeos_client_base::workspace::ViewSpec::bound("view:fixture/projects")
            .initial_local_state(),
    );
    populate(&mut core, &data);
    let work = core.envelope(vec![]);
    let _ = core.dispatch(RyeOsEvent::Ui {
        event: RyeOsUiEvent::OpenOverlay {
            overlay_id: "views".into(),
        },
    });
    let launcher = core.envelope(vec![]);
    println!(
        "{}",
        json!({"overview":overview,"work":work,"launcher":launcher})
    );
}
