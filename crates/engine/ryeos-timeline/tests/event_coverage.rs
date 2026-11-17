//! Every event type the state layer registers must be a deliberate choice in
//! the shipped thread timeline definition: drawn by a rule, or dropped by an
//! explicit `drop` rule. A new first-party event fails here until it is declared.

use std::path::PathBuf;

use ryeos_state::event_types;
use ryeos_timeline::Definition;

#[test]
fn every_registered_event_type_is_declared() {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/ryeos-ui/.ai/config/ryeos/ui/timeline/thread.yaml"),
    )
    .unwrap();
    let definition = Definition::from_yaml(&text).unwrap();
    let undeclared: Vec<&str> = event_types::ALL
        .iter()
        .copied()
        .filter(|event_type| {
            definition
                .events
                .get(*event_type)
                .is_none_or(|rules| rules.is_empty())
        })
        .collect();
    assert!(
        undeclared.is_empty(),
        "event types with no rule in the thread timeline definition (add a rule or an explicit drop): {undeclared:?}"
    );
}
