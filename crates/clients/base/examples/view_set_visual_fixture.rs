//! Synthetic visual qualification only. Uses the production shared model;
//! no daemon, credentials, installed state or execution requests are involved.
use ryeos_client_base::ui::{
    BrowserSession, BrowserViewport, RyeOsCore, RyeOsEvent, RyeOsSourceInstanceKey, RyeOsUiEvent,
    RyeOsUiIntent, UiBindingAttachment, UiBindingAttachmentCoordinate, UiBindingRequestBounds,
    UiPresentationMembership, UiSurfaceMembership,
};
use serde_json::{Value, json};

fn dispatch(core: &mut RyeOsCore, intent: RyeOsUiIntent) {
    let _ = core.dispatch(RyeOsEvent::Ui {
        event: RyeOsUiEvent::Activate { intent },
    });
}

fn populate(core: &mut RyeOsCore, data: &Value) {
    let mut mounts: Vec<_> = core
        .active_view_set()
        .into_iter()
        .flat_map(|set| set.tiles.values())
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
        "summary":"Refine the view-set header and keep the current view when switching between remote sessions.",
        "metadata":["Codex", "Dev machine ↗", "started 14:28 · sample"]
    }});
    conversation["body"]["supplement"] =
        json!({"frame_label":"Worker","frame_detail":"/ ryeos-next"});
    conversation["input"]["placeholder"] = json!("Continue the conversation…");
    conversation["projections"]["event_kinds"]["author"] =
        json!({"primary":"text","meta":"label","role":"line"});
    let surface = json!({
        "name":"visual-qualification", "title":"Visual fixture", "style":{"view_set_tabs":true,"border":"thin"},
        "library":[{"group":"Work","views":["view:fixture/conversation","view:fixture/changes","view:fixture/execution"]},
            {"group":"Explore","views":["view:fixture/projects","view:fixture/overview"]}],
        "view_sets":[
            {"schema":2,"id":"overview","title":"Overview","root":{"type":"group","views":["view:fixture/overview","view:fixture/projects"],"active":0}},
            {"schema":2,"id":"development","title":"Development",
             "root":{"type":"split","axis":"horizontal","ratio":0.72,
                "first":{"type":"group","views":["view:fixture/conversation","view:fixture/activity","view:fixture/evidence"],"active":0},
                "second":{"type":"split","axis":"vertical","ratio":0.525,
                    "first":{"type":"group","views":["view:fixture/changes"],"active":0},
                    "second":{"type":"group","views":["view:fixture/execution"],"active":0}}}},
            {"schema":2,"id":"review","title":"Review","root":{"type":"group","views":["view:fixture/changes","view:fixture/evidence"],"active":0}}
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
                    {"title":"ViewSets","source_channel":"default","collection":"sets","projection":{"primary":"title","glyph":"glyph"}}
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
                    "supplement":{"excerpt_title":"view_set.rs · +14 −4","excerpt":[
                        {"field":"218","value":"  // Keep the selected view attached"},
                        {"field":"219","value":"  // to its existing view_set."},
                        {"field":"220","value":"+ let active = view_set.active_view();","tone":"good"},
                        {"field":"221","value":"+ header.set_selected(active);","tone":"good"},
                        {"field":"222","value":"  preserve_draft(&view_set);"}
                    ],"footer":"Sample changes · not an executed candidate"}},
                "projections":{"primary":"file","secondary":"path","meta":"change","glyph":"glyph"}}
        }
    });
    let data = json!({
        "view:fixture/projects":{"rows":[
            {"title":"RyeOS","detail":"next · local + 1 remote","glyph":"◇"},
            {"title":"ARC experiments","detail":"2 view_sets","glyph":"◇"},
            {"title":"Farm","detail":"1 view set","glyph":"◇"}],
            "views":[{"title":"Worker conversation","glyph":"◧"},{"title":"Changes","glyph":"≋"},{"title":"Execution","glyph":"⌁"},{"title":"Project files","glyph":"◇"}],
            "sets":[{"title":"Development","glyph":"⊞"},{"title":"Overview","glyph":"⊞"},{"title":"Open a view…","glyph":"＋"}]},
        "view:fixture/overview":{"rows":[
            {"title":"A clearer view of work.","detail":"Projects, conversations and evidence — arranged around the work at hand.","state":""},
            {"title":"Refine the view-set header","detail":"RyeOS / next · conversation and candidate available","state":"IN REVIEW"},
            {"title":"Explore the next experiment","detail":"ARC experiments · research notes","state":"READY"},
            {"title":"Review the latest simulation","detail":"Farm · evaluation evidence","state":"READY"}]},
        "view:fixture/conversation":{"rows":[
            {"event_type":"author","label":"14:28","text":"You"},
            {"event_type":"prose","text":"Keep the existing tiling behaviour. Make the active view set easier to identify, and check that switching views preserves the draft."},
            {"event_type":"author","label":"14:29","text":"Codex"},
            {"event_type":"prose","text":"I’ve traced view-set selection and draft ownership. The state is already retained; the header needs to make that relationship visible."},
            {"event_type":"author","label":"✓","text":"Read view-set and input bindings"},
            {"event_type":"author","label":"✓","text":"Refine the active view set header"},
            {"event_type":"prose","text":"I’ve kept the change within the current presentation model. I’m checking keyboard focus and restoring a draft after a view switch."}]},
        "view:fixture/changes":{"rows":[
            {"file":"view_set.rs","path":"clients/base/src/ui","change":"+14","glyph":"▧"},{"file":"view_model.rs","path":"clients/base/src/ui","change":"+6","glyph":"▧"},{"file":"web-shell.css","path":"clients/web/pkg","change":"+4","glyph":"▧"}]},
        "view:fixture/execution":{"rows":[
            {"title":"Format","state":"✓ Passed   0.8s","glyph":""},
            {"title":"Focused checks","state":"✓ Passed   2.1s","glyph":""},
            {"title":"Draft restoration","state":"◈ Running  24s","glyph":""}]},
        "view:fixture/evidence":{"rows":[{"title":"Sample evidence only","detail":"No execution or qualification is claimed by this preview","state":"FIXTURE"}]}
    });
    let surface_attachment = UiBindingAttachment {
        binding_attachment_id: "visual-fixture-attachment".into(),
        binding_generation: 1,
        binding_digest: "visual-fixture-binding".into(),
        root_ref: "surface:fixture/visual".into(),
        root_generation: "visual-fixture-surface".into(),
        presentation:
            ryeos_client_base::presentation::UiCompositionPresentation::from_effective_surface(
                surface.clone(),
            )
            .unwrap(),
        project_path: None,
        subject: ryeos_client_base::ui::UiBindingSubject::Projectless,
        posture: Default::default(),
        binding_request_bounds: UiBindingRequestBounds {
            max_request_bytes: 64 * 1024,
            max_input_bytes: 16 * 1024,
            max_seat_append_bytes: 64 * 1024,
        },
    };
    let shared_attachment = UiBindingAttachment {
        binding_attachment_id: "visual-shared-attachment".into(),
        binding_generation: 2,
        binding_digest: "visual-shared-binding".into(),
        root_ref: "config:fixture/presentation".into(),
        root_generation: "visual-shared-config".into(),
        presentation: serde_json::from_value(json!({
            "definition":{"kind":"shared_presentation","config":{
                "category":"ui","shared_presentation":{
                    "schema":1,"title":"Visual fixture","launcher":"launch",
                    "dialogs":[{"id":"launch","title":"Fixture launch","view":"view:fixture/launch"}],
                    "docks":{"left":{"content":"view:fixture/projects","open":true,"size":32}},
                    "capabilities":{"sources":[],"affordances":[]}
                }
            }},
            "views":{
                "view:fixture/projects":surface["views"]["view:fixture/projects"],
                "view:fixture/launch":{"widget":"text","title":"Fixture launch",
                    "body":{"lines":["Visual fixture only — no live launch operation."]}}
            }
        })).unwrap(),
        project_path: None, subject: ryeos_client_base::ui::UiBindingSubject::Projectless, posture: Default::default(),
        binding_request_bounds: surface_attachment.binding_request_bounds,
    };
    let coordinate = |attachment: &UiBindingAttachment| UiBindingAttachmentCoordinate {
        binding_attachment_id: attachment.binding_attachment_id.clone(),
        binding_generation: attachment.binding_generation,
        binding_digest: attachment.binding_digest.clone(),
    };
    let presentation_id = "11111111-1111-4111-8111-111111111111";
    let seeds = surface_attachment
        .presentation
        .definition
        .surface()
        .unwrap()
        .to_mount_seeds()
        .expect("fixture mount seeds");
    let active_seed = seeds.first().map(|seed| seed.id.clone());
    let mount_manifest =
        ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest::allocate(
            ryeos_client_base::ids::PresentationId::try_new(presentation_id).unwrap(),
            ryeos_client_base::ids::SurfaceInstanceId::try_new(
                "22222222-2222-4222-8222-222222222222",
            )
            .unwrap(),
            1,
            seeds
                .into_iter()
                .map(|seed| (seed, coordinate(&surface_attachment)))
                .collect(),
            active_seed.as_deref(),
        )
        .expect("fixture mount manifest");
    let membership = UiPresentationMembership {
        next_mount_id: mount_manifest.next_mount_id,
        initialization_digest: "a".repeat(64),
        presentation_id: presentation_id.into(),
        root_binding: coordinate(&shared_attachment),
        thread_id: None,
        chain_root_id: None,
        surfaces: vec![UiSurfaceMembership {
            mount_manifest,
            saved_surface: None,
            surface_instance_id: "22222222-2222-4222-8222-222222222222".into(),
            ordinal: 1,
            title: "Visual fixture".into(),
            binding: coordinate(&surface_attachment),
        }],
    };
    let mut core = RyeOsCore::new(
        BrowserSession {
            ui_binding_contract_revision: ryeos_client_base::UI_BINDING_CONTRACT_REVISION.into(),
            session_id: "visual-fixture".into(),
            user_principal_id: Some("fixture".into()),
            root_attachment_id: shared_attachment.binding_attachment_id.clone(),
            binding_attachments: vec![surface_attachment, shared_attachment],
            presentations: vec![membership],
            ..Default::default()
        },
        presentation_id,
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
    populate(&mut core, &data);
    let work = core.envelope(vec![]);
    let _ = core.dispatch(RyeOsEvent::Ui {
        event: RyeOsUiEvent::OpenLauncher,
    });
    let launcher = core.envelope(vec![]);
    println!(
        "{}",
        json!({"overview":overview,"work":work,"launcher":launcher})
    );
}
