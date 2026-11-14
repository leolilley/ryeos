//! Two-daemon end-to-end proof that an origin operator delivers a signed
//! offline operation to a hosted node through its own origin node.
//!
//! ORIGIN holds its operator U1 and a remote `hosted`. HOSTED admits ORIGIN's
//! node key as `remote_node` for ORIGIN's site and U1 as a `remote_principal`
//! bound to that site, with the offline-operations route capabilities and the
//! exact offline scope of `node.allow-restart` (authority-free, so no profile
//! delegation is needed and the properties under test are the delivery's).
//!
//! U1 reads HOSTED's facts through ORIGIN, signs the request with its own key,
//! and submits it through ORIGIN, which forwards it under U1's delegation.
//! HOSTED queues it as U1, names ORIGIN's site as the forwarding node, stops
//! when asked, and applies it at its next start. The refusals the operator
//! meets on the way are typed: no delegation, a delegation that does not lend
//! the submit capability, an inbound-only remote, and a request HOSTED
//! refuses.

use ryeosd::test_support as common;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_node::offline_operation::sign::{Delivery, RemoteSigner, SignDraft, sign_request_now};
use ryeos_node_runtime::remote_delegation::{DelegationBody, DelegationParties, SignedDelegation};
use serde_json::{Value, json};

use common::DaemonHarness;
use common::fast_fixture::FastFixture;

const HOSTED_REMOTE: &str = "hosted";
const INBOUND_REMOTE: &str = "inbound";
const OPERATION: &str = "node.allow-restart";

const SUBMIT_CAP: &str = "ryeos.execute.service.offline-operations/submit";
const FACTS_CAP: &str = "ryeos.execute.service.offline-operations/facts";
const STATUS_CAP: &str = "ryeos.execute.service.offline-operations/status";
const OFFLINE_SCOPE: &str = "ryeos.offline.node.allow-restart";

fn hosted_node_key() -> SigningKey {
    SigningKey::from_bytes(&[181_u8; 32])
}

fn hosted_operator_key() -> SigningKey {
    SigningKey::from_bytes(&[182_u8; 32])
}

fn fingerprint(key: &SigningKey) -> String {
    lillux::signature::compute_fingerprint(&key.verifying_key())
}

fn public_b64(key: &SigningKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn authorized_keys_dir(state_path: &Path) -> std::path::PathBuf {
    state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/auth/authorized_keys")
}

fn node_identity(state_path: &Path) -> Result<ryeos_node_runtime::identity::NodeIdentity> {
    ryeos_node_runtime::identity::NodeIdentity::load(
        &state_path
            .join(ryeos_engine::AI_DIR)
            .join("node/identity/private_key.pem"),
    )
}

struct Pair {
    origin: DaemonHarness,
    origin_fixture: FastFixture,
    hosted: DaemonHarness,
    http: reqwest::Client,
}

async fn start_pair() -> Result<Pair> {
    let origin_node = common::fast_fixture::node_signing_key();
    let u1 = common::fast_fixture::user_signing_key();
    let origin_site = format!("site:{}", fingerprint(&origin_node));
    let (origin, origin_fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {}).await?;
    let (hosted, hosted_fixture) = DaemonHarness::start_fast_with_node_and_user_keys(
        hosted_node_key(),
        hosted_operator_key(),
        |state_path, _, fixture| {
            ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
                &authorized_keys_dir(state_path),
                &fingerprint(&origin_node),
                &public_b64(&origin_node),
                &[ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE.to_owned()],
                "remote-offline-e2e-origin-node",
                &fixture.node_fp(),
                &lillux::time::iso8601_now(),
                &ryeos_node_runtime::identity::GrantTerms::for_tests(),
                &origin_site,
                &fixture.node,
            )?;
            ryeos_node_runtime::identity::reconcile_authorized_key_toml_scopes(
                &authorized_keys_dir(state_path),
                &fingerprint(&u1),
                &public_b64(&u1),
                &strings(&[SUBMIT_CAP, FACTS_CAP, STATUS_CAP, OFFLINE_SCOPE]),
                "remote-offline-e2e-operator",
                &fixture.node_fp(),
                &lillux::time::iso8601_now(),
                &ryeos_node_runtime::identity::GrantTerms::for_tests(),
                &node_identity(state_path)?,
                ryeos_node_runtime::identity::WildcardPolicy::Reject,
                false,
                Some(&origin_site),
                false,
            )?;
            Ok(())
        },
        |_| {},
    )
    .await?;
    let hosted_node = hosted_node_key();
    let pin = |name: &str, reachability, url| ryeos_api::remote::config::RemoteConfig {
        name: name.to_owned(),
        url,
        reachability,
        principal_id: format!("fp:{}", fingerprint(&hosted_node)),
        signing_key: format!("ed25519:{}", public_b64(&hosted_node)),
        site_id: hosted_fixture.site_id(),
        vault_fingerprint: "sha256:remote-offline-e2e-vault".to_owned(),
        ingest_ignore: None,
        project_bindings: HashMap::new(),
    };
    for remote in [
        pin(
            HOSTED_REMOTE,
            ryeos_api::remote::config::Reachability::Outbound,
            Some(format!("http://{}", hosted.bind)),
        ),
        pin(
            INBOUND_REMOTE,
            ryeos_api::remote::config::Reachability::InboundOnly,
            None,
        ),
    ] {
        ryeos_api::remote::config::write_remote_pin(
            &origin.state_path,
            &origin_fixture.node,
            &remote,
            ryeos_api::remote::config::PinSource::DescriptorWithoutContact,
            None,
        )?;
    }
    Ok(Pair {
        origin,
        origin_fixture,
        hosted,
        http: reqwest::Client::new(),
    })
}

