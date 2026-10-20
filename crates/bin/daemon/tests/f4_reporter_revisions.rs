//! Reporter revisions over one retained doctor result, on one native set.
//!
//! An authenticated Core-only node runs `tool:ryeos/core/node/doctor` against
//! a fixed malformed manifest and retains the result. The three signed
//! reporter revisions (`report`, `report_r1`, `report_r2`) are then admitted
//! and run, each bound to the doctor result by thread id, tool ref and the
//! sha256 of the canonical result JSON. The complete native artifact set is
//! recorded before the first run and compared after every run; only tool
//! source changes between revisions, so the digest must never move.
//!
//! Negative controls: a tampered and an unsigned copy of a reporter are
//! refused at admission, a wrong doctor identity is refused by the reporter by
//! name, and checks the doctor could not run stay unavailable in r1.
//!
//! The host interpreter path, hash, version and `RYE_PYTHON` are printed so
//! the evidence record can pin them.

use ryeosd::test_support as common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::DaemonHarness;
use serde_json::{Value, json};

const DOCTOR_REF: &str = "tool:ryeos/core/node/doctor";
const REPORT_REFS: [(&str, &str); 3] = [
    ("r0", "tool:ryeos/core/reporter/report"),
    ("r1", "tool:ryeos/core/reporter/report_r1"),
    ("r2", "tool:ryeos/core/reporter/report_r2"),
];
const MALFORMED_MANIFEST: &str =
    "name: scheduler-doctor-probe\nversion: \"0.1.0\"\nbogus_field: true\n";

/// The reporter's canonical JSON: keys sorted recursively, compact
/// separators, non-ASCII left unescaped (Python
/// `json.dumps(sort_keys=True, separators=(",", ":"), ensure_ascii=False)`).
fn canonical(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(k, v)| (k.clone(), sorted(v)))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    // Insertion order is preserved only when serde_json keeps it; the map is
    // rebuilt in sorted order, and a map that orders by key yields the same.
    serde_json::to_string(&sorted(value)).expect("serialize canonical JSON")
}

/// The refusal evidence must name the signature or trust failure, which is
/// what distinguishes a tampered or unsigned copy from any other failure.
fn assert_names_trust_failure(name: &str, evidence: &str) {
    const MARKERS: [&str; 6] = [
        "signature missing",
        "signature verification failed",
        "content hash mismatch",
        "untrusted signer",
        "is not trusted",
        "no verified signer fingerprint",
    ];
    let lowered = evidence.to_lowercase();
    assert!(
        MARKERS.iter().any(|marker| lowered.contains(marker)),
        "{name}: refusal does not name a signature or trust failure: {evidence}"
    );
}

fn sha256_of(value: &Value) -> String {
    lillux::cas::sha256_hex(canonical(value).as_bytes())
}

/// Run one item to its terminal state through `/execute` in wait mode and
/// return the HTTP status with the envelope (`thread`, `result`). A Core-only
/// node does not install the thread-reading services, so the result is read
/// from the wait envelope the executing request itself returns.
async fn execute_wait(
    h: &DaemonHarness,
    item_ref: &str,
    project_path: Option<&Path>,
    params: Value,
) -> (reqwest::StatusCode, Value) {
    use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
    let policy = match project_path {
        Some(_) => ExecutionPolicy::local_live(ExecutionResponse::Wait),
        None => ExecutionPolicy::projectless(ExecutionResponse::Wait),
    };
    h.post_json(
        "/execute",
        json!({
            "item_ref": item_ref,
            "ref_bindings": {},
            "project_path": project_path.map(|p| p.display().to_string()),
            "parameters": params,
            "execution_policy": serde_json::to_value(policy).expect("serialize execution policy"),
        }),
    )
    .await
    .expect("post /execute")
}

fn is_completed(envelope: &Value) -> bool {
    envelope["thread"]["status"] == "completed"
}

/// Execute to completion; the retained tool result and the thread id.
async fn run_to_result(
    h: &DaemonHarness,
    item_ref: &str,
    project_path: Option<&Path>,
    params: Value,
) -> (String, Value) {
    let (status, envelope) = execute_wait(h, item_ref, project_path, params).await;
    assert!(
        status.is_success(),
        "{item_ref} was not admitted: {status} {envelope}"
    );
    assert!(
        is_completed(&envelope),
        "{item_ref} did not complete: {envelope}"
    );
    let thread_id = envelope["thread"]["thread_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{item_ref}: envelope without thread id: {envelope}"))
        .to_string();
    // The wait envelope's `result` is the subprocess outcome (`outcome_code`,
    // `error`, `artifacts`); the tool's own result is its `result` member.
    assert!(
        envelope["result"]["error"].is_null(),
        "{item_ref} reported a subprocess error: {envelope}"
    );
    let result = envelope["result"]["result"].clone();
    assert!(
        result.is_object(),
        "{item_ref} retained no result object: {envelope}"
    );
    (thread_id, result)
}

