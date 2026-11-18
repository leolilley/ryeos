//! End-to-end proof of machine locality.
//!
//! One node: `service:node/locality` serves a digest or an unknown for each
//! relation and never a raw boot id, path or uid, and refuses a caller that
//! does not hold `ryeos.execute.service.node/locality`.
//!
//! Two nodes on one test host: HOSTED grants ORIGIN's operator the locality
//! capability, ORIGIN holds remotes for it and for an unreachable ghost, and
//! `service:node/locality/groups` on ORIGIN asks each over the remote
//! forwarding path. Two nodes on one host come out as one group sharing a
//! boot; a remote that has not granted the capability is `not_granted`; an
//! unreachable remote is `did_not_answer`, not unknown.

use ryeosd::test_support as common;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_effect_contract::locality::{
    GroupState, LocalityGrade, LocalityGroup, LocalityGroups, MemberAnswer, MemberKind,
    NodeLocalityFacts, REASON_EMULATED_KERNEL, REASON_UNREACHABLE, RELATION_ACCOUNT, RELATION_BOOT,
    RELATION_STORAGE,
};
use serde_json::json;

use common::DaemonHarness;
use common::fast_fixture::FastFixture;

const LOCALITY_CAP: &str = "ryeos.execute.service.node/locality";
const HOSTED_REMOTE: &str = "hosted";
const GHOST_REMOTE: &str = "ghost";

fn hosted_node_key() -> SigningKey {
    SigningKey::from_bytes(&[181_u8; 32])
}

fn hosted_operator_key() -> SigningKey {
    SigningKey::from_bytes(&[182_u8; 32])
}

fn ghost_node_key() -> SigningKey {
    SigningKey::from_bytes(&[183_u8; 32])
}

fn restricted_key() -> SigningKey {
    SigningKey::from_bytes(&[184_u8; 32])
}

fn fingerprint(key: &SigningKey) -> String {
    lillux::signature::compute_fingerprint(&key.verifying_key())
}

fn public_b64(key: &SigningKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn authorized_keys_dir(state_path: &Path) -> std::path::PathBuf {
    state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/auth/authorized_keys")
}

fn gvisor() -> bool {
    lillux::process_control::host_kernel_is_gvisor()
}

/// Admit `key` on the daemon at `state_path` as a local client with `scopes`.
fn authorize_local_client(
    state_path: &Path,
    fixture: &FastFixture,
    key: &SigningKey,
    scopes: &[&str],
) -> Result<()> {
    ryeos_node_runtime::identity::write_authorized_key_toml(
        &authorized_keys_dir(state_path),
        &fingerprint(key),
        &public_b64(key),
        &strings(scopes),
        "node-locality-e2e-local",
        &fixture.node_fp(),
        &lillux::time::iso8601_now(),
        &ryeos_node_runtime::identity::GrantTerms::for_tests(),
        &fixture.node,
        ryeos_node_runtime::identity::WildcardPolicy::Reject,
    )?;
    Ok(())
}

fn authorize_remote_node(
    state_path: &Path,
    fixture: &FastFixture,
    node: &SigningKey,
    site_id: &str,
) -> Result<()> {
    ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
        &authorized_keys_dir(state_path),
        &fingerprint(node),
        &public_b64(node),
        &[ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE.to_owned()],
        "node-locality-e2e-origin-node",
        &fixture.node_fp(),
        &lillux::time::iso8601_now(),
        &ryeos_node_runtime::identity::GrantTerms::for_tests(),
        site_id,
        &fixture.node,
    )?;
    Ok(())
}

fn authorize_remote_principal(
    state_path: &Path,
    fixture: &FastFixture,
    principal: &SigningKey,
    site_id: &str,
    scopes: &[&str],
) -> Result<()> {
    let identity_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(&identity_path)?;
    ryeos_node_runtime::identity::reconcile_authorized_key_toml_scopes(
        &authorized_keys_dir(state_path),
        &fingerprint(principal),
        &public_b64(principal),
        &strings(scopes),
        "node-locality-e2e-operator",
        &fixture.node_fp(),
        &lillux::time::iso8601_now(),
        &ryeos_node_runtime::identity::GrantTerms::for_tests(),
        &identity,
        ryeos_node_runtime::identity::WildcardPolicy::Reject,
        false,
        Some(site_id),
        false,
    )?;
    Ok(())
}

fn remote_config(
    name: &str,
    url: String,
    node_key: &SigningKey,
    site_id: &str,
) -> ryeos_api::remote::config::RemoteConfig {
    ryeos_api::remote::config::RemoteConfig {
        name: name.to_owned(),
        url: Some(url),
        reachability: Default::default(),
        principal_id: format!("fp:{}", fingerprint(node_key)),
        signing_key: format!("ed25519:{}", public_b64(node_key)),
        site_id: site_id.to_owned(),
        vault_fingerprint: format!("sha256:{name}-fixture-vault"),
        ingest_ignore: None,
        project_bindings: HashMap::new(),
    }
}

