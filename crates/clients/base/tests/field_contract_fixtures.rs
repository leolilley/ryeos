use std::time::Instant;

use ryeos_client_base::ui::content::ViewBinding;
use ryeos_client_base::ui::field::{
    FIELD_FACTS_SCHEMA, FIELD_PROJECTION_SCHEMA, FieldSourceInput, parse_field_document,
    project_field,
};
use serde_json::{Value, json};

const LIVE: &str = include_str!("fixtures/field/build-deploy-live.json");
const BRAID_CUT: &str = include_str!("fixtures/field/build-deploy-braid-cut.json");
const DUPLICATES: &str = include_str!("fixtures/field/hook-observation-duplicates.json");
const REAL_PROJECT: &str = include_str!("fixtures/field/real-project-portfolio.json");

#[test]
fn generic_truth_fixtures_are_well_formed_and_domain_neutral() {
    let live: Value = serde_json::from_str(LIVE).expect("live fixture JSON");
    let cut: Value = serde_json::from_str(BRAID_CUT).expect("cut fixture JSON");

    let parsed_live = parse_field_document(&live).expect("live fixture satisfies the wire schema");
    let parsed_cut =
        parse_field_document(&cut).expect("braid-cut fixture satisfies the wire schema");
    assert_eq!(parsed_live.entities.len(), 11);
    assert_eq!(parsed_live.relations.len(), 8);
    assert_eq!(parsed_cut.entities.len(), 3);
    assert_eq!(parsed_cut.relations.len(), 1);

    for fixture in [&live, &cut] {
        assert_eq!(fixture["schema_version"], "ryeos.ui.field.facts.v1");
        let encoded = serde_json::to_string(fixture).unwrap().to_lowercase();
        for forbidden in ["arc.", "game_solver", "controller", "accepted_state"] {
            assert!(
                !encoded.contains(forbidden),
                "generic fixture contains {forbidden}"
            );
        }
        for entity in fixture["entities"].as_array().expect("entities array") {
            assert!(entity["id"].is_string(), "entity has stable id: {entity}");
            assert!(
                entity["provenance"]["evidence"]
                    .as_array()
                    .is_some_and(|evidence| !evidence.is_empty()),
                "entity has evidence: {entity}"
            );
        }
    }

    assert_eq!(cut["cursor"]["mode"], "braid_cut");
    assert_eq!(cut["cursor"]["through_chain_seq"], 12);
    assert_eq!(cut["cursor"]["outside_cut"].as_array().unwrap().len(), 2);
    assert_eq!(cut["replay"]["previous"]["chain_seq"], 11);
    assert_eq!(cut["replay"]["next"]["chain_seq"], 13);
    assert_eq!(cut["replay"]["live_head"]["chain_seq"], 18);
}

#[test]
fn duplicate_fixture_distinguishes_fold_from_integrity_error() {
    let fixture: Value = serde_json::from_str(DUPLICATES).expect("duplicates fixture JSON");
    let valid = fixture["valid"].as_array().unwrap();
    assert_eq!(valid[0], valid[1], "valid retry is byte-identical evidence");

    let invalid = fixture["invalid"].as_array().unwrap();
    assert_eq!(invalid[0]["observation_id"], invalid[1]["observation_id"]);
    assert_ne!(invalid[0]["response_hash"], invalid[1]["response_hash"]);
    assert_ne!(invalid[0]["observation"], invalid[1]["observation"]);
}

#[test]
fn real_project_fixture_uses_the_same_generic_contract() {
    let fixture: Value = serde_json::from_str(REAL_PROJECT).expect("real field fixture JSON");
    let parsed = parse_field_document(&fixture).expect("real field fixture parses generically");
    assert_eq!(parsed.entities.len(), 121);
    assert_eq!(parsed.relations.len(), 76);
    assert!(!parsed.truncated);
    assert!(parsed.warnings.is_empty());
    assert!(
        parsed
            .entities
            .iter()
            .all(|entity| !entity.provenance.evidence.is_empty())
    );
}

