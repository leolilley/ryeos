//! Pins the `--validate` mode of the graph runtime binary: it parses and
//! statically analyzes a graph file and prints a JSON report, with no
//! launch envelope, callback, or daemon involved. This is the primitive
//! the `graph validate` command drives.

use std::process::Command;

fn run_validate(yaml: &str) -> serde_json::Value {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.yaml");
    std::fs::write(&path, yaml).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ryeos-graph-runtime"))
        .arg("--validate")
        .arg("--graph-path")
        .arg(&path)
        .output()
        .expect("run graph runtime --validate");

    assert!(
        output.status.success(),
        "validate exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("validate emits JSON")
}

#[test]
fn validate_reports_valid_graph() {
    let report = run_validate(
        r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#,
    );
    assert_eq!(report["valid"], true);
    assert_eq!(report["node_count"], 2);
    assert!(report["errors"].as_array().unwrap().is_empty());
}

#[test]
fn validate_reports_errors_for_broken_graph() {
    let report = run_validate(
        r#"
version: "1.0.0"
category: test
config:
  start: nope
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "results"
      collect: "results"
      action: {item_id: "tool:test/echo"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#,
    );
    assert_eq!(report["valid"], false);
    let errors = report["errors"].as_array().unwrap();
    assert!(
        errors.iter().any(|e| e.as_str().unwrap().contains("nope")),
        "expected missing-start error: {errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.as_str().unwrap().contains("both 'collect' and 'as'")),
        "expected collect==as error: {errors:?}"
    );
}

#[test]
fn validate_surfaces_warnings() {
    let report = run_validate(
        r#"
version: "1.0.0"
category: test
config:
  start: step1
  config_schema:
    type: object
    properties:
      known: {type: string}
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {a: "${inputs.missing}"}}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#,
    );
    assert_eq!(report["valid"], true);
    let warnings = report["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("missing")),
        "expected undeclared-input warning: {warnings:?}"
    );
}
