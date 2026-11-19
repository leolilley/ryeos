//! `node.locality.groups` — which of the operator's nodes share a boot,
//! storage or account, compared by the node the operator is attached to.
//!
//! This node reads its own facts, asks every configured remote for
//! `service:node/locality` and runs the pure comparison in
//! [`ryeos_effect_contract::locality`]. The ask is one wait-mode, projectless
//! `/execute` on the remote with a bounded timeout, under the same outbound
//! principal every other remote handler uses: the caller under its signed
//! delegation, or this node when the caller is the node. The remote's own
//! grants decide whether it answers.
//!
//! Outcomes per remote:
//! - it answered: its facts and the time;
//! - it refused (HTTP 401 or 403, or no delegation to present): `not_granted`;
//! - it could not be asked or did not answer in time: `did_not_answer` with a
//!   reason key. That is not "unknown": nothing was learned.
//!
//! Nodes installed as host services on this host that this node has no remote
//! for are listed as "installed here, not connected". They are never asked and
//! make no claim.
//!
//! Member ids carry a prefix naming their identity: `site:<fp>` for this node
//! and every pinned remote, `installed:<service name>` for an installed node
//! that is not connected, and `pin:<name>` for a pin that failed verification.
//!
//! Machine declarations are the node-signed records under
//! `.ai/node/machines/` ([`crate::machine_declaration`]). A record that does
//! not verify, and every record that shares a node with another, is reported in
//! the groups' `invalid_declarations` and left out of the comparison; it never
//! fails the view. Declarations name site ids only, so an installed-not-
//! connected node is never a member of a declared machine.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use ryeos_app::state::AppState;
use ryeos_effect_contract::locality::{
    InvalidDeclaration, LocalityError, MachineDeclaration, MemberKind, NodeAnswer, NodeInput,
    NodeLocalityFacts, REASON_INBOUND_ONLY, REASON_INVALID_ANSWER, REASON_REMOTE_ERROR,
    REASON_REMOTE_INVALID, REASON_SERVICE_NOT_INSTALLED, REASON_SITE_MISMATCH, REASON_TIMEOUT,
    REASON_UNREACHABLE, compare_locality, exclude_conflicting_declarations, installed_member_id,
    pin_member_id, validate_member_id,
};
use ryeos_effect_contract::rfc3339::now_millis;
use ryeos_executor::executor::ServiceAvailability;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::handlers::node_locality::local_facts;
use crate::handlers::remote_push::OutboundPrincipal;
use crate::machine_declaration::{self, DeclarationsReport};
use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteHttpError;
use crate::remote::config::{self, RemoteConfig, RemotesLoadReport};

/// The capability a caller needs to read the compared groups.
pub const CAPABILITY: &str = "ryeos.execute.service.node/locality/groups";

/// The service each remote is asked for.
const LOCALITY_SERVICE: &str = "service:node/locality";

/// How long one remote has to answer. A remote that is slower is a
/// did-not-answer row, so one stopped node cannot hold the view.
const PER_REMOTE_TIMEOUT: Duration = Duration::from_secs(8);

/// Remotes asked at once.
const MAX_CONCURRENT_ASKS: usize = 8;

/// Display name of the node doing the comparing.
const THIS_NODE_NAME: &str = "this node";

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

fn did_not_answer(reason: &str, as_of: String) -> NodeAnswer {
    NodeAnswer::DidNotAnswer {
        reason: reason.to_owned(),
        as_of,
    }
}

/// What a failed ask means. A refusal is `not_granted`; anything else is a
/// did-not-answer with a stable reason.
fn answer_for_error(error: &anyhow::Error, as_of: String) -> NodeAnswer {
    if let Some(http) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RemoteHttpError>())
    {
        return match http.status.as_u16() {
            401 | 403 => NodeAnswer::NotGranted { as_of },
            404 => did_not_answer(REASON_SERVICE_NOT_INSTALLED, as_of),
            _ => did_not_answer(REASON_REMOTE_ERROR, as_of),
        };
    }
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<reqwest::Error>().is_some())
    {
        return did_not_answer(REASON_UNREACHABLE, as_of);
    }
    did_not_answer(REASON_REMOTE_ERROR, as_of)
}