#[test]
fn real_project_fixture_exercises_occurrence_scoped_compound_joins() {
    let fixture: Value = serde_json::from_str(REAL_PROJECT).expect("real field fixture JSON");
    let parsed = parse_field_document(&fixture).expect("real field fixture parses generically");
    let parsed_result = Ok(parsed);
    let binding: ViewBinding = serde_json::from_value(json!({
        "widget": "field",
        "sources": {"execution": {"ref": "service:fixture/field/execution"}},
        "projections": {
            "schema_version": FIELD_PROJECTION_SCHEMA,
            "derived_relations": [{
                "id": "admitted-controller",
                "left": {
                    "match": {
                        "source": "execution",
                        "kind": "hook_observation",
                        "attributes.observation.kind": "arc.portfolio_decision"
                    },
                    "keys": [
                        "attributes.hook.occurrence.graph_run_id",
                        "attributes.observation.payload.selected_candidate_key"
                    ]
                },
                "right": {
                    "match": {"source": "execution", "kind": "thread"},
                    "keys": [
                        "attributes.thread.facets.portfolio",
                        "attributes.thread.facets.candidate_key"
                    ]
                },
                "relation": {"kind": "admitted", "directed": true}
            }]
        }
    }))
    .unwrap();
    let vm = project_field(
        "field:portfolio",
        "Portfolio",
        "view:fixture/portfolio",
        &binding,
        &[FieldSourceInput {
            channel: "execution",
            source_ref: "service:fixture/field/execution",
            subject_fingerprint: Some("thread:portfolio"),
            response: Some(&fixture),
            parsed: Some(&parsed_result),
            error: None,
            refreshing: false,
        }],
        None,
    );
    let admitted = vm
        .relations
        .iter()
        .filter(|relation| relation.kind == "admitted")
        .collect::<Vec<_>>();
    assert_eq!(
        admitted.len(),
        1,
        "the compound key must select one candidate"
    );
    assert!(admitted[0].source_id.starts_with("hook-observation:"));
    assert_eq!(
        admitted[0].target_id,
        "thread:T-a13f49f1-8e89-502c-9875-416156215693"
    );
}

#[test]
fn performance_fixture_shape_is_exact_and_deterministic() {
    let entities = (0..1_000)
        .map(|index| json!({ "id": format!("entity:{index}"), "kind": "work_unit" }))
        .collect::<Vec<_>>();
    let relations = (0..3_000)
        .map(|index| {
            json!({
                "id": format!("relation:{index}"),
                "kind": "depends_on",
                "source_id": format!("entity:{}", index % 1_000),
                "target_id": format!("entity:{}", (index + 1) % 1_000),
                "directed": true
            })
        })
        .collect::<Vec<_>>();
    let fixture = json!({
        "schema_version": "ryeos.ui.field.facts.v1",
        "source": "performance",
        "subject": { "kind": "fixture", "id": "generic-scale-v1" },
        "revision": "deterministic-v1",
        "cursor": { "mode": "live" },
        "truncated": false,
        "entities": entities,
        "relations": relations
    });

    assert_eq!(fixture["entities"].as_array().unwrap().len(), 1_000);
    assert_eq!(fixture["relations"].as_array().unwrap().len(), 3_000);
    assert_eq!(fixture["relations"][2_999]["source_id"], "entity:999");
    assert_eq!(fixture["relations"][2_999]["target_id"], "entity:0");
}

