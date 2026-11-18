//! The machines service and view over every group state, answer state and an
//! unknown reason key, drawn through the same table projection the clients
//! use.

use std::collections::{BTreeMap, BTreeSet};

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_client_base::ui::content::{
    ViewBinding, project_table, table_columns, table_hierarchy_rows, views_from_surface,
};
use ryeos_effect_contract::locality::{
    LocalityGroups, MachineDeclaration, MemberKind, NodeAnswer, NodeInput, NodeLocalityFacts,
    REASON_EMULATED_KERNEL, REASON_INBOUND_ONLY, REASON_INVALID_ANSWER, REASON_NOT_MEASURABLE,
    REASON_NOT_REPORTED, REASON_REMOTE_ERROR, REASON_REMOTE_INVALID, REASON_SERVICE_NOT_INSTALLED,
    REASON_TIMEOUT, REASON_UNREACHABLE, REASON_USER_NAMESPACE, RelationFact, compare_locality,
};
use ryeos_ui::handlers::ui_machines::{
    MachinesWords, NODE_CAPABILITY, machines_response, require_node_capability,
};
use serde_json::{Value, json};

const AS_OF: &str = "2026-11-18T01:45:00.123Z";
const VIEW_REF: &str = "view:ryeos/node/machines";

fn bundle_file(relative: &str) -> String {
    let path = ryeos_engine::test_support::workspace_root()
        .join("bundles/ryeos-ui/.ai")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn words() -> MachinesWords {
    MachinesWords::from_value(
        serde_yaml::from_str(&bundle_file("config/ryeos/ui/machines.yaml")).unwrap(),
    )
    .unwrap()
}

fn binding() -> ViewBinding {
    let raw: Value = serde_yaml::from_str(&bundle_file("views/ryeos/node/machines.yaml")).unwrap();
    let mut parsed = views_from_surface(Some(&json!({ "views": { VIEW_REF: raw } })));
    let binding = parsed.remove(VIEW_REF).expect("the view parses");
    assert_eq!(binding.degraded, None);
    binding
}

fn digest(seed: char) -> String {
    seed.to_string().repeat(64)
}

fn measured(seed: char) -> RelationFact {
    RelationFact::Measured {
        digest: digest(seed),
    }
}

fn facts(relations: &[(&str, RelationFact)]) -> NodeLocalityFacts {
    NodeLocalityFacts::new(
        relations
            .iter()
            .map(|(name, fact)| ((*name).to_owned(), fact.clone()))
            .collect(),
    )
}

fn answered(relations: &[(&str, RelationFact)]) -> Option<NodeAnswer> {
    Some(NodeAnswer::Answered {
        facts: facts(relations),
        as_of: AS_OF.to_owned(),
    })
}

fn node(id: &str, name: &str, kind: MemberKind, answer: Option<NodeAnswer>) -> NodeInput {
    NodeInput {
        id: id.to_owned(),
        name: name.to_owned(),
        kind,
        answer,
    }
}

/// A boot, with the storage and account that only mean something inside it.
fn host(seed: char) -> Vec<(&'static str, RelationFact)> {
    vec![
        ("boot", measured(seed)),
        ("storage", measured(seed)),
        ("account", measured(seed)),
    ]
}

fn declare(label: &str, members: &[&str]) -> MachineDeclaration {
    MachineDeclaration {
        label: label.to_owned(),
        members: members.iter().map(|id| (*id).to_owned()).collect(),
    }
}

fn fixture_groups() -> LocalityGroups {
    let mut desk_this = host('a');
    desk_this.push(("rack", measured('r')));
    let mut desk_other = host('a');
    desk_other.push(("rack", measured('r')));
    let unknown_everywhere = [
        (
            "boot",
            RelationFact::Unknown {
                reason: REASON_EMULATED_KERNEL.to_owned(),
            },
        ),
        (
            "storage",
            RelationFact::Unknown {
                reason: REASON_NOT_MEASURABLE.to_owned(),
            },
        ),
    ];
    let nodes = vec![
        node(
            "site:this",
            "laptop",
            MemberKind::This,
            answered(&desk_this),
        ),
        node(
            "site:b",
            "desktop",
            MemberKind::Remote,
            answered(&desk_other),
        ),
        node(
            "site:c",
            "lab-one",
            MemberKind::Remote,
            answered(&host('c')),
        ),
        node(
            "site:d",
            "lab-two",
            MemberKind::Remote,
            answered(&host('d')),
        ),
        node(
            "site:e",
            "pair-one",
            MemberKind::Remote,
            answered(&host('e')),
        ),
        node(
            "site:f",
            "pair-two",
            MemberKind::Remote,
            Some(NodeAnswer::DidNotAnswer {
                reason: REASON_TIMEOUT.to_owned(),
                as_of: AS_OF.to_owned(),
            }),
        ),
        node(
            "site:g",
            "seen-one",
            MemberKind::Remote,
            answered(&host('g')),
        ),
        node(
            "site:h",
            "seen-two",
            MemberKind::Remote,
            answered(&host('g')),
        ),
        node("site:i", "solo", MemberKind::Remote, answered(&host('i'))),
        node(
            "site:j",
            "sandbox",
            MemberKind::Remote,
            answered(&unknown_everywhere),
        ),
        node(
            "site:k",
            "closed",
            MemberKind::Remote,
            Some(NodeAnswer::NotGranted {
                as_of: AS_OF.to_owned(),
            }),
        ),
        node(
            "site:l",
            "odd",
            MemberKind::Remote,
            Some(NodeAnswer::DidNotAnswer {
                reason: "weird_reason".to_owned(),
                as_of: AS_OF.to_owned(),
            }),
        ),
        node("svc-m", "svc-m", MemberKind::InstalledNotConnected, None),
    ];
    let declarations = [
        declare("desk", &["site:this", "site:b"]),
        declare("lab", &["site:c", "site:d"]),
        declare("pair", &["site:e", "site:f", "site:gone"]),
    ];
    compare_locality(AS_OF, &nodes, &declarations).unwrap()
}

struct Drawn {
    response: Value,
    cells: BTreeMap<String, Vec<String>>,
    tones: BTreeMap<String, Option<String>>,
    cell_tones: BTreeMap<String, Vec<Option<String>>>,
    columns: Vec<String>,
}

fn draw(groups: &LocalityGroups) -> Drawn {
    let binding = binding();
    let response = machines_response(groups, &words());
    let columns = table_columns(&binding);
    let rows = project_table(&binding, &response, &columns);
    let mut cells = BTreeMap::new();
    let mut tones = BTreeMap::new();
    let mut cell_tones = BTreeMap::new();
    for row in rows {
        let id = row.raw["id"].as_str().unwrap().to_owned();
        tones.insert(id.clone(), row.tone);
        cell_tones.insert(id.clone(), row.cell_tones);
        cells.insert(id, row.cells);
    }
    Drawn {
        response,
        cells,
        tones,
        cell_tones,
        columns: columns.into_iter().map(|column| column.label).collect(),
    }
}

impl Drawn {
    fn cell(&self, row: &str, column: &str) -> &str {
        let index = self.columns.iter().position(|c| c == column).unwrap();
        &self.cells[row][index]
    }

    fn tone(&self, row: &str) -> Option<&str> {
        self.tones[row].as_deref()
    }

    fn cell_tone(&self, row: &str, column: &str) -> Option<&str> {
        let index = self.columns.iter().position(|c| c == column).unwrap();
        self.cell_tones[row][index].as_deref()
    }
}

#[test]
fn every_group_state_draws_its_words_and_tone() {
    let drawn = draw(&fixture_groups());
    let cases = [
        ("machine:desk", "Named by you, confirmed this boot", "good"),
        (
            "machine:lab",
            "Named by you, but these nodes report different boots",
            "danger",
        ),
        ("machine:pair", "Named by you", "neutral"),
        ("node:site:g", "Same boot", "good"),
        ("node:site:i", "On its own", "neutral"),
        ("node:site:j", "Cannot tell", "warn"),
    ];
    for (key, text, tone) in cases {
        assert_eq!(drawn.cell(key, "state"), text, "{key}");
        assert_eq!(drawn.tone(key), Some(tone), "{key}");
    }
    // The raw state key rides on the row for anything keyed on it.
    let states: BTreeSet<&str> = drawn.response["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "group")
        .map(|row| row["state"].as_str().unwrap())
        .collect();
    assert_eq!(
        states,
        BTreeSet::from([
            "declared_confirmed",
            "declared_contradicted",
            "declared",
            "observed",
            "alone",
            "cannot_tell",
        ])
    );
}

#[test]
fn this_computer_is_marked_on_its_group_and_the_group_name_is_the_label() {
    let drawn = draw(&fixture_groups());
    assert_eq!(drawn.cell("machine:desk", "machine"), "desk");
    assert_eq!(drawn.cell("machine:desk", "place"), "This computer");
    assert_eq!(drawn.cell("machine:lab", "place"), "");
    assert_eq!(drawn.cell("machine:desk/site:this", "place"), "This node");
    assert_eq!(drawn.cell("machine:desk/site:b", "place"), "Remote");
    assert_eq!(
        drawn.cell("node:svc-m/svc-m", "place"),
        "Installed here, not connected"
    );
    // An unnamed group is named by the nodes in it.
    assert_eq!(drawn.cell("node:site:g", "machine"), "seen-one, seen-two");
}

#[test]
fn a_contradicted_machine_is_shown_with_who_disagrees() {
    let drawn = draw(&fixture_groups());
    for member in ["machine:lab/site:c", "machine:lab/site:d"] {
        assert_eq!(drawn.cell(member, "state"), "Reports a different boot");
        assert_eq!(drawn.tone(member), Some("danger"));
    }
    let lab = drawn.response["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "machine:lab")
        .unwrap();
    assert_eq!(
        lab["disagreeing_text"],
        "Reports a different boot: lab-one, lab-two"
    );
    assert_eq!(lab["disagreeing"], json!(["site:c", "site:d"]));
}

#[test]
fn a_declared_id_nobody_answered_for_is_named_not_hidden() {
    let drawn = draw(&fixture_groups());
    let pair = drawn.response["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "machine:pair")
        .unwrap();
    assert_eq!(pair["missing_text"], "Named but not seen: site:gone");
    assert_eq!(pair["declared_missing"], json!(["site:gone"]));
}

#[test]
fn every_answer_state_is_its_own_row_state_with_its_time() {
    let drawn = draw(&fixture_groups());
    let cases = [
        ("machine:desk/site:b", "Answered", None, "01:45 · 18 Nov"),
        (
            "machine:pair/site:f",
            "Did not answer: no answer in time",
            Some("warn"),
            "01:45 · 18 Nov",
        ),
        (
            "node:site:k/site:k",
            "Not granted",
            Some("warn"),
            "01:45 · 18 Nov",
        ),
        ("node:svc-m/svc-m", "Not asked", None, ""),
    ];
    for (row, text, tone, as_of) in cases {
        assert_eq!(drawn.cell(row, "state"), text, "{row}");
        assert_eq!(drawn.cell(row, "as of"), as_of, "{row}");
        if let Some(tone) = tone {
            assert_eq!(drawn.tone(row), Some(tone), "{row}");
        }
    }
    // Not granted, did not answer and cannot tell are three different rows.
    let status = |id: &str| {
        drawn.response["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()["status"]
            .clone()
    };
    assert_eq!(status("node:site:k/site:k"), "not_granted");
    assert_eq!(status("machine:pair/site:f"), "did_not_answer");
    assert_eq!(status("node:site:j"), "cannot_tell");
    assert_eq!(status("node:site:j/site:j"), "answered");
}

#[test]
fn an_unknown_reason_key_is_shown_as_itself() {
    let drawn = draw(&fixture_groups());
    assert_eq!(
        drawn.cell("node:site:l/site:l", "state"),
        "Did not answer: weird_reason"
    );
    assert_eq!(drawn.tone("node:site:l/site:l"), Some("warn"));
}

#[test]
fn relations_name_who_shares_and_what_cannot_be_told() {
    let drawn = draw(&fixture_groups());
    for column in ["boot", "storage", "account"] {
        assert_eq!(
            drawn.cell("machine:desk/site:this", column),
            "Shared with desktop",
            "{column}"
        );
        assert_eq!(
            drawn.cell("machine:desk/site:b", column),
            "Shared with laptop"
        );
        assert_eq!(drawn.cell_tone("machine:desk/site:b", column), Some("good"));
    }
    // Measured on its own: not shared with anyone.
    assert_eq!(
        drawn.cell("node:site:i/site:i", "boot"),
        "Measured, not shared"
    );
    // A relation this wording has no name for is carried, with its key.
    let this = drawn.response["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "machine:desk/site:this")
        .unwrap();
    assert!(
        this["facts_text"]
            .as_str()
            .unwrap()
            .contains("rack: Shared with desktop")
    );
    assert_eq!(this["facts"]["rack"]["grade"], "measured");
    // A node that could not tell says why, and is toned as such.
    assert_eq!(
        drawn.cell("node:site:j/site:j", "boot"),
        "Cannot tell: the kernel is emulated, so its boot id says nothing"
    );
    assert_eq!(drawn.cell_tone("node:site:j/site:j", "boot"), Some("warn"));
    assert_eq!(drawn.cell("node:site:j/site:j", "account"), "");
    // A node that never answered has no relation cells at all.
    assert_eq!(drawn.cell("node:site:k/site:k", "boot"), "");
}

#[test]
fn members_nest_under_their_group_in_the_table_hierarchy() {
    let binding = binding();
    let response = machines_response(&fixture_groups(), &words());
    let records = response["rows"].as_array().unwrap().clone();
    let rows = table_hierarchy_rows(&binding, &records, &BTreeSet::new()).unwrap();
    assert_eq!(rows.len(), records.len());
    for row in &rows {
        let record = &records[row.source_index];
        if record["kind"] == "group" {
            assert!(row.ancestor_continues.is_empty(), "{}", record["id"]);
            assert!(row.has_children);
        } else {
            assert_eq!(row.ancestor_continues.len(), 1, "{}", record["id"]);
            assert!(!row.has_children);
        }
    }
    // Pre-order: each group is followed by its members before the next group.
    let order: Vec<&str> = rows
        .iter()
        .map(|row| records[row.source_index]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(order[0], "group");
    let desk = rows
        .iter()
        .position(|row| records[row.source_index]["id"] == "machine:desk")
        .unwrap();
    assert_eq!(order[desk + 1], "member");
    assert_eq!(order[desk + 2], "member");
}

#[test]
fn row_ids_are_stable_and_unique() {
    let response = machines_response(&fixture_groups(), &words());
    let ids: Vec<&str> = response["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    let unique: BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len());
    assert!(ids.contains(&"machine:desk/site:this"));
}

#[test]
fn the_wording_covers_every_state_answer_kind_and_reason_the_comparison_names() {
    let words = words();
    let value =
        serde_yaml::from_str::<Value>(&bundle_file("config/ryeos/ui/machines.yaml")).unwrap();
    let keys = |section: &str| -> BTreeSet<String> {
        value[section]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    for (section, expected) in [
        (
            "states",
            vec![
                "declared_confirmed",
                "declared",
                "declared_contradicted",
                "observed",
                "alone",
                "cannot_tell",
            ],
        ),
        (
            "answers",
            vec![
                "answered",
                "did_not_answer",
                "not_granted",
                "not_asked",
                "disagrees",
            ],
        ),
        ("kinds", vec!["this", "remote", "installed_not_connected"]),
        ("relations", vec!["boot", "storage", "account"]),
        (
            "reasons",
            vec![
                REASON_EMULATED_KERNEL,
                REASON_NOT_MEASURABLE,
                REASON_USER_NAMESPACE,
                REASON_NOT_REPORTED,
                REASON_TIMEOUT,
                REASON_UNREACHABLE,
                REASON_SERVICE_NOT_INSTALLED,
                REASON_INBOUND_ONLY,
                REASON_REMOTE_INVALID,
                REASON_REMOTE_ERROR,
                REASON_INVALID_ANSWER,
            ],
        ),
    ] {
        let expected: BTreeSet<String> = expected.into_iter().map(str::to_owned).collect();
        assert_eq!(keys(section), expected, "{section}");
    }
    assert!(!words.this_computer.is_empty());
}

#[test]
fn wording_with_an_unknown_field_or_an_empty_text_is_refused() {
    let mut value: Value =
        serde_yaml::from_str(&bundle_file("config/ryeos/ui/machines.yaml")).unwrap();
    value["surprise"] = json!("x");
    assert!(MachinesWords::from_value(value).is_err());
    let mut value: Value =
        serde_yaml::from_str(&bundle_file("config/ryeos/ui/machines.yaml")).unwrap();
    value["states"]["alone"] = json!("");
    assert!(MachinesWords::from_value(value).is_err());
}

#[test]
fn the_view_offers_no_action() {
    let raw: Value = serde_yaml::from_str(&bundle_file("views/ryeos/node/machines.yaml")).unwrap();
    assert!(raw.get("affordances").is_none());
    assert!(raw.get("selection").is_none());
}

fn caller(scopes: &[&str]) -> HandlerContext {
    HandlerContext::new(
        "fp:caller".to_owned(),
        scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        true,
    )
}

#[test]
fn the_service_needs_what_the_node_service_needs() {
    assert!(require_node_capability(&caller(&[NODE_CAPABILITY])).is_ok());
    assert!(require_node_capability(&caller(&["ryeos.execute.service.*"])).is_ok());
    assert!(require_node_capability(&caller(&["ryeos.execute.service.node/*"])).is_ok());
    for scopes in [
        &[][..],
        &["ryeos.execute.service.ui/ryeos-ui/node/machines"][..],
        &["ryeos.execute.service.node/status"][..],
        &["ryeos.execute.service.node/locality"][..],
    ] {
        assert!(
            matches!(
                require_node_capability(&caller(scopes)),
                Err(HandlerError::Forbidden(_))
            ),
            "{scopes:?}"
        );
    }
}

#[test]
fn the_signed_service_and_the_node_service_name_the_same_capability() {
    assert_eq!(
        NODE_CAPABILITY,
        ryeos_api::handlers::node_locality_groups::CAPABILITY
    );
    assert_eq!(
        ryeos_ui::handlers::ui_machines::MACHINES_DESCRIPTOR.required_caps,
        &[NODE_CAPABILITY]
    );
}

#[test]
fn the_node_surface_admits_both_the_service_and_the_capability_it_needs() {
    let surface: Value = serde_yaml::from_str(&bundle_file("surfaces/ryeos/ui/node.yaml")).unwrap();
    let sources: Vec<&str> = surface["capabilities"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cap| cap.as_str().unwrap())
        .collect();
    assert!(sources.contains(&NODE_CAPABILITY));
    assert!(sources.contains(&"ryeos.execute.service.ui/ryeos-ui/node/machines"));
    let library = surface["library"].to_string();
    assert!(library.contains(VIEW_REF));
}
