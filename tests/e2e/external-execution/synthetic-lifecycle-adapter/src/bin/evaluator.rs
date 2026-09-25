//! Bounded native checker using Lillux-owned startup and filesystem I/O.
//! Input authority is established by ordinary admission, not by these claims.
//! JSON is YAML-compatible; the signed Tool source contains evaluation_rule.
//! This module never resolves a Tool, executes C source, or claims isolation.

use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Component, Path},
};

const MAX_SOURCE: usize = 64 * 1024;
const MAX_REQUEST: usize = 4096;
const MAX_DATA: usize = 64 * 1024;
const MAX_EXPECTED: usize = 4096;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // No malformed request/source bytes are copied into diagnostics and no
        // successful evaluation testimony is emitted on invocation failure.
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

fn run() -> Result<()> {
    use lillux::invocation::{InvocationBounds, StartupInvocation};
    let bounds = InvocationBounds {
        max_arguments: 2,
        max_argument_bytes: 4096,
        max_total_argument_bytes: 8192,
        max_input_bytes: MAX_REQUEST,
        max_output_bytes: 4096,
    };
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
    // SAFETY: this single-threaded executable's admitted opaque launch supplies
    // uniquely inherited input/output pipes. No Rust stdio owner or task has
    // been created; the parent retains only the opposite pipe ends.
    let mut invocation =
        unsafe { StartupInvocation::take_inherited_pipes(0, 1, bounds, deadline) }?;
    ensure!(
        invocation.arguments().len() == 2,
        "expected only the admitted source entry"
    );
    let source_entry = std::path::PathBuf::from(&invocation.arguments()[1]);
    let request = invocation.read_input()?;
    let result = evaluate_pinned(&source_entry, &request)?;
    let mut output = serde_json::to_vec(&result)?;
    output.push(b'\n');
    invocation.write_output(&output)?;
    Ok(())
}

#[derive(Deserialize)]
struct Descriptor {
    evaluation_rule: Rule,
    // The normal resolver validates Tool metadata. This consumer interprets
    // only its finite nested rule, not executor/endpoint/source declarations.
    #[serde(flatten)]
    _metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    schema_version: u32,
    sample_path: String,
    expected_utf8: String,
    #[serde(default)]
    integration: Option<IntegrationRule>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct IntegrationRule {
    path: String,
    required_utf8_marker: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    base_snapshot_hash: String,
    candidate_snapshot_hash: String,
    #[serde(default)]
    expect_integration: bool,
}

fn relative_path(value: &str) -> Result<&Path> {
    ensure!(
        !value.is_empty()
            && value.len() <= 512
            && !value.contains('\\')
            && !value.chars().any(char::is_control),
        "invalid rule path"
    );
    ensure!(
        value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "noncanonical rule path"
    );
    let path = Path::new(value);
    ensure!(
        !path.is_absolute()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "rule path must be normalized and relative"
    );
    Ok(path)
}

fn parse_request(bytes: &[u8]) -> Result<Request> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_REQUEST,
        "request exceeds bound"
    );
    let request: Request =
        serde_json::from_slice(bytes).context("decode exact evaluation request")?;
    ensure!([&request.base_snapshot_hash, &request.candidate_snapshot_hash].into_iter()
        .all(|hash| lillux::valid_hash(hash) && !hash.bytes().any(|byte| byte.is_ascii_uppercase())),
        "invalid canonical snapshot hash");
    Ok(request)
}

fn parse_rule(source: &[u8]) -> Result<Rule> {
    ensure!(
        !source.is_empty() && source.len() <= MAX_SOURCE,
        "source exceeds bound"
    );
    let text = std::str::from_utf8(source).context("source is not UTF-8")?;
    // Signature verification belongs to source admission. Strip only the
    // recognized signature envelope before parsing the admitted JSON body.
    let body = lillux::signature::strip_signature_lines_with_envelope(text, "#", None);
    let descriptor: Descriptor =
        serde_json::from_str(&body).context("decode rule-bearing Tool source")?;
    let rule = descriptor.evaluation_rule;
    ensure!(rule.schema_version == 1, "unknown evaluation rule schema");
    relative_path(&rule.sample_path)?;
    ensure!(
        !rule.expected_utf8.is_empty() && rule.expected_utf8.len() <= MAX_EXPECTED,
        "expected sample exceeds bound"
    );
    if let Some(integration) = &rule.integration {
        relative_path(&integration.path)?;
        ensure!(
            !integration.required_utf8_marker.is_empty()
                && integration.required_utf8_marker.len() <= 256,
            "integration marker exceeds bound"
        );
    }
    Ok(rule)
}