fn pin_remote(state_path: &Path, remote: &ryeos_api::remote::config::RemoteConfig) -> Result<()> {
    let node = ryeos_node_runtime::identity::NodeIdentity::load(
        &ryeos_engine::roots::RuntimeRoot::new(state_path.to_path_buf()).node_signing_key_path(),
    )?;
    ryeos_api::remote::config::write_remote_pin(
        state_path,
        node.signing_key(),
        remote,
        ryeos_api::remote::config::PinSource::DescriptorWithoutContact,
        None,
    )?;
    Ok(())
}

/// ORIGIN, HOSTED and an unreachable ghost remote on ORIGIN. HOSTED grants
/// ORIGIN's operator `hosted_grant`; the operator's delegations lend
/// `LOCALITY_CAP` to both remotes.
struct Pair {
    origin: DaemonHarness,
    origin_fixture: FastFixture,
    _hosted: DaemonHarness,
    hosted_fixture: FastFixture,
}

async fn start_pair(hosted_grant: &[&str]) -> Result<Pair> {
    let origin_node = common::fast_fixture::node_signing_key();
    let operator = common::fast_fixture::user_signing_key();
    let origin_site = format!("site:{}", fingerprint(&origin_node));
    let (hosted, hosted_fixture) = DaemonHarness::start_fast_with_node_and_user_keys(
        hosted_node_key(),
        hosted_operator_key(),
        |state_path, _, fixture| {
            authorize_remote_node(state_path, fixture, &origin_node, &origin_site)?;
            authorize_remote_principal(state_path, fixture, &operator, &origin_site, hosted_grant)?;
            Ok(())
        },
        |_| {},
    )
    .await?;
    let (origin, origin_fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {}).await?;

    pin_remote(
        &origin.state_path,
        &remote_config(
            HOSTED_REMOTE,
            format!("http://{}", hosted.bind),
            &hosted_node_key(),
            &hosted_fixture.site_id(),
        ),
    )?;
    let ghost = ghost_node_key();
    pin_remote(
        &origin.state_path,
        &remote_config(
            GHOST_REMOTE,
            // Port 9 (discard) is closed on a test host: connection refused.
            "http://127.0.0.1:9".to_owned(),
            &ghost,
            &format!("site:{}", fingerprint(&ghost)),
        ),
    )?;
    for remote in [HOSTED_REMOTE, GHOST_REMOTE] {
        common::remote_delegation::store_operator_delegation(
            origin.bind,
            &origin_node,
            &operator,
            remote,
            &[LOCALITY_CAP],
        )
        .await?;
    }
    Ok(Pair {
        origin,
        origin_fixture,
        _hosted: hosted,
        hosted_fixture,
    })
}

impl Pair {
    async fn groups(&self) -> Result<LocalityGroups> {
        let (status, body) = self
            .origin
            .post_execute("service:node/locality/groups", ".", json!({}))
            .await?;
        ensure!(status.is_success(), "groups returned {status}: {body}");
        serde_json::from_value(
            body.get("result")
                .cloned()
                .with_context(|| format!("groups returned no result: {body}"))?,
        )
        .with_context(|| format!("groups result is not the contract shape: {body}"))
    }
}

