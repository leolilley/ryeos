//! Deterministic remote structured-session fixture. It receives provider
//! protocol only through the admitted connector and authors C only after the
//! exact turn request. Merely launching the remote occurrence cannot produce
//! a candidate.
//!
//! The `--qualification-probe` branch is a synthetic protocol response only:
//! it does not examine the subject or establish its asserted security claims.
//! Do not use it as independent evidence for public worker acceptance or runtime
//! activation. The production product remains gated on a real verifier.

use std::io::{BufRead as _, Write as _};

fn main() {
    if let Err(error) = run() {
        eprintln!("synthetic candidate runtime failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let arguments = std::env::args().collect::<Vec<_>>();
    if arguments.get(1).map(String::as_str) == Some("--qualification-probe") {
        anyhow::ensure!(
            arguments.len() == 3
                && arguments[2].len() == 64
                && arguments[2]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "qualification probe requires one canonical subject manifest digest"
        );
        serde_json::to_writer(
            std::io::stdout().lock(),
            &serde_json::json!({
                "schema":"ryeos.product_qualification_result.v1",
                "subject_manifest_hash":arguments[2],
                "claims":[
                    "bounded_candidate_capture",
                    "candidate_only_execution",
                    "controller_credential_exclusion",
                    "native_writer_exclusion",
                    "no_local_execution_fallback"
                ],
                "probe_evidence":{
                    "fixture":"synthetic_external_candidate_runtime_v1"
                }
            }),
        )?;
        return Ok(());
    }
    if std::env::var_os("RYEOS_SYNTHETIC_JOINED_PROVIDER_REQUIRED").is_none() {
        std::fs::write("candidate-strategy.txt", b"composed external candidate C\n")?;
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut session_started = false;
    for line in stdin.lock().lines() {
        let line = line?;
        let request: serde_json::Value = serde_json::from_str(&line)?;
        let id = request
            .get("id")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("remote request has no id"))?;
        let method = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("remote request has no method"))?;
        let result = match method {
            "session/start" => {
                anyhow::ensure!(!session_started, "remote session start was duplicated");
                session_started = true;
                serde_json::json!({"thread":{"id":"remote-thread-external-candidate-v1"}})
            }
            "session/resume" => {
                anyhow::ensure!(
                    request["params"]["threadId"] == "remote-thread-external-candidate-v1",
                    "remote resume changed the retained session identity"
                );
                session_started = true;
                serde_json::json!({"thread":{"id":"remote-thread-external-candidate-v1"}})
            }
            "session/read" => {
                anyhow::ensure!(
                    request["params"]["threadId"] == "remote-thread-external-candidate-v1",
                    "remote inspection changed the retained session identity"
                );
                serde_json::json!({"thread":{"id":"remote-thread-external-candidate-v1","status":{"type":"idle"}}})
            }
            "turn/start" => {
                anyhow::ensure!(session_started, "remote turn preceded session start");
                std::fs::write("candidate-strategy.txt", b"composed external candidate C\n")?;
                serde_json::json!({"turn":{"id":"remote-turn-external-candidate-v1"}})
            }
            other => anyhow::bail!("unsupported remote method `{other}`"),
        };
        serde_json::to_writer(
            &mut stdout,
            &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
        )?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    anyhow::ensure!(
        session_started,
        "remote provider stream closed before session start"
    );
    Ok(())
}
