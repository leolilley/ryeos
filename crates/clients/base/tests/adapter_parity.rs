//! The adapter parity ledger (`docs/adapter-parity.md`) records a checked
//! browser and terminal decision for every variant of the core's adapter
//! enums. A variant added or removed without a ledger row fails here.

use std::collections::BTreeSet;

const LEDGER: &str = include_str!("../docs/adapter-parity.md");
const EFFECT: &str = include_str!("../src/ui/effect.rs");
const EVENT: &str = include_str!("../src/ui/event.rs");
const VIEW_MODEL: &str = include_str!("../src/ui/view_model.rs");

const STATUSES: [&str; 5] = ["native", "shared", "presentation", "neutral", "gap"];

/// The variant names of `pub enum <name>` in `source`, at the enum's own
/// nesting depth (struct-variant fields are skipped).
fn variants(source: &str, name: &str) -> Vec<String> {
    let marker = format!("pub enum {name} ");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("missing Rust enum {name}"));
    let open = start + source[start..].find('{').expect("enum body");
    let mut depth = 0_i32;
    let mut close = open;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = open + offset;
                    break;
                }
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    let mut depth = 0_i32;
    for raw in source[open + 1..close].lines() {
        let line = raw.split("//").next().unwrap_or("");
        if depth == 0 {
            let trimmed = line.trim_start();
            let ident: String = trimmed
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            let rest = trimmed[ident.len()..].trim_start();
            if ident.starts_with(|ch: char| ch.is_ascii_uppercase())
                && (rest.starts_with('{') || rest.starts_with(',') || rest.starts_with('('))
            {
                found.push(ident);
            }
        }
        for ch in line.chars() {
            match ch {
                '{' | '(' => depth += 1,
                '}' | ')' => depth -= 1,
                _ => {}
            }
        }
    }
    found
}

/// The checked rows between `parity:<name>` markers, each with valid
/// browser and terminal statuses.
fn rows(name: &str) -> Vec<String> {
    let start = format!("<!-- parity:{name}:start -->");
    let end = format!("<!-- parity:{name}:end -->");
    let first = LEDGER
        .find(&start)
        .unwrap_or_else(|| panic!("missing ledger start for {name}"));
    let last = LEDGER
        .find(&end)
        .filter(|last| *last > first)
        .unwrap_or_else(|| panic!("missing ledger end for {name}"));
    let block = &LEDGER[first + start.len()..last];
    assert!(!block.contains("[ ]"), "{name} has an unchecked row");
    let mut found = Vec::new();
    for line in block.lines() {
        let Some(row) = line.strip_prefix("| [x] `") else {
            continue;
        };
        let (variant, rest) = row.split_once('`').expect("closed variant name");
        let cells: Vec<&str> = rest.split('|').map(str::trim).collect();
        assert!(
            STATUSES.contains(&cells[1]),
            "{name}.{variant} has invalid web status {:?}",
            cells[1]
        );
        assert!(
            STATUSES.contains(&cells[2]),
            "{name}.{variant} has invalid terminal status {:?}",
            cells[2]
        );
        found.push(variant.to_owned());
    }
    found
}

#[test]
fn every_adapter_enum_has_an_exact_checked_inventory() {
    for (name, source) in [
        ("RyeOsEffectKind", EFFECT),
        ("RyeOsEvent", EVENT),
        ("RyeOsUiIntent", EVENT),
        ("RyeOsUiEvent", EVENT),
        ("RyeOsInputAction", EVENT),
        ("RyeOsLayoutNodeVm", VIEW_MODEL),
        ("RyeOsViewVm", VIEW_MODEL),
    ] {
        let recorded = rows(name);
        let unique: BTreeSet<&String> = recorded.iter().collect();
        assert_eq!(unique.len(), recorded.len(), "{name} has duplicate rows");
        let actual: BTreeSet<String> = variants(source, name).into_iter().collect();
        assert!(!actual.is_empty(), "no variants read for {name}");
        let recorded: BTreeSet<String> = recorded.into_iter().collect();
        assert_eq!(recorded, actual, "{name}: ledger rows differ from the enum");
    }
}
