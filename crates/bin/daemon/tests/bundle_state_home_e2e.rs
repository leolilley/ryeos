//! A bundle's node-owned state home, end to end through the daemon.
//!
//! An installed bundle's signed manifest declares `runtime_authority.state_home`
//! (any namespace the launch names); its tool requests one namespace from a
//! launch parameter. The daemon mints the access cap only when the manifest
//! backs it, and a root launch opens the namespace only for a caller whose own
//! grant carries `ryeos.use.bundle-state.<bundle>/<namespace>`:
//!
//!   * with the caller cap for `ns1`, a tool's write lands under
//!     `<app_root>/.ai/state/bundle-state/<bundle>/ns1/`;
//!   * the same caller asking for `ns2` is refused with
//!     `bundle_state_namespace_denied`, naming the cap that cures it;
//!   * an item of a bundle whose manifest declares no state home is refused with
//!     `bundle_state_home_undeclared`;
//!   * under enforced isolation the tool sees its namespace at the fixed mount
//!     destination and neither its sibling namespace nor the app root.

use ryeosd::test_support as common;

use std::path::{Path, PathBuf};

use common::DaemonHarness;
use common::fast_fixture::{
    FAST_FIXTURE_TIME, FastFixture, register_presigned_fixture_bundle, register_workflow_bundles,
    write_authorized_key_with_scopes,
};
use lillux::crypto::SigningKey;
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use serde_json::{Value, json};

/// The bundle that declares a state home, and the one that does not.
const BUNDLE: &str = "statefix";
const UNDECLARED_BUNDLE: &str = "statenone";

const WRITE_TOOL: &str = "tool:statefix/write";
const PROBE_TOOL: &str = "tool:statefix/probe";
const UNDECLARED_TOOL: &str = "tool:statenone/write";

const MOUNT_DESTINATION: &str = "/run/ryeos/bundle-state";

fn sign(content: &str, fixture: &FastFixture) -> String {
    lillux::signature::sign_content_at(content, &fixture.publisher, "#", None, FAST_FIXTURE_TIME)
}

fn write_signed(path: &Path, content: &str, fixture: &FastFixture) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().expect("fixture file has a parent"))?;
    std::fs::write(path, sign(content, fixture))?;
    Ok(())
}

/// A subprocess tool that runs `script` under `/bin/sh` and requests the
/// bundle state namespace named by its `realm` parameter.
fn state_tool(bundle: &str, script: &str) -> String {
    format!(
        r#"category: "{bundle}"
version: "1.0.0"
tool_type: "subprocess"
executor_id: "@subprocess"
execution_protocol: "protocol:ryeos/core/tool_callback"
description: "state home fixture"
config:
  command: "/bin/sh"
  args:
    - "-c"
    - {script:?}
  timeout_secs: 30
config_schema:
  type: object
  properties:
    realm:
      type: string
requires:
  capabilities:
    manifest:
      runtime_authority:
        state_home:
          namespace:
            from_param: realm
          access: read_write
"#
    )
}

fn plant_bundle(
    state: &Path,
    bundle: &str,
    declares_state_home: bool,
    tools: &[(&str, String)],
    fixture: &FastFixture,
) -> anyhow::Result<()> {
    let root = state.join(".ai/bundles").join(bundle);
    for (name, body) in tools {
        write_signed(
            &root.join(format!(".ai/tools/{bundle}/{name}.yaml")),
            body,
            fixture,
        )?;
    }
    let authority = if declares_state_home {
        "runtime_authority:\n  state_home:\n    namespaces:\n      kind: launch_param\n"
    } else {
        ""
    };
    write_signed(
        &root.join(".ai/manifest.yaml"),
        &format!(
            "name: {bundle}\nversion: 1.0.0\ndescription: state home fixture\nprovides_kinds: []\nrequires_kinds:\n  - handler\n  - parser\n  - runtime\nuses_kinds: []\n{authority}"
        ),
        fixture,
    )?;
    register_presigned_fixture_bundle(state, bundle, &root, fixture)
}