/// Ask one remote for its locality facts.
async fn ask_remote(
    state: &Arc<AppState>,
    ctx: &HandlerContext,
    principal: OutboundPrincipal,
    remote: &RemoteConfig,
) -> NodeAnswer {
    if remote.outbound_url().is_err() {
        // This node never dials an inbound-only remote.
        return did_not_answer(REASON_INBOUND_ONLY, now_millis());
    }
    let client = match principal.client(state, remote, ctx) {
        Ok(client) => client,
        // No delegation to present, or an expired one: the operator has not
        // lent this node authority to ask there.
        Err(HandlerError::Forbidden(_)) => {
            return NodeAnswer::NotGranted {
                as_of: now_millis(),
            };
        }
        Err(HandlerError::Structured { status, .. }) if status == 401 || status == 403 => {
            return NodeAnswer::NotGranted {
                as_of: now_millis(),
            };
        }
        Err(_) => return did_not_answer(REASON_REMOTE_ERROR, now_millis()),
    };
    let no_bindings = BTreeMap::new();
    let no_parameters = serde_json::json!({});
    let policy = ExecutionPolicy::projectless(ExecutionResponse::Wait);
    let call = client.execute_service_result(
        LOCALITY_SERVICE,
        &no_bindings,
        None,
        &no_parameters,
        &policy,
    );
    match tokio::time::timeout(PER_REMOTE_TIMEOUT, call).await {
        Err(_) => did_not_answer(REASON_TIMEOUT, now_millis()),
        Ok(Err(error)) => answer_for_error(&error, now_millis()),
        Ok(Ok(value)) => match serde_json::from_value::<NodeLocalityFacts>(value)
            .map_err(anyhow::Error::from)
            .and_then(|facts| facts.validate().map(|()| facts).map_err(Into::into))
        {
            // The pin names a site; a node answering as another site is not
            // the node that was pinned, whatever its facts say.
            Ok(facts) if facts.site_id != remote.site_id => {
                did_not_answer(REASON_SITE_MISMATCH, now_millis())
            }
            Ok(facts) => NodeAnswer::Answered {
                facts,
                as_of: now_millis(),
            },
            Err(_) => did_not_answer(REASON_INVALID_ANSWER, now_millis()),
        },
    }
}

/// The remotes to ask, and rows for the ones that cannot be. A remote that is
/// this node, or a second name for a site already listed, is skipped: one node
/// is one row. A pin that failed verification has no site, so its row is keyed
/// by its name and reads as did-not-answer.
fn plan_remote_asks(
    own_site_id: &str,
    report: RemotesLoadReport,
    as_of: &str,
) -> (Vec<RemoteConfig>, Vec<NodeInput>) {
    let mut named: Vec<RemoteConfig> = report
        .remotes
        .into_values()
        .map(|loaded| loaded.config)
        .collect();
    named.sort_by(|a, b| a.name.cmp(&b.name));
    let mut seen = BTreeSet::from([own_site_id.to_owned()]);
    let mut asks = Vec::new();
    for remote in named {
        if seen.insert(remote.site_id.clone()) {
            asks.push(remote);
        }
    }
    let mut invalid: Vec<_> = report.invalid;
    invalid.sort_by(|a, b| a.name.cmp(&b.name));
    let rows = invalid
        .into_iter()
        .map(|entry| NodeInput {
            id: pin_member_id(&entry.name),
            name: entry.name,
            kind: MemberKind::Remote,
            answer: Some(did_not_answer(REASON_REMOTE_INVALID, as_of.to_owned())),
        })
        .collect();
    (asks, rows)
}

/// The site id of the node whose app root is `app_root`, from its public
/// identity document. `None` when it cannot be read: a node that cannot be
/// named is listed rather than guessed at.
fn site_id_of_app_root(app_root: &Path) -> Option<String> {
    const MAX_IDENTITY_BYTES: u64 = 64 * 1024;
    let path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/public-identity.json");
    if std::fs::metadata(&path).ok()?.len() > MAX_IDENTITY_BYTES {
        return None;
    }
    let identity: ryeos_node_runtime::identity::PublicIdentityDoc =
        serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    Some(format!("site:{}", identity.verified_fingerprint().ok()?))
}

