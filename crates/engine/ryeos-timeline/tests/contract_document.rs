//! The timeline contract document says what the engine does. Its version line,
//! its entry-type table and its group-id table are compared with the code, so
//! a change to either side fails here until the other follows.

use std::path::PathBuf;

use std::collections::BTreeMap;

use ryeos_timeline::{
    CLUSTER_ID_PREFIX, Cluster, Entry, EntryKind, Item, RECAP_ID_PREFIX, Recap,
    TIMELINE_CONTRACT_VERSION,
};
use serde_json::Value;

fn read(relative: &str) -> String {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/ryeos-ui/.ai/knowledge/ryeos/ui")
            .join(relative),
    )
    .unwrap_or_else(|error| panic!("{relative} is part of the repository: {error}"))
}

/// The rows of the table under the heading `<heading>` (any level), as trimmed
/// cells, without the header row.
fn table(document: &str, heading: &str) -> Vec<Vec<String>> {
    let mut in_section = false;
    let mut rows = Vec::new();
    for line in document.lines() {
        if line.starts_with('#') {
            in_section = line.trim_start_matches('#').trim() == heading;
            continue;
        }
        if !in_section || !line.starts_with('|') {
            continue;
        }
        let cells: Vec<String> = line
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().trim_matches('`').to_string())
            .collect();
        let separator = cells
            .iter()
            .all(|cell| cell.chars().all(|c| c == '-' || c == ':'));
        if !separator {
            rows.push(cells);
        }
    }
    assert!(!rows.is_empty(), "no table under the heading {heading}");
    // The first row is the header.
    rows.split_off(1)
}

#[test]
fn the_document_names_the_contract_version_the_code_carries() {
    let document = read("timeline-v1.md");
    let line = document
        .lines()
        .find_map(|line| line.strip_prefix("Contract version: "))
        .expect("the document has a `Contract version:` line");
    assert_eq!(line.trim().trim_matches('`'), TIMELINE_CONTRACT_VERSION);
}

#[test]
fn the_entry_type_table_is_the_engines_closed_set() {
    let rows = table(&read("timeline-v1.md"), "Entry types");
    let documented: Vec<(String, String)> = rows
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    let in_code: Vec<(String, String)> = EntryKind::ALL
        .iter()
        .map(|kind| {
            let name = serde_json::to_value(kind)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            (name, kind.prefix().to_string())
        })
        .collect();
    assert_eq!(documented, in_code);
}

#[test]
fn the_group_id_table_names_the_engines_prefixes() {
    let rows = table(&read("timeline-v1.md"), "Group items");
    let documented: Vec<(String, String)> = rows
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    assert_eq!(
        documented,
        [
            ("cluster".to_string(), CLUSTER_ID_PREFIX.to_string()),
            ("recap".to_string(), RECAP_ID_PREFIX.to_string()),
        ]
    );
}

#[test]
fn the_frame_contract_references_this_timeline_contract() {
    let frame = read("frame-v2.md");
    assert!(
        frame.contains(TIMELINE_CONTRACT_VERSION),
        "the frame contract must name the timeline contract version it implements"
    );
}

fn entry() -> Entry {
    Entry {
        id: "act:1".to_string(),
        kind: EntryKind::Activity,
        state: Some("done".to_string()),
        tone: None,
        thread: "T-1".to_string(),
        parent: None,
        exchange: None,
        counts_as: None,
        text_streamed: true,
        slots: BTreeMap::new(),
        seq: (1, 2),
        ts: ("a".to_string(), "b".to_string()),
    }
}

fn documented_fields(heading: &str) -> Vec<String> {
    let mut fields: Vec<String> = table(&read("timeline-v1.md"), heading)
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    fields.sort();
    fields
}

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

#[test]
fn the_item_field_tables_are_the_fields_the_engine_serialises() {
    let entry_item = serde_json::to_value(Item::Entry(entry())).unwrap();
    assert_eq!(documented_fields("Entry item"), keys(&entry_item));

    let cluster = Item::Cluster(Cluster {
        id: "cl:act:1".to_string(),
        headline: "2 operations".to_string(),
        members: vec![entry()],
    });
    let cluster = serde_json::to_value(cluster).unwrap();
    assert_eq!(documented_fields("Cluster item"), keys(&cluster));
    // A cluster's members are bare entries; the `item` tag marks only the
    // items of a response window and of a recap.
    let member = &cluster["members"][0];
    let mut bare = keys(&entry_item);
    bare.retain(|key| key != "item");
    assert_eq!(keys(member), bare);

    let recap = Item::Recap(Recap {
        id: "rc:act:1".to_string(),
        exchange: "msg:1".to_string(),
        work: 1,
        duration_s: None,
        members: vec![Item::Entry(entry())],
        seq: 2,
    });
    let recap = serde_json::to_value(recap).unwrap();
    assert_eq!(documented_fields("Recap item"), keys(&recap));
    assert_eq!(recap["members"][0]["item"], "entry");
}