fn executable_performance_fixture() -> Value {
    let entities = (0..1_000)
        .map(|index| {
            json!({
                "id": format!("entity:{index}"),
                "kind": "work_unit",
                "label": format!("Work {index}"),
                "status": "ready",
                "attributes": {"rank": index / 80, "lane": index % 8},
                "provenance": {
                    "source_ref": "service:fixture/performance",
                    "source_revision": "deterministic-v1",
                    "evidence": [{"fixture": "generic-scale-v1"}]
                }
            })
        })
        .collect::<Vec<_>>();
    let relations = (0..3_000)
        .map(|index| {
            json!({
                "id": format!("relation:{index}"),
                "kind": "depends_on",
                "source_id": format!("entity:{}", index % 1_000),
                "target_id": format!("entity:{}", (index + 1 + index / 1_000) % 1_000),
                "directed": true,
                "attributes": {},
                "provenance": {
                    "source_ref": "service:fixture/performance",
                    "source_revision": "deterministic-v1",
                    "evidence": [{"fixture": "generic-scale-v1"}]
                }
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema_version": FIELD_FACTS_SCHEMA,
        "source": "performance",
        "subject": {"kind": "fixture", "id": "generic-scale-v1"},
        "revision": "deterministic-v1",
        "cursor": {"mode": "live"},
        "truncated": false,
        "entities": entities,
        "relations": relations,
        "previews": [], "metrics": [], "expansions": [], "warnings": []
    })
}

fn performance_binding() -> ViewBinding {
    serde_json::from_value(json!({
        "widget": "field",
        "sources": {"performance": {"ref": "service:fixture/performance"}},
        "projections": {
            "schema_version": FIELD_PROJECTION_SCHEMA,
            "groups": [{"id": "work", "label": "Work", "layout": "lanes"}],
            "layers": [{"id": "live", "label": "Live"}],
            "entity_rules": [{
                "match": {"kind": "work_unit"},
                "set": {
                    "group": "work", "layer": "live", "rank": "{attributes.rank}",
                    "lane": "{attributes.lane}", "shape": "rect"
                }
            }]
        }
    }))
    .unwrap()
}

#[test]
fn parse_projection_and_vm_serialization_scale_gate() {
    let fixture = executable_performance_fixture();
    let binding = performance_binding();
    let iterations = if cfg!(debug_assertions) { 1 } else { 50 };
    let mut samples = Vec::with_capacity(iterations);
    let mut serialized_bytes = 0;
    for _ in 0..iterations {
        let started = Instant::now();
        let parsed = parse_field_document(&fixture).expect("fixed performance facts parse");
        let parsed_result = Ok(parsed);
        let vm = project_field(
            "field:performance",
            "Performance",
            "view:fixture/performance",
            &binding,
            &[FieldSourceInput {
                channel: "performance",
                source_ref: "service:fixture/performance",
                subject_fingerprint: Some("fixture:generic-scale-v1"),
                response: Some(&fixture),
                parsed: Some(&parsed_result),
                error: None,
                refreshing: false,
            }],
            None,
        );
        let encoded = serde_json::to_vec(&vm).expect("serialize performance field VM");
        samples.push(started.elapsed());
        serialized_bytes = encoded.len();
        assert_eq!(vm.entities.len(), 1_000);
        assert_eq!(vm.relations.len(), 3_000);
    }
    assert!(serialized_bytes > 0);
    samples.sort();
    let p95 = samples[(samples.len() * 95).div_ceil(100).saturating_sub(1)];
    if !cfg!(debug_assertions) {
        assert!(
            p95.as_millis() <= 100,
            "facts parse + projection + VM serialization p95 was {p95:?} (limit 100ms)"
        );
    }
}

#[test]
fn client_large_closure_cap_fails_closed_at_entity_boundary() {
    let mut fixture = executable_performance_fixture();
    fixture["relations"] = json!([]);
    let prototype = fixture["entities"][0].clone();
    let entities = fixture["entities"].as_array_mut().unwrap();
    while entities.len() <= 5_000 {
        let index = entities.len();
        let mut entity = prototype.clone();
        entity["id"] = json!(format!("overflow:{index}"));
        entity["label"] = json!(format!("Overflow {index}"));
        entities.push(entity);
    }
    assert!(
        parse_field_document(&fixture)
            .expect_err("5,001 entities must cross the client count boundary")
            .contains("count limits")
    );
}