fn decide(
    source: &[u8],
    request: &Request,
    rule: &Rule,
    sample: Option<&[u8]>,
    integration: Option<&[u8]>,
) -> Result<Value> {
    ensure!(
        sample.is_none_or(|bytes| bytes.len() <= MAX_DATA)
            && integration.is_none_or(|bytes| bytes.len() <= MAX_DATA),
        "candidate data exceeds bound"
    );
    ensure!(
        !request.expect_integration || rule.integration.is_some(),
        "request requires an integration rule not admitted by B"
    );
    let sample_matches = sample == Some(rule.expected_utf8.as_bytes());
    let integration_matches = if request.expect_integration {
        let marker = &rule
            .integration
            .as_ref()
            .context("integration rule absent")?
            .required_utf8_marker;
        integration
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .is_some_and(|text| text.contains(marker))
    } else {
        true
    };
    let accepted = request.candidate_snapshot_hash != request.base_snapshot_hash
        && sample_matches
        && integration_matches;
    Ok(json!({
        "schema_version":1,
        "base_snapshot_hash":request.base_snapshot_hash,
        "candidate_snapshot_hash":request.candidate_snapshot_hash,
        "accepted":accepted,
        "evidence":{
            "operation":"independent-test-evaluator",
            "fixture":"native-external-evaluator-body-v1",
            "source_rule_sha256":lillux::sha256_hex(source),
            "normalized_rule_sha256":lillux::sha256_hex(lillux::canonical_json(&serde_json::to_value(rule)?)?.as_bytes()),
            "sample_bytes_match":sample_matches,
            "sample_bytes":sample.map(|bytes| bytes.len()),
            "sample_sha256":sample.map(lillux::sha256_hex),
            "integration_required":request.expect_integration,
            "integration_matches":integration_matches
        }
    }))
}

fn read_candidate(directory: &lillux::PinnedDirectory, relative: &str) -> Result<Option<Vec<u8>>> {
    let Some(file) = directory.open_pinned_regular_descendant(relative_path(relative)?, false)?
    else {
        return Ok(None);
    };
    let observation = file.observation()?;
    Ok(Some(
        file.read_stable_bounded(&observation, MAX_DATA as u64)?,
    ))
}

