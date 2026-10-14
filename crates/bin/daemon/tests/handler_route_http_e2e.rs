//! HTTP end-to-end regression for `response.mode: handler` routes.
//!
//! `handler_route_authority_e2e.rs` drives the engine's plan builder directly
//! and therefore never crosses the daemon's route dispatch path. Two bugs hid
//! there:
//!
//!   * the dispatch input built by `handler_mode.rs` omitted `ref_bindings`,
//!     which `DispatchSourceConfig` requires, so every handler request died
//!     with a 400 "missing field `ref_bindings`" before the tool ran;
//!   * the fixed route-handler authority carried only
//!     `ryeos.execute.tool.<subject>`, but the dispatch invoker also demands
//!     `ryeos.write.project.live` for any live execution, so the request was
//!     then refused with 403.
//!
//! These tests send a real anonymous GET through a running `ryeosd` to a
//! bundle-declared `auth: none` handler route, so they fail on either bug.

mod common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{FAST_FIXTURE_TIME, FastFixture};

/// Mirrors the Agent Kiwi OAuth callback tool: it ignores its input and prints
/// an HTTP response envelope that handler mode turns into a 302 redirect.
const CALLBACK_TOOL: &str = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: agent-kiwi/oauth
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "OAuth callback handler that returns an HTTP response envelope"

import json
import sys

print(json.dumps({
    "response": {
        "status": 302,
        "headers": {"Location": "https://agentkiwi.nz/?google=connected"},
    }
}))
sys.exit(0)
"#;

/// Public GET route whose fixed handler is the callback tool above. GET
/// carries no body, so `request.body` stays at its `none` default and the
/// handler only receives the query string.
const CALLBACK_ROUTE: &str = r#"id: oauth/callback
path: /oauth/callback
methods:
  - GET
auth: none
limits:
  body_bytes_max: 0
  timeout_ms: 30000
  concurrent_max: 4
response:
  mode: handler
  source: tool:agent-kiwi/oauth/callback
  source_config:
    request:
      query: true
"#;

/// Plant a registered fixture bundle named `bundle_name` under the daemon
/// state root, holding the signed route and (optionally) the signed tool.
///
/// Handler mode derives the route's bundle id from the bundle root's directory
/// name and requires it to equal the bundle segment of the `tool:` source, so
/// the directory name is the lever these tests pull.
fn plant_handler_bundle(
    state: &Path,
    bundle_name: &str,
    with_tool: bool,
    fixture: &FastFixture,
) -> anyhow::Result<()> {
    let bundle_root = state.join(".ai/bundles").join(bundle_name);
    let routes_dir = bundle_root.join(".ai/node/routes");
    std::fs::create_dir_all(&routes_dir)?;
    let signed_route = lillux::signature::sign_content_at(
        CALLBACK_ROUTE,
        &fixture.publisher,
        "#",
        None,
        FAST_FIXTURE_TIME,
    );
    std::fs::write(routes_dir.join("oauth-callback.yaml"), signed_route)?;

    if with_tool {
        // `tool:agent-kiwi/oauth/callback` resolves under the bundle root to
        // `.ai/tools/agent-kiwi/oauth/callback.py`. Project-owned executable
        // source must carry a verified signer; the fixture publisher is pinned
        // in the node trust store.
        let tool_dir = bundle_root.join(".ai/tools/agent-kiwi/oauth");
        std::fs::create_dir_all(&tool_dir)?;
        let signed_tool = lillux::signature::sign_content_at(
            CALLBACK_TOOL,
            &fixture.publisher,
            "#",
            None,
            FAST_FIXTURE_TIME,
        );
        std::fs::write(tool_dir.join("callback.py"), signed_tool)?;
    }

    common::fast_fixture::register_fixture_bundle(state, bundle_name, &bundle_root, fixture)
}

/// Positive case: an anonymous GET to the handler route must run the fixed
/// tool under route-handler authority and relay its 302 envelope. Before the
/// fixes this returned 400 (missing `ref_bindings`) and then 403 (missing
/// live-project write capability); neither is reachable from the plan-builder
/// test, which is why this one goes over HTTP.
#[tokio::test(flavor = "multi_thread")]
async fn anonymous_get_to_handler_route_returns_tool_redirect() {
    let (h, _fixture) = DaemonHarness::start_fast_with(
        |state, _user, fixture| plant_handler_bundle(state, "agent-kiwi", true, fixture),
        |_cmd| {},
    )
    .await
    .expect("start daemon with agent-kiwi handler bundle");

    // The assertion is on the redirect itself, so the client must not follow
    // it; the harness helpers only speak signed POST, hence raw reqwest here.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build non-following client");
    let resp = client
        .get(format!("http://{}/oauth/callback?code=x", h.bind))
        .send()
        .await
        .expect("GET /oauth/callback");
    let status = resp.status();
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = resp.text().await.unwrap_or_default();

    assert_eq!(
        status,
        reqwest::StatusCode::FOUND,
        "handler route must relay the tool's 302 envelope; got {status} with body: {body}"
    );
    assert_eq!(
        location.as_deref(),
        Some("https://agentkiwi.nz/?google=connected"),
        "handler route must relay the tool's Location header; body: {body}"
    );
}

/// Negative case: the route bundle id comes from the bundle root directory
/// name, so a route planted in `handler-route-e2e` that names
/// `tool:agent-kiwi/...` is a cross-bundle handler. Route compilation runs at
/// startup, so the daemon must refuse to come up with the exact mismatch
/// text rather than exposing a route that would resolve into another bundle.
#[tokio::test(flavor = "multi_thread")]
async fn cross_bundle_handler_source_is_refused_at_startup() {
    let result = DaemonHarness::start_fast_with(
        |state, _user, fixture| plant_handler_bundle(state, "handler-route-e2e", false, fixture),
        |_cmd| {},
    )
    .await;

    let startup_error = match result {
        Ok((_h, _fixture)) => {
            panic!("daemon must not start with a handler route whose source names another bundle")
        }
        Err(error) => format!("{error:#}"),
    };
    assert!(
        startup_error
            .contains("handler source bundle 'agent-kiwi' must match route bundle 'handler-route-e2e'"),
        "startup must report the exact bundle mismatch; got: {startup_error}"
    );
}