/// Execute expecting refusal: either rejected at admission, or admitted and
/// terminal without completing. Returns the full evidence text.
async fn run_expecting_refusal(
    h: &DaemonHarness,
    item_ref: &str,
    project_path: Option<&Path>,
    params: Value,
) -> String {
    let (status, envelope) = execute_wait(h, item_ref, project_path, params).await;
    if !status.is_success() {
        return envelope.to_string();
    }
    assert!(
        !is_completed(&envelope),
        "{item_ref} must be refused but completed: {envelope}"
    );
    envelope.to_string()
}

fn path_python() -> PathBuf {
    if let Some(explicit) = std::env::var_os("RYE_PYTHON").filter(|v| !v.is_empty()) {
        return PathBuf::from(explicit);
    }
    let path = std::env::var_os("PATH").expect("PATH must be set to resolve python3");
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("python3 must resolve on PATH when RYE_PYTHON is unset")
}

struct NativeSet {
    interpreter: PathBuf,
    script: PathBuf,
    core_root: PathBuf,
    binaries: Vec<(String, PathBuf)>,
}

impl NativeSet {
    fn record(&self) -> Value {
        let mut command = Command::new(&self.interpreter);
        command
            .arg("-I")
            .arg("-B")
            .arg(&self.script)
            .arg("--root")
            .arg(&self.core_root)
            .arg("--interpreter")
            .arg(&self.interpreter);
        for (label, path) in &self.binaries {
            command
                .arg("--binary")
                .arg(format!("{label}={}", path.display()));
        }
        let output = command.output().expect("run native-set inspector");
        assert!(
            output.status.success(),
            "native-set inspector failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("inspector emits one JSON record")
    }

    fn assert_unchanged(&self, baseline: &Value, after: &str) {
        let current = self.record();
        assert_eq!(
            current["digest"], baseline["digest"],
            "native set digest moved after {after}"
        );
        assert_eq!(&current, baseline, "native set record moved after {after}");
        eprintln!(
            "[f4] native set unchanged after {after}: {}",
            current["digest"]
        );
    }
}

fn report_sealed_correctly(report: &Value) {
    let claimed = report["report_sha256"]
        .as_str()
        .expect("report carries report_sha256")
        .to_string();
    let mut body = report.clone();
    body.as_object_mut().unwrap().remove("report_sha256");
    assert_eq!(
        sha256_of(&body),
        claimed,
        "report seal does not match its content"
    );
}

fn check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|c| c["name"] == name))
        .unwrap_or_else(|| panic!("report has no check {name}: {report}"))
}

fn string_matching<'a>(value: &'a Value, wanted: &str) -> Option<&'a str> {
    match value {
        Value::String(s) if s.contains(wanted) => Some(s),
        Value::Array(items) => items.iter().find_map(|v| string_matching(v, wanted)),
        Value::Object(map) => map.values().find_map(|v| string_matching(v, wanted)),
        _ => None,
    }
}