/// Called only through the Lillux-owned executable entrypoint. The
/// argument is the actual admitted source.entry, not a requested project path.
/// C's files are data; no filename under C is ever used to select the rule.
fn evaluate_pinned(source_entry: &Path, request_bytes: &[u8]) -> Result<Value> {
    let source_root_path = Path::new(ryeos_state::objects::EXECUTION_RUNTIME_REALIZATIONS_ROOT);
    let relative = source_entry
        .strip_prefix(source_root_path)
        .context("source.entry is outside the admitted runtime source namespace")?;
    let relative = relative.to_str().context("source.entry is not UTF-8")?;
    let source_root =
        lillux::PinnedDirectory::open(source_root_path)?.context("source namespace absent")?;
    let source = source_root
        .open_pinned_regular_descendant(relative_path(relative)?, false)?
        .context("admitted source.entry absent")?;
    let observed = source.observation()?;
    let source_bytes = source.read_stable_bounded(&observed, MAX_SOURCE as u64)?;
    let request = parse_request(request_bytes)?;
    let rule = parse_rule(&source_bytes)?;
    let candidate = lillux::PinnedDirectory::open(Path::new("/workspace"))?
        .context("frozen candidate absent")?;
    let sample = read_candidate(&candidate, &rule.sample_path)?;
    let integration = if request.expect_integration {
        let rule = rule
            .integration
            .as_ref()
            .context("B supplied no integration rule")?;
        read_candidate(&candidate, &rule.path)?
    } else {
        None
    };
    decide(
        &source_bytes,
        &request,
        &rule,
        sample.as_deref(),
        integration.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "category":"fixtures/evaluation", "name":"run",
            "executor_id":"tool:fixtures/evaluation/runtime",
            "execution_protocol":"protocol:ryeos/core/opaque",
            "evaluation_rule":{"schema_version":1,"sample_path":"candidate-strategy.txt",
                "expected_utf8":"composed external candidate C\n",
                "integration":{"path":".ai/knowledge/test/external-candidate/integration.md",
                    "required_utf8_marker":"# Integration D"}}
        }))
        .unwrap()
    }
    fn request() -> Request {
        parse_request(
            &serde_json::to_vec(&json!({
                "base_snapshot_hash":"a".repeat(64),"candidate_snapshot_hash":"b".repeat(64)
            }))
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn exact_good_bad_and_unchanged_candidate() {
        let source = source();
        let rule = parse_rule(&source).unwrap();
        let mut request = request();
        let good = decide(
            &source,
            &request,
            &rule,
            Some(rule.expected_utf8.as_bytes()),
            None,
        )
        .unwrap();
        assert_eq!(good["accepted"], true);
        assert_eq!(
            good["evidence"]["source_rule_sha256"],
            lillux::sha256_hex(&source)
        );
        assert_eq!(
            good,
            decide(
                &source,
                &request,
                &rule,
                Some(rule.expected_utf8.as_bytes()),
                None
            )
            .unwrap()
        );
        for bytes in [None, Some(b"bad candidate".as_slice())] {
            assert_eq!(
                decide(&source, &request, &rule, bytes, None).unwrap()["accepted"],
                false
            );
        }
        request.candidate_snapshot_hash = request.base_snapshot_hash.clone();
        assert_eq!(
            decide(
                &source,
                &request,
                &rule,
                Some(rule.expected_utf8.as_bytes()),
                None
            )
            .unwrap()["accepted"],
            false
        );
    }

    #[test]
    fn integration_is_b_owned_and_optional() {
        let source = source();
        let rule = parse_rule(&source).unwrap();
        let mut request = request();
        request.expect_integration = true;
        assert_eq!(
            decide(
                &source,
                &request,
                &rule,
                Some(rule.expected_utf8.as_bytes()),
                None
            )
            .unwrap()["accepted"],
            false
        );
        assert_eq!(
            decide(
                &source,
                &request,
                &rule,
                Some(rule.expected_utf8.as_bytes()),
                Some(b"header\n# Integration D\n")
            )
            .unwrap()["accepted"],
            true
        );
    }

    #[test]
    fn rules_requests_and_paths_fail_closed() {
        for path in [
            "",
            "/workspace/a",
            "../a",
            "a/../b",
            "./a",
            "a//b",
            "a/",
            "a\\b",
        ] {
            assert!(relative_path(path).is_err(), "{path}");
        }
        for mutation in ["unknown", "schema_version", "sample_path", "expected_utf8"] {
            let mut value: Value = serde_json::from_slice(&source()).unwrap();
            value["evaluation_rule"][mutation] = match mutation {
                "schema_version" => json!(2),
                "sample_path" => json!("../candidate"),
                "expected_utf8" => json!("x".repeat(MAX_EXPECTED + 1)),
                _ => json!(true),
            };
            assert!(parse_rule(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        assert!(parse_rule(&vec![b' '; MAX_SOURCE + 1]).is_err());
        assert!(parse_request(&vec![b' '; MAX_REQUEST + 1]).is_err());
        assert!(
            decide(
                &source(),
                &request(),
                &parse_rule(&source()).unwrap(),
                Some(&vec![b'x'; MAX_DATA + 1]),
                None
            )
            .is_err()
        );
        let valid = serde_json::to_value(
            json!({"base_snapshot_hash":"a".repeat(64),"candidate_snapshot_hash":"b".repeat(64)}),
        )
        .unwrap();
        for (field, value) in [
            ("unknown", json!(true)),
            ("base_snapshot_hash", json!("bad")),
            ("base_snapshot_hash", json!("A".repeat(64))),
            ("expect_integration", json!("yes")),
        ] {
            let mut changed = valid.clone();
            changed[field] = value;
            assert!(parse_request(&serde_json::to_vec(&changed).unwrap()).is_err());
        }
        assert!(parse_request(br#"{"base_snapshot_hash":"a","base_snapshot_hash":"b","candidate_snapshot_hash":"c"}"#).is_err());
        let duplicate = format!(
            r#"{{"evaluation_rule":{},"evaluation_rule":{}}}"#,
            serde_json::to_value(parse_rule(&source()).unwrap()).unwrap(),
            serde_json::to_value(parse_rule(&source()).unwrap()).unwrap()
        );
        assert!(parse_rule(duplicate.as_bytes()).is_err());
    }
}