impl Pair {
    /// Call an ORIGIN service as U1.
    async fn origin_service(
        &self,
        service: &str,
        parameters: Value,
    ) -> Result<(reqwest::StatusCode, Value)> {
        let u1 = common::fast_fixture::user_signing_key();
        let body = json!({
            "item_ref": service,
            "ref_bindings": {},
            "parameters": parameters,
            "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
        });
        let bytes = serde_json::to_vec(&body)?;
        let mut request = self
            .http
            .post(format!("http://{}/execute", self.origin.bind))
            .header("content-type", "application/json")
            .body(bytes.clone());
        for (name, value) in common::build_signed_headers_for_bytes(
            &u1,
            &self.origin_fixture.node,
            "POST",
            "/execute",
            &bytes,
        ) {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let status = response.status();
        let value = response.json().await.unwrap_or(json!({}));
        Ok((status, value))
    }

    /// Sign a delegation as U1 for `hosted`, the way `ryeos remote delegate`
    /// does, and store it on ORIGIN.
    async fn delegate(&self, scopes: &[&str]) -> Result<()> {
        let u1 = common::fast_fixture::user_signing_key();
        let (status, template) = self
            .origin_service("service:remote/delegate", json!({"remote": HOSTED_REMOTE}))
            .await?;
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
        let body = DelegationBody::new(
            &field("delegator")?,
            DelegationParties {
                delegate_site: &field("delegate_site")?,
                delegate_node: &field("delegate_node")?,
                audience_site: &field("audience_site")?,
                audience_node: &field("audience_node")?,
            },
            &strings(scopes),
            now() + 600,
        );
        let signed = SignedDelegation::sign(body, &u1)?;
        let (status, stored) = self
            .origin_service(
                "service:remote/delegate",
                json!({"remote": HOSTED_REMOTE, "delegation": signed}),
            )
            .await?;
        ensure!(status.is_success(), "store delegation: {status} {stored}");
        Ok(())
    }

    /// Sign `node.allow-restart` as U1, a remote operator of HOSTED, against
    /// the facts read through ORIGIN.
    fn sign(&self, facts: &Value) -> Result<Delivery> {
        let u1 = common::fast_fixture::user_signing_key();
        let grant = ryeos_node_runtime::identity::load_verified_authorized_key(
            &fingerprint(&u1),
            &authorized_keys_dir(&self.hosted.state_path),
            &node_identity(&self.hosted.state_path)?,
        )?
        .context("HOSTED holds no grant for U1")?;
        // The facts read through ORIGIN name U1's own grant on HOSTED, the
        // delegated principal's, not ORIGIN's node grant; `sign` takes the
        // grant digest and origin site from them.
        let signer = &facts["signer"];
        ensure!(
            signer["fingerprint"] == fingerprint(&u1).as_str(),
            "facts name another signer: {signer}"
        );
        ensure!(
            signer["grant_digest"] == grant.source_file_hash.as_str(),
            "facts name another grant: {signer}"
        );
        ensure!(
            signer["origin_site_id"] == self.origin_fixture.site_id().as_str(),
            "facts name another origin: {signer}"
        );
        let measured = facts["facts"].as_object().context("facts object")?;
        let spec = ryeos_node::offline_operation::table::lookup(OPERATION).context("operation")?;
        let pins = ryeos_node::offline_operation::facts::MANDATORY_FACTS
            .iter()
            .chain(spec.preconditions)
            .map(|fact| {
                Ok((
                    (*fact).to_owned(),
                    measured[*fact]
                        .as_str()
                        .with_context(|| format!("fact {fact}"))?
                        .to_owned(),
                ))
            })
            .collect::<Result<_>>()?;
        sign_request_now(
            SignDraft {
                node_fingerprint: facts["node_fingerprint"]
                    .as_str()
                    .context("node fingerprint")?
                    .to_owned(),
                operation: OPERATION.to_owned(),
                arguments: json!({}),
                inputs: Default::default(),
                expected_result: Default::default(),
                pins,
                lifetime_minutes: None,
                remote: Some(RemoteSigner {
                    grant_digest: signer["grant_digest"]
                        .as_str()
                        .context("grant digest")?
                        .to_owned(),
                    origin_site_id: signer["origin_site_id"]
                        .as_str()
                        .context("origin site")?
                        .to_owned(),
                }),
            },
            &u1,
        )
    }

    async fn submit(
        &self,
        remote: &str,
        delivery: &Delivery,
        stop: bool,
    ) -> Result<(reqwest::StatusCode, Value)> {
        let mut parameters = serde_json::to_value(delivery)?;
        parameters["remote"] = json!(remote);
        if stop {
            parameters["stop"] = json!("now");
        }
        self.origin_service("service:remote/offline-operations-submit", parameters)
            .await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_origin_operator_delivers_an_offline_operation_through_its_node() -> Result<()> {
    let mut pair = start_pair().await?;
    let u1_principal = format!(
        "fp:{}",
        fingerprint(&common::fast_fixture::user_signing_key())
    );

    // An inbound-only remote is never dialed.
    let (status, body) = pair
        .origin_service(
            "service:remote/offline-operations-facts",
            json!({"remote": INBOUND_REMOTE}),
        )
        .await?;
    assert!(!status.is_success(), "{body}");
    assert!(body.to_string().contains("remote_inbound_only"), "{body}");

    // No delegation: refused on ORIGIN with the command that cures it.
    let (status, body) = pair
        .origin_service(
            "service:remote/offline-operations-facts",
            json!({"remote": HOSTED_REMOTE}),
        )
        .await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.to_string().contains("remote_delegation_missing"),
        "{body}"
    );

    // A delegation that lends the reads but not the submit capability.
    pair.delegate(&[FACTS_CAP, STATUS_CAP]).await?;
    let (status, facts) = pair
        .origin_service(
            "service:remote/offline-operations-facts",
            json!({"remote": HOSTED_REMOTE}),
        )
        .await?;
    ensure!(
        status.is_success(),
        "facts through origin: {status} {facts}"
    );
    let facts = facts["result"].clone();
    assert_eq!(facts["remote"], HOSTED_REMOTE);
    let delivery = pair.sign(&facts)?;
    let (status, body) = pair.submit(HOSTED_REMOTE, &delivery, false).await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body}");
    let text = body.to_string();
    assert!(text.contains("remote_delegation_scope_missing"), "{text}");
    assert!(text.contains(SUBMIT_CAP), "{text}");

    // The full delegation. A request HOSTED refuses keeps HOSTED's reason.
    pair.delegate(&[SUBMIT_CAP, FACTS_CAP, STATUS_CAP]).await?;
    let mut forged = delivery.clone();
    forged.request.body.arguments = json!({ "tampered": true });
    let (status, body) = pair.submit(HOSTED_REMOTE, &forged, false).await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body}");
    let text = body.to_string();
    assert!(text.contains("remote_offline_operation_refused"), "{text}");
    assert!(text.contains("bad_signature"), "{text}");