fn reporter_source(name: &str) -> String {
    let path = common::workspace_root()
        .join("bundles/core/.ai/tools/ryeos/core/reporter")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[tokio::test(flavor = "multi_thread")]
async fn reporter_revisions_run_on_one_unchanged_native_set() {
    let (mut h, fixture) = DaemonHarness::start_fast_core_only_prepared()
        .await
        .expect("start authenticated Core node from prepared bundles");
    h.retain_evidence_on_drop(true);

    // Interpreter identity for the evidence record.
    let interpreter = path_python();
    eprintln!("[f4] RYE_PYTHON={:?}", std::env::var("RYE_PYTHON").ok());
    let native = NativeSet {
        interpreter: interpreter.clone(),
        script: common::workspace_root().join("scripts/release/inspect-core-native-set.py"),
        core_root: h.state_path.join(".ai/bundles/core"),
        binaries: vec![
            ("daemon".to_string(), common::ryeosd_binary()),
            ("cli".to_string(), common::ryeos_binary()),
        ],
    };
    let baseline = native.record();
    eprintln!(
        "[f4] interpreter path={} record={}",
        interpreter.display(),
        baseline["interpreter"]
    );
    eprintln!("[f4] native set digest (baseline)={}", baseline["digest"]);
    assert!(
        baseline["interpreter"].is_object(),
        "inspector must record the interpreter: {baseline}"
    );

    // Fixed malformed doctor subject inside a live project. The node runs the
    // doctor in its sandbox, which grants a tool its project and the bundle
    // roots and nothing else, so a subject elsewhere on the host is invisible
    // to it. The subject is a directory of the project, not the project root,
    // so the project's own `.ai` stays empty and valid.
    let project = h.user_space.path().join("f4-doctor-project");
    std::fs::create_dir_all(project.join(".ai")).expect("create doctor project");
    let project = project.canonicalize().expect("resolve doctor project");
    let subject = project.join("f4-doctor-subject");
    let subject_ai = subject.join(".ai");
    std::fs::create_dir_all(&subject_ai).expect("create doctor subject");
    std::fs::write(subject_ai.join("manifest.source.yaml"), MALFORMED_MANIFEST)
        .expect("write malformed manifest");
    // The admitted plan refuses a parameter that embeds the project root, so
    // the subject is named relative to the project the doctor runs in.
    let doctor_params = json!({
        "source": "f4-doctor-subject",
        "registry_roots": [native.core_root.display().to_string()],
    });

    // 1. Doctor on the Core node, result retained.
    let (doctor_thread, doctor_result) =
        run_to_result(&h, DOCTOR_REF, Some(&project), doctor_params).await;
    assert_eq!(doctor_result["ok"], false, "doctor: {doctor_result}");
    assert!(
        doctor_result["source"]
            .as_str()
            .is_some_and(|source| source.ends_with("f4-doctor-subject")),
        "doctor must report the subject it inspected: {doctor_result}"
    );
    let manifest = check(&doctor_result, "manifest");
    assert_eq!(manifest["status"], "fail");
    assert!(
        manifest["detail"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("bogus_field")),
        "manifest finding must name bogus_field: {manifest}"
    );
    let identity = json!({
        "thread_id": doctor_thread,
        "tool_ref": DOCTOR_REF,
        "result_sha256": sha256_of(&doctor_result),
    });
    eprintln!("[f4] doctor identity={identity}");
    native.assert_unchanged(&baseline, "doctor");

    // The reporters run on the Python function runtime, whose launch binds a
    // project path. They get their own empty project so the doctor's subject
    // path, which their parameters carry, is not the root they run in.
    let reporter_project = h.user_space.path().join("f4-reporter-project");
    std::fs::create_dir_all(reporter_project.join(".ai")).expect("create reporter project");
    let reporter_project = reporter_project
        .canonicalize()
        .expect("resolve reporter project");

    // 2. Reporter revisions, each against the same native set.
    let mut r0 = None;
    let mut r1 = None;
    for (revision, reporter_ref) in REPORT_REFS {
        let mut params = json!({
            "doctor_identity": identity,
            "doctor_result": doctor_result,
        });
        if revision == "r2" {
            params["prior_report"] = r1.clone().expect("r1 report precedes r2");
        }
        let (thread, report) =
            run_to_result(&h, reporter_ref, Some(&reporter_project), params).await;
        eprintln!(
            "[f4] {revision} thread={thread} report_sha256={}",
            report["report_sha256"]
        );
        assert_eq!(report["schema"], "ryeos.doctor_report.v1");
        assert_eq!(report["revision"], revision);
        assert_eq!(report["reporter"], reporter_ref);
        assert_eq!(report["doctor_ok"], false);
        assert_eq!(report["provenance"]["doctor_identity"], identity);
        assert_eq!(check(&report, "manifest")["status"], "fail");
        report_sealed_correctly(&report);
        match revision {
            "r0" => r0 = Some(report),
            "r1" => {
                assert_eq!(report["verdict"], "findings_present");
                assert_eq!(check(&report, "manifest")["classification"], "finding");
                for c in doctor_result["checks"].as_array().unwrap() {
                    let classification =
                        check(&report, c["name"].as_str().unwrap())["classification"]
                            .as_str()
                            .unwrap()
                            .to_string();
                    match c["status"].as_str().unwrap() {
                        "ok" => assert_eq!(classification, "passed"),
                        "fail" => assert_eq!(classification, "finding"),
                        _ => assert_ne!(
                            classification, "passed",
                            "non-ok doctor check {} became success",
                            c["name"]
                        ),
                    }
                    if c["detail"]["import_check"] == "unavailable" {
                        assert_eq!(classification, "unavailable", "check {}", c["name"]);
                    }
                }
                r1 = Some(report);
            }
            _ => {
                let comparison = &report["comparison"];
                assert_eq!(comparison["same_doctor_result"], true);
                assert_eq!(
                    comparison["prior_report_sha256"],
                    r1.as_ref().unwrap()["report_sha256"]
                );
                assert_eq!(comparison["added"], json!([]));
                assert_eq!(comparison["removed"], json!([]));
                assert_eq!(comparison["status_changed"], json!([]));
            }
        }
        native.assert_unchanged(&baseline, revision);
    }
    assert!(r0.is_some());

    // 3. Unavailable stays unavailable and is never success in r1: a doctor
    // result whose only non-passing check is an unavailable one.
    let synthetic = json!({
        "source": doctor_result["source"],
        "ok": true,
        "checks": [
            {"name": "alpha", "status": "ok", "detail": null},
            {"name": "beta", "status": "n/a", "detail": {
                "import_check": "unavailable",
                "import_check_reason": "interpreter dependency not importable"
            }}
        ],
    });
    let synthetic_identity = json!({
        "thread_id": doctor_thread,
        "tool_ref": DOCTOR_REF,
        "result_sha256": sha256_of(&synthetic),
    });
    let (_, unavailable) = run_to_result(
        &h,
        REPORT_REFS[1].1,
        Some(&reporter_project),
        json!({"doctor_identity": synthetic_identity, "doctor_result": synthetic}),
    )
    .await;
    assert_eq!(check(&unavailable, "beta")["classification"], "unavailable");
    assert_eq!(check(&unavailable, "alpha")["classification"], "passed");
    assert_eq!(unavailable["verdict"], "incomplete");
    assert_ne!(unavailable["verdict"], "clean");
    assert_eq!(unavailable["groups"]["unavailable"], json!(["beta"]));
    native.assert_unchanged(&baseline, "unavailable control");

    // 4. Wrong doctor identity is refused by name by every revision.
    let mut wrong = identity.clone();
    wrong["result_sha256"] = json!("0".repeat(64));
    for (revision, reporter_ref) in REPORT_REFS {
        let mut params = json!({
            "doctor_identity": wrong,
            "doctor_result": doctor_result,
        });
        if revision == "r2" {
            params["prior_report"] = r1.clone().unwrap();
        }
        let text = run_expecting_refusal(&h, reporter_ref, Some(&reporter_project), params).await;
        let found = string_matching(
            &serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
            "doctor_identity_mismatch",
        )
        .map(str::to_owned)
        .or_else(|| {
            text.contains("doctor_identity_mismatch")
                .then(|| text.clone())
        });
        let found = found.unwrap_or_else(|| {
            panic!("{revision}: refusal does not name doctor_identity_mismatch: {text}")
        });
        eprintln!("[f4] {revision} wrong-identity refusal: {found}");
    }
    native.assert_unchanged(&baseline, "wrong identity controls");

    // 5. Tampered and unsigned reporter copies are refused at admission; the
    // correctly signed copy under the same ref shape is admitted, so the
    // refusals are attributable to the signature alone.
    // Each copy gets its own project: a live project is admitted as a whole,
    // so a refused copy sharing a project with the signed one would refuse the
    // signed one as well and the positive control would prove nothing.
    let original = reporter_source("report.py");
    let unsigned_body: String = original
        .lines()
        .filter(|line| !line.starts_with("# ryeos:signed:"))
        .map(|line| format!("{line}\n"))
        .collect::<String>()
        .replace("category: ryeos/core/reporter", "category: f4/reporter");
    let signed = lillux::signature::sign_content(&unsigned_body, &fixture.publisher, "#", None);
    let tampered = signed.replacen("REVISION = \"r0\"", "REVISION = \"rX\"", 1);
    assert_ne!(tampered, signed, "tamper must change the signed bytes");
    let copy_project = |name: &str, bytes: &str| -> PathBuf {
        let project = h.user_space.path().join(format!("f4-{name}-project"));
        let tools = project.join(".ai/tools/f4/reporter");
        std::fs::create_dir_all(&tools).expect("create project tools");
        std::fs::write(
            project.join(".ai/manifest.source.yaml"),
            format!("name: f4-{name}-project\nversion: \"0.1.0\"\n"),
        )
        .expect("write project manifest");
        std::fs::write(tools.join(format!("report_{name}.py")), bytes).expect("write copy");
        project.canonicalize().expect("resolve copy project")
    };
    let copy_params = json!({"doctor_identity": identity, "doctor_result": doctor_result});

    let signed_project = copy_project("signed", &signed);
    let (_, positive) = run_to_result(
        &h,
        "tool:f4/reporter/report_signed",
        Some(&signed_project),
        copy_params.clone(),
    )
    .await;
    assert_eq!(
        positive["revision"], "r0",
        "signed copy must run: {positive}"
    );
    for (name, bytes) in [("tampered", &tampered), ("unsigned", &unsigned_body)] {
        let project = copy_project(name, bytes);
        let evidence = run_expecting_refusal(
            &h,
            &format!("tool:f4/reporter/report_{name}"),
            Some(&project),
            copy_params.clone(),
        )
        .await;
        assert_names_trust_failure(name, &evidence);
        eprintln!("[f4] {name} refused: {evidence}");
    }
    native.assert_unchanged(&baseline, "tamper controls");
    eprintln!("[f4] DONE native set digest={}", baseline["digest"]);
}