/// Node services installed as host services on this host that this node has
/// no remote for. `known_site_ids` holds this node and every remote.
fn installed_not_connected(
    own_app_root: &Path,
    known_site_ids: &BTreeSet<String>,
) -> Vec<NodeInput> {
    let Ok(Some(services)) = ryeos_node::supervision::host_services() else {
        return Vec::new();
    };
    let own = own_app_root.canonicalize().ok();
    let is_this_node = |root: &Path| {
        root == own_app_root
            || own
                .as_deref()
                .is_some_and(|own| root.canonicalize().is_ok_and(|root| root == own))
    };
    let mut rows: Vec<NodeInput> = services
        .into_iter()
        .filter(|service| service.problem.is_none())
        .filter(|service| {
            service.app_root.as_deref().is_some_and(|root| {
                !is_this_node(root)
                    && !site_id_of_app_root(root).is_some_and(|site| known_site_ids.contains(&site))
            })
        })
        .map(|service| NodeInput {
            id: installed_member_id(&service.name),
            name: service.name,
            kind: MemberKind::InstalledNotConnected,
            answer: None,
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows.dedup_by(|a, b| a.id == b.id);
    rows
}

/// The declarations the comparison may use, and a report for each record that
/// cannot be. A record that failed verification is already in `invalid`; the
/// records that name a node another also names are set aside here, all of them,
/// because nothing may pick which is right.
fn usable_declarations(
    report: DeclarationsReport,
) -> (Vec<MachineDeclaration>, Vec<InvalidDeclaration>) {
    let (usable, conflicting) = exclude_conflicting_declarations(
        report
            .declarations
            .into_iter()
            .map(|loaded| loaded.declaration)
            .collect(),
    );
    let mut invalid = report.invalid;
    invalid.extend(conflicting);
    invalid.sort_by(|a, b| a.label.cmp(&b.label));
    (usable, invalid)
}

/// The operator's machine declarations from the node-signed records.
fn declarations(app_root: &Path) -> Result<(Vec<MachineDeclaration>, Vec<InvalidDeclaration>)> {
    Ok(usable_declarations(machine_declaration::load_declarations(
        app_root,
    )?))
}

pub async fn handle(_req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    Ok(serde_json::to_value(compare_groups(ctx, state).await?)?)
}

/// The compared groups. The caller has already been checked for
/// [`CAPABILITY`]; this asks every configured remote under the caller's own
/// outbound principal, so a second reader of the groups sees what the node
/// service shows and nothing more.
pub async fn compare_groups(
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<ryeos_effect_contract::locality::LocalityGroups> {
    ctx.require_verified()?;
    let own_site_id = state.threads.site_id().to_string();
    let app_root = state.config.app_root.clone();
    let principal = OutboundPrincipal::resolve(None, &state, &ctx);

    let this_facts = {
        let root = app_root.clone();
        let site_id = own_site_id.clone();
        tokio::task::spawn_blocking(move || local_facts(&root, &site_id)).await?
    };
    let mut nodes = vec![NodeInput {
        id: own_site_id.clone(),
        name: THIS_NODE_NAME.to_owned(),
        kind: MemberKind::This,
        answer: Some(NodeAnswer::Answered {
            facts: this_facts,
            as_of: now_millis(),
        }),
    }];

    let report = config::load_remotes_report(&app_root)?;
    let (asks, unusable) = plan_remote_asks(&own_site_id, report, &now_millis());
    nodes.extend(unusable);

    let mut known_site_ids: BTreeSet<String> = asks.iter().map(|r| r.site_id.clone()).collect();
    known_site_ids.insert(own_site_id.clone());

    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_ASKS));
    let mut asked = JoinSet::new();
    for remote in asks {
        let permits = permits.clone();
        let state = state.clone();
        let ctx = ctx.clone();
        asked.spawn(async move {
            let _permit = permits.acquire_owned().await;
            let answer = ask_remote(&state, &ctx, principal, &remote).await;
            NodeInput {
                id: remote.site_id.clone(),
                name: remote.name.clone(),
                kind: MemberKind::Remote,
                answer: Some(answer),
            }
        });
    }
    while let Some(joined) = asked.join_next().await {
        match joined {
            Ok(node) => nodes.push(node),
            Err(error) => tracing::warn!(%error, "a locality ask ended abnormally"),
        }
    }

    let installed = {
        let root = app_root.clone();
        let known = known_site_ids.clone();
        tokio::task::spawn_blocking(move || installed_not_connected(&root, &known)).await?
    };
    nodes.extend(installed);

    for node in &nodes {
        validate_member_id(node.kind, &node.id)
            .map_err(|error| HandlerError::Internal(error.to_string()))?;
    }
    let (declared, invalid_declarations) = declarations(&app_root)?;
    let mut groups = compare_locality(&now_millis(), &nodes, &declared)
        .map_err(|error: LocalityError| HandlerError::Conflict(error.to_string()))?;
    groups.invalid_declarations = invalid_declarations;
    Ok(groups)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:node/locality/groups",
    endpoint: "node.locality.groups",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[CAPABILITY],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::config::{InvalidRemote, LoadedRemote, PinSource, Reachability};
    use std::collections::HashMap;

    const AS_OF: &str = "2026-11-18T00:00:00.000Z";

    fn remote(name: &str, site_id: &str) -> RemoteConfig {
        RemoteConfig {
            name: name.to_owned(),
            url: Some("http://127.0.0.1:1".to_owned()),
            reachability: Reachability::Outbound,
            principal_id: "fp:aa".to_owned(),
            signing_key: "ed25519:AA".to_owned(),
            site_id: site_id.to_owned(),
            vault_fingerprint: "sha256:aa".to_owned(),
            ingest_ignore: None,
            project_bindings: HashMap::new(),
        }
    }

    fn loaded(config: RemoteConfig) -> (String, LoadedRemote) {
        (
            config.name.clone(),
            LoadedRemote {
                config,
                config_path: "/x".into(),
                pin_source: PinSource::DescriptorWithoutContact,
                pinned_at: AS_OF.to_owned(),
            },
        )
    }

    #[test]
    fn a_remote_that_is_this_node_or_a_second_name_for_a_site_is_one_row() {
        let report = RemotesLoadReport {
            remotes: HashMap::from([
                loaded(remote("zeta", "site:b")),
                loaded(remote("alpha", "site:b")),
                loaded(remote("self", "site:own")),
                loaded(remote("other", "site:c")),
            ]),
            invalid: vec![InvalidRemote {
                name: "broken".to_owned(),
                config_path: "/x".into(),
                code: "remote_pin_invalid".to_owned(),
                error: "bad".to_owned(),
                url: None,
                repair_hint: String::new(),
            }],
        };
        let (asks, rows) = plan_remote_asks("site:own", report, AS_OF);
        let asked: Vec<_> = asks
            .iter()
            .map(|r| (r.name.as_str(), r.site_id.as_str()))
            .collect();
        // The first name in order keeps the site; this node is not asked.
        assert_eq!(asked, [("alpha", "site:b"), ("other", "site:c")]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "pin:broken");
        assert_eq!(
            rows[0].answer,
            Some(did_not_answer(REASON_REMOTE_INVALID, AS_OF.to_owned()))
        );
    }

    fn http_error(status: u16) -> anyhow::Error {
        anyhow::Error::new(RemoteHttpError {
            method: "POST",
            url: "http://x/execute".to_owned(),
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            body: String::new(),
        })
        .context("remote call")
    }

    #[test]
    fn a_refusal_is_not_granted_and_everything_else_did_not_answer() {
        let as_of = || AS_OF.to_owned();
        for status in [401, 403] {
            assert_eq!(
                answer_for_error(&http_error(status), as_of()),
                NodeAnswer::NotGranted { as_of: as_of() },
                "{status}"
            );
        }
        assert_eq!(
            answer_for_error(&http_error(404), as_of()),
            did_not_answer(REASON_SERVICE_NOT_INSTALLED, as_of())
        );
        assert_eq!(
            answer_for_error(&http_error(500), as_of()),
            did_not_answer(REASON_REMOTE_ERROR, as_of())
        );
        assert_eq!(
            answer_for_error(&anyhow::anyhow!("anything else"), as_of()),
            did_not_answer(REASON_REMOTE_ERROR, as_of())
        );
    }

    #[test]
    fn the_signed_item_and_the_descriptor_name_one_capability() {
        assert_eq!(DESCRIPTOR.required_caps, &[CAPABILITY]);
        assert_eq!(DESCRIPTOR.service_ref, "service:node/locality/groups");
    }

    fn declared(label: &str, members: &[&str]) -> crate::machine_declaration::LoadedDeclaration {
        crate::machine_declaration::LoadedDeclaration {
            declaration: MachineDeclaration {
                label: label.to_owned(),
                members: members.iter().map(|id| (*id).to_owned()).collect(),
            },
            declared_at: AS_OF.to_owned(),
            path: "/x".into(),
        }
    }

    #[test]
    fn conflicting_records_are_reported_per_record_and_the_view_still_compares() {
        let report = DeclarationsReport {
            declarations: vec![
                declared("one", &["site:a", "site:b"]),
                declared("two", &["site:b"]),
                declared("fine", &["site:c"]),
            ],
            invalid: vec![InvalidDeclaration {
                label: "edited".to_owned(),
                code: "machine_declaration_tampered".to_owned(),
                message: "edited after signing".to_owned(),
            }],
        };
        let (usable, invalid) = usable_declarations(report);
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].label, "fine");
        let labels: Vec<_> = invalid.iter().map(|entry| entry.label.as_str()).collect();
        assert_eq!(labels, ["edited", "one", "two"]);
        // What is left compares: no whole-view refusal.
        compare_locality(AS_OF, &[], &usable).unwrap();
    }

    #[test]
    fn member_ids_follow_the_prefixed_scheme() {
        assert_eq!(pin_member_id("broken"), "pin:broken");
        assert_eq!(installed_member_id("ryeos-ab"), "installed:ryeos-ab");
        assert!(validate_member_id(MemberKind::Remote, &pin_member_id("x")).is_ok());
        assert!(
            validate_member_id(MemberKind::InstalledNotConnected, &installed_member_id("x"))
                .is_ok()
        );
    }
}