/// A caller that may execute any tool and may open exactly the namespaces
/// named by `use_caps`.
fn limited_caller(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

async fn start(callers: Vec<(SigningKey, Vec<String>)>) -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with(
        move |state: &Path, _user: &Path, fixture: &FastFixture| {
            register_workflow_bundles(state, fixture)?;
            plant_bundle(
                state,
                BUNDLE,
                true,
                &[
                    (
                        "write",
                        state_tool(
                            BUNDLE,
                            "printf 'STATE_HOME=%s' \"$RYEOSD_BUNDLE_STATE_HOME\"; \
                             printf written > \"$RYEOSD_BUNDLE_STATE_HOME/x\"",
                        ),
                    ),
                    (
                        "probe",
                        state_tool(
                            BUNDLE,
                            "printf 'STATE_HOME=%s;' \"$RYEOSD_BUNDLE_STATE_HOME\"; \
                             for sibling in ns2 ns3; do \
                               if [ -e \"$RYEOSD_BUNDLE_STATE_HOME/../$sibling\" ]; then \
                                 printf 'SIBLING=%s:visible;' \"$sibling\"; \
                               else printf 'SIBLING=%s:hidden;' \"$sibling\"; fi; \
                             done; \
                             if [ -n \"$RYEOS_APP_ROOT\" ] && [ -e \"$RYEOS_APP_ROOT/.ai/node/identity\" ]; then \
                               printf 'APP_ROOT=visible'; else printf 'APP_ROOT=hidden'; fi",
                        ),
                    ),
                ],
                fixture,
            )?;
            plant_bundle(
                state,
                UNDECLARED_BUNDLE,
                false,
                &[("write", state_tool(UNDECLARED_BUNDLE, "true"))],
                fixture,
            )?;
            for (key, scopes) in &callers {
                let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
                write_authorized_key_with_scopes(state, key, &fixture.node, &scopes)?;
            }
            Ok(())
        },
        |_| {},
    )
    .await
    .expect("start daemon with state home fixtures")
}

/// Execute `item_ref` projectless as `caller`, with `realm` as the namespace
/// parameter.
async fn execute_as(
    h: &DaemonHarness,
    caller: &SigningKey,
    item_ref: &str,
    realm: &str,
) -> (reqwest::StatusCode, Value) {
    let body = json!({
        "item_ref": item_ref,
        "ref_bindings": {},
        "project_path": null,
        "parameters": { "realm": realm },
        "execution_policy": ExecutionPolicy::projectless(ExecutionResponse::Wait),
    });
    let bytes = serde_json::to_vec(&body).expect("encode execute body");
    let node_key = h.node_key.as_ref().expect("fast fixture node key");
    let mut request = reqwest::Client::new()
        .post(format!("http://{}/execute", h.bind))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in
        common::build_signed_headers_for_bytes(caller, node_key, "POST", "/execute", &bytes)
    {
        request = request.header(name, value);
    }
    let response = request.send().await.expect("post /execute");
    let status = response.status();
    let value = response.json().await.unwrap_or_else(|_| json!({}));
    (status, value)
}

fn namespace_dir(h: &DaemonHarness, bundle: &str, namespace: &str) -> PathBuf {
    h.state_path
        .join(".ai/state/bundle-state")
        .join(bundle)
        .join(namespace)
}

