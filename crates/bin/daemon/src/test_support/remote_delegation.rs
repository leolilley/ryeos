//! Store an operator's delegation for a remote on a running origin node, the
//! way `ryeos remote delegate --remote <name>` does: read the template the
//! origin publishes, sign it as the operator, and store it.

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use lillux::crypto::SigningKey;
use ryeos_node_runtime::remote_delegation::{DelegationBody, DelegationParties, SignedDelegation};
use serde_json::{Value, json};

async fn origin_service(
    origin: SocketAddr,
    origin_node: &SigningKey,
    operator: &SigningKey,
    parameters: Value,
) -> Result<(reqwest::StatusCode, Value)> {
    let body = json!({
        "item_ref": "service:remote/delegate",
        "ref_bindings": {},
        "parameters": parameters,
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    });
    let bytes = serde_json::to_vec(&body)?;
    let mut request = reqwest::Client::new()
        .post(format!("http://{origin}/execute"))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in
        super::build_signed_headers_for_bytes(operator, origin_node, "POST", "/execute", &bytes)
    {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let value = response.json().await.unwrap_or(json!({}));
    Ok((status, value))
}

/// Sign a delegation as `operator` for `remote` lending `scopes`, and store it
/// on the origin node listening at `origin`.
pub async fn store_operator_delegation(
    origin: SocketAddr,
    origin_node: &SigningKey,
    operator: &SigningKey,
    remote: &str,
    scopes: &[&str],
) -> Result<()> {
    let (status, template) =
        origin_service(origin, origin_node, operator, json!({"remote": remote})).await?;
    ensure!(
        status.is_success(),
        "delegation template: {status} {template}"
    );
    let facts = &template["result"]["template"];
    let field = |name: &str| -> Result<String> {
        facts[name]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("template omits {name}"))
    };
    let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
    let not_after = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 600;
    let body = DelegationBody::new(
        &field("delegator")?,
        DelegationParties {
            delegate_site: &field("delegate_site")?,
            delegate_node: &field("delegate_node")?,
            audience_site: &field("audience_site")?,
            audience_node: &field("audience_node")?,
        },
        &scopes,
        not_after,
    );
    let signed = SignedDelegation::sign(body, operator)?;
    let (status, stored) = origin_service(
        origin,
        origin_node,
        operator,
        json!({"remote": remote, "delegation": signed}),
    )
    .await?;
    ensure!(status.is_success(), "store delegation: {status} {stored}");
    Ok(())
}