fn group_of<'a>(groups: &'a LocalityGroups, id: &str) -> &'a LocalityGroup {
    groups
        .groups
        .iter()
        .find(|group| group.members.iter().any(|member| member.id == id))
        .unwrap_or_else(|| panic!("no group holds {id}: {groups:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_serves_digests_and_never_a_raw_fact() -> Result<()> {
    let (h, _fixture) = DaemonHarness::start_fast().await?;
    let (status, body) = h
        .post_execute("service:node/locality", ".", json!({}))
        .await?;
    ensure!(status.is_success(), "locality returned {status}: {body}");
    let result = body
        .get("result")
        .cloned()
        .with_context(|| format!("no result: {body}"))?;
    let facts: NodeLocalityFacts = serde_json::from_value(result.clone())?;
    facts.validate()?;
    for relation in [RELATION_BOOT, RELATION_ACCOUNT, RELATION_STORAGE] {
        assert!(facts.relations.contains_key(relation), "{relation}");
    }
    if gvisor() {
        for relation in [RELATION_BOOT, RELATION_ACCOUNT, RELATION_STORAGE] {
            assert_eq!(
                facts.relations[relation].grade(),
                LocalityGrade::Unknown,
                "{relation}"
            );
        }
    } else {
        assert_eq!(
            facts.relations[RELATION_BOOT].grade(),
            LocalityGrade::Measured,
            "{result}"
        );
    }

    // Nothing the host measured is in the response.
    let text = result.to_string();
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap_or_default()
        .trim()
        .to_owned();
    assert!(boot_id.is_empty() || !text.contains(&boot_id), "{text}");
    let root = h.state_path.display().to_string();
    assert!(!text.contains(&root), "{text}");
    assert!(
        !text.contains("boot_id") && !text.contains("mount"),
        "{text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_without_the_capability_is_refused() -> Result<()> {
    let restricted = restricted_key();
    let (h, fixture) = DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            authorize_local_client(
                state_path,
                fixture,
                &restricted_key(),
                &["ryeos.execute.service.node/status"],
            )
        },
        |_| {},
    )
    .await?;
    let body = json!({
        "item_ref": "service:node/locality",
        "ref_bindings": {},
        "parameters": {},
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    });
    let bytes = serde_json::to_vec(&body)?;
    let mut request = reqwest::Client::new()
        .post(format!("http://{}/execute", h.bind))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in common::build_signed_headers_for_bytes(
        &restricted,
        &fixture.node,
        "POST",
        "/execute",
        &bytes,
    ) {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{text}");
    assert!(text.contains(LOCALITY_CAP), "{text}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_on_one_host_are_one_group_and_an_unreachable_remote_did_not_answer() -> Result<()>
{
    let pair = start_pair(&[LOCALITY_CAP]).await?;
    let groups = pair.groups().await?;
    let origin_site = pair.origin_fixture.site_id();
    let hosted_site = pair.hosted_fixture.site_id();

    // ORIGIN and HOSTED run on this host's one kernel.
    let together = group_of(&groups, &hosted_site);
    let mut member_ids: Vec<&str> = together.members.iter().map(|m| m.id.as_str()).collect();
    member_ids.sort();
    let mut expected = vec![origin_site.as_str(), hosted_site.as_str()];
    expected.sort();
    if gvisor() {
        assert_eq!(together.state, GroupState::CannotTell, "{groups:?}");
        assert_eq!(member_ids, [hosted_site.as_str()]);
        let hosted = &together.members[0];
        assert_eq!(
            hosted.facts[RELATION_BOOT].reason.as_deref(),
            Some(REASON_EMULATED_KERNEL)
        );
    } else {
        assert_eq!(together.state, GroupState::Observed, "{groups:?}");
        assert_eq!(member_ids, expected);
        assert!(
            together.relations.iter().any(|row| {
                row.relation == RELATION_BOOT && row.grade == LocalityGrade::Measured
            })
        );
        let hosted = together
            .members
            .iter()
            .find(|member| member.id == hosted_site)
            .unwrap();
        assert_eq!(hosted.name, HOSTED_REMOTE);
        assert_eq!(hosted.kind, MemberKind::Remote);
        assert!(matches!(hosted.answer, MemberAnswer::Answered { .. }));
        let this = together
            .members
            .iter()
            .find(|member| member.id == origin_site)
            .unwrap();
        assert_eq!(this.kind, MemberKind::This);
    }

    // The unreachable remote is its own row: nothing was learned, which is not
    // the same as "cannot tell".
    let ghost_site = format!("site:{}", fingerprint(&ghost_node_key()));
    let ghost = group_of(&groups, &ghost_site);
    assert_eq!(ghost.state, GroupState::CannotTell);
    assert_eq!(ghost.members.len(), 1);
    assert_eq!(ghost.members[0].name, GHOST_REMOTE);
    match &ghost.members[0].answer {
        MemberAnswer::DidNotAnswer { reason, as_of } => {
            assert_eq!(reason, REASON_UNREACHABLE);
            assert!(!as_of.is_empty());
        }
        other => panic!("the ghost should not have answered: {other:?}"),
    }
    assert!(ghost.members[0].facts.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_that_has_not_granted_the_capability_is_not_granted() -> Result<()> {
    // HOSTED grants the operator something else, so the lent capability is
    // outside the grant and HOSTED refuses.
    let pair = start_pair(&["ryeos.execute.service.node/status"]).await?;
    let groups = pair.groups().await?;
    let hosted_site = pair.hosted_fixture.site_id();
    let hosted = group_of(&groups, &hosted_site);
    assert_eq!(hosted.state, GroupState::CannotTell, "{groups:?}");
    assert!(
        matches!(hosted.members[0].answer, MemberAnswer::NotGranted { .. }),
        "{:?}",
        hosted.members[0].answer
    );
    // This node still answers for itself.
    let origin = group_of(&groups, &pair.origin_fixture.site_id());
    assert!(matches!(
        origin.state,
        GroupState::Alone | GroupState::CannotTell
    ));
    Ok(())
}