    // Delivered: HOSTED queues it as U1, forwarded by ORIGIN's site, and stops.
    let request_digest = delivery.request.digest()?;
    let (status, reply) = pair.submit(HOSTED_REMOTE, &delivery, true).await?;
    ensure!(status.is_success(), "delivery: {status} {reply}");
    let result = &reply["result"]["result"];
    assert_eq!(result["request_digest"], request_digest.as_str(), "{reply}");
    assert_eq!(result["submitted_by"], u1_principal.as_str(), "{reply}");
    assert_eq!(
        result["delegated_via"],
        pair.origin_fixture.site_id().as_str(),
        "{reply}"
    );
    assert_eq!(result["stop"], "requested", "{reply}");

    let exit = tokio::time::timeout(Duration::from_secs(120), pair.hosted.child.wait()).await??;
    assert!(exit.success(), "hosted exited with {exit}");
    let queued = ryeos_node::offline_operation::read_ledger(&pair.hosted.state_path)?;
    assert_eq!(queued.last_sequence, 0, "queued, not yet applied");

    // The next start applies it, signed by U1.
    pair.hosted.respawn_with(|_| {}).await?;
    let applied = ryeos_node::offline_operation::read_ledger(&pair.hosted.state_path)?;
    assert_eq!(applied.last_sequence, 1);
    assert!(applied.request_digests.contains(&request_digest));
    let (status, report) = pair
        .origin_service(
            "service:remote/offline-operations-status",
            json!({"remote": HOSTED_REMOTE}),
        )
        .await?;
    ensure!(
        status.is_success(),
        "status through origin: {status} {report}"
    );
    let receipts = report["result"]["receipts"]
        .as_array()
        .context("receipts")?;
    assert_eq!(receipts.len(), 1, "{report}");
    assert_eq!(receipts[0]["request_digest"], request_digest.as_str());
    Ok(())
}
