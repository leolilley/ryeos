//! The acting principal reaches a tool through the daemon.
//!
//! A bundle-style Python tool launched under the default `tool_callback`
//! protocol receives `RYEOSD_ACTING_PRINCIPAL`, and its value is the exact
//! principal the daemon authorized for the launch: the authenticated caller's
//! `fp:` fingerprint.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use lillux::crypto::SigningKey;

/// Prints the principal the daemon injected, or a marker when it is absent.
const ECHO_PRINCIPAL_TOOL: &str = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: principal
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/tool_callback"
#   description: "echo the injected acting principal"

import json, os
print(json.dumps({"acting_principal": os.environ.get("RYEOSD_ACTING_PRINCIPAL", "<absent>")}))
"#;

fn plant_echo_principal(project: &Path, signer: &SigningKey) {
    let dir = project.join(".ai/tools/principal");
    std::fs::create_dir_all(&dir).expect("create project tools dir");
    let signed = lillux::signature::sign_content(ECHO_PRINCIPAL_TOOL, signer, "#", None);
    std::fs::write(dir.join("echo.py"), signed).expect("write echo principal tool");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_receives_the_callers_principal_in_its_environment() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_echo_principal(project.path(), &fixture.user);

    let (status, body) = h
        .post_execute(
            "tool:principal/echo",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post tool:principal/echo");

    assert!(status.is_success(), "status={status}; body={body:#}");
    let expected = format!("fp:{}", fixture.user_fp());
    assert!(
        body.to_string().contains(&expected),
        "the tool must see the caller's principal {expected}: {body:#}"
    );
    assert!(
        !body.to_string().contains("<absent>"),
        "RYEOSD_ACTING_PRINCIPAL must be injected: {body:#}"
    );
}