fn execute_scopes(use_caps: &[&str]) -> Vec<String> {
    let mut scopes = vec!["ryeos.execute.*".to_owned()];
    scopes.extend(use_caps.iter().map(|cap| (*cap).to_owned()));
    scopes
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_with_the_caller_cap_writes_into_its_namespace() {
    let caller = limited_caller(0x61);
    let (h, _fixture) = start(vec![(
        caller.clone(),
        execute_scopes(&["ryeos.use.bundle-state.statefix/ns1"]),
    )])
    .await;

    let (status, body) = execute_as(&h, &caller, WRITE_TOOL, "ns1").await;

    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    let landed = namespace_dir(&h, BUNDLE, "ns1").join("x");
    assert_eq!(
        std::fs::read_to_string(&landed).unwrap_or_else(|error| {
            panic!("the tool's write must land at {}: {error}\n{body:#}", landed.display())
        }),
        "written"
    );
    // The home carries the node's signed record, and the tool's own namespace
    // is private to the node's account.
    assert!(
        namespace_dir(&h, BUNDLE, "ns1")
            .parent()
            .unwrap()
            .join(".home.json")
            .is_file()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_cap_for_one_namespace_does_not_open_another() {
    let caller = limited_caller(0x62);
    let (h, _fixture) = start(vec![(
        caller.clone(),
        execute_scopes(&["ryeos.use.bundle-state.statefix/ns1"]),
    )])
    .await;

    let (status, body) = execute_as(&h, &caller, WRITE_TOOL, "ns2").await;

    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body={body:#}");
    let text = body.to_string();
    assert!(text.contains("bundle_state_namespace_denied"), "{text}");
    assert!(
        text.contains("ryeos.use.bundle-state.statefix/ns2") && text.contains("authorize-key"),
        "the refusal must name the cap that cures it: {text}"
    );
    assert!(
        !namespace_dir(&h, BUNDLE, "ns2").exists(),
        "a refused launch must not create the namespace"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_item_whose_bundle_declares_no_state_home_is_refused() {
    let caller = limited_caller(0x63);
    let (h, _fixture) = start(vec![(
        caller.clone(),
        execute_scopes(&["ryeos.use.bundle-state.statenone/ns1"]),
    )])
    .await;

    let (status, body) = execute_as(&h, &caller, UNDECLARED_TOOL, "ns1").await;

    assert!(!status.is_success(), "body={body:#}");
    let text = body.to_string();
    assert!(text.contains("bundle_state_home_undeclared"), "{text}");
    assert!(
        text.contains("runtime_authority.state_home"),
        "the refusal must name the manifest fix: {text}"
    );
    assert!(!namespace_dir(&h, UNDECLARED_BUNDLE, "ns1").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_namespace_is_refused_before_any_directory_exists() {
    let caller = limited_caller(0x64);
    let (h, _fixture) = start(vec![(caller.clone(), execute_scopes(&["ryeos.use.bundle-state.*"]))]).await;

    for bad in ["../escape", "UPPER", ""] {
        let (status, body) = execute_as(&h, &caller, WRITE_TOOL, bad).await;
        assert!(!status.is_success(), "{bad:?}: {body:#}");
        assert!(
            body.to_string().contains("bundle_state_namespace_invalid"),
            "{bad:?}: {body:#}"
        );
    }
    assert!(!h.state_path.join(".ai/state/bundle-state").join(BUNDLE).exists());
}

/// Under enforced isolation the tool sees only its own namespace, at the fixed
/// mount destination, and not the app root. Under disabled isolation there is no
/// mount: the variable names the real directory and the home is a convention.
#[tokio::test(flavor = "multi_thread")]
async fn the_tool_sees_its_own_namespace_and_nothing_beside_it_under_enforced_isolation() {
    let caller = limited_caller(0x65);
    let (h, _fixture) = start(vec![(
        caller.clone(),
        execute_scopes(&["ryeos.use.bundle-state.statefix/*"]),
    )])
    .await;
    // A sibling namespace exists on disk before the probe launches.
    let (status, body) = execute_as(&h, &caller, WRITE_TOOL, "ns2").await;
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");

    let (status, body) = execute_as(&h, &caller, PROBE_TOOL, "ns1").await;

    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    let text = body.to_string();
    let mode = common::fast_fixture::node_isolation_mode(&h.state_path)
        .expect("read the node's isolation mode");
    match mode.as_str() {
        "enforce" => {
            assert!(
                text.contains(&format!("STATE_HOME={MOUNT_DESTINATION}")),
                "the tool names the fixed mount destination: {text}"
            );
            assert!(text.contains("SIBLING=ns2:hidden"), "{text}");
            assert!(text.contains("APP_ROOT=hidden"), "{text}");
        }
        "disabled" => {
            let real = namespace_dir(&h, BUNDLE, "ns1");
            assert!(
                text.contains(&format!("STATE_HOME={}", real.display())),
                "without a mount the tool names the real directory: {text}"
            );
        }
        other => panic!("node isolation mode {other:?} is neither enforce nor disabled"),
    }
}
