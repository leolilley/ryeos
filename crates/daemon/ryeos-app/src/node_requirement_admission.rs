//! Admission of the node guarantees a launch declares as data.
//!
//! An item's `requires.node` list and its declared credential kind's list
//! under `requires.credential_kinds`, and a structured-session profile's
//! `requires.controller` and `requires.credential_kinds`, name capabilities
//! the executing node must prove in its signed capability report. Each entry
//! is matched with Lillux `satisfies` against that report before any launch
//! side effect, and an unmet entry refuses the launch with the report's
//! reason for it. A Codex credential login, for one, needs a durable private
//! credential home here.
//!
//! A profile's `requires.lease` and `requires.guest` are not judged here:
//! external placement sends them to the lease adapter's admission before any
//! provider contact, and the guest list is judged against the guest's own
//! report when the guest channel delivers it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use lillux::process_control::{CapabilityRequirement, HostCapabilities, satisfies};
use serde_json::Value;

/// The credential kind a dedicated-session start binds: it names a
/// credential profile, owner-bound files kept in a node-private credential
/// home. A workload whose profile does not declare this kind cannot be
/// started on a credential profile.
pub const CREDENTIAL_PROFILE_KIND: &str = "credential_profile";

/// Reads the node's current signed capability report as the capabilities it
/// provides and the reasons it lacks the others. The daemon composition root
/// supplies the reader; its own documentation states which facts it
/// re-observes at each read and which are the snapshot the node took when it
/// started.
pub struct NodeCapabilityReportReader {
    read: Box<dyn Fn() -> Result<HostCapabilities> + Send + Sync>,
}

impl NodeCapabilityReportReader {
    pub fn new(read: impl Fn() -> Result<HostCapabilities> + Send + Sync + 'static) -> Self {
        Self { read: Box::new(read) }
    }

    pub fn current(&self) -> Result<HostCapabilities> {
        (self.read)()
    }
}

/// Where admission finds the composition root's report reader: the daemon's
/// application state, or the extension bag that state carries.
pub trait NodeCapabilitySource {
    fn node_capability_report_reader(&self) -> Option<Arc<NodeCapabilityReportReader>>;
}

impl NodeCapabilitySource for crate::extension_state::ExtensionState {
    fn node_capability_report_reader(&self) -> Option<Arc<NodeCapabilityReportReader>> {
        self.get::<NodeCapabilityReportReader>()
    }
}

impl NodeCapabilitySource for crate::state::AppState {
    fn node_capability_report_reader(&self) -> Option<Arc<NodeCapabilityReportReader>> {
        self.extensions.node_capability_report_reader()
    }
}

/// The node's current capabilities from the composition root's report
/// reader. A node composed without one has no report to admit against, so
/// a launch that declares requirements is refused rather than admitted: a
/// [`NodeRequirementRefused`]. A reader failure is classified by
/// [`classify_report_failure`].
pub fn current_node_capabilities(state: &impl NodeCapabilitySource) -> Result<HostCapabilities> {
    let Some(reader) = state.node_capability_report_reader() else {
        bail!(NodeRequirementRefused(
            "launch refused: this node has no signed capability report to prove what the launch \
             requires"
                .to_owned()
        ));
    };
    reader.current().map_err(classify_report_failure)
}

/// Classify a report reader's failure by whether it can clear while this
/// daemon runs.
///
/// Only an I/O failure, or one the reader typed as a
/// [`NodeReportUnreadable`], can: the same report may read once the
/// filesystem answers again, so it is a [`NodeReportUnreadable`]. Every
/// other failure is a fact about the recorded report that no retry
/// changes until the node restarts and records a new one: no report is
/// recorded, its digest or signature does not verify, it is of a stale
/// format, or it describes another host or policy generation. Those, and
/// a failure the reader already typed as a refusal, are a
/// [`NodeRequirementRefused`].
fn classify_report_failure(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<NodeRequirementRefused>().is_some() {
        return error;
    }
    let transient = error
        .chain()
        .any(|cause| cause.is::<NodeReportUnreadable>() || cause.is::<std::io::Error>());
    if transient {
        anyhow::Error::new(NodeReportUnreadable(format!("{error:#}")))
            .context("launch refused: read the node's signed capability report")
    } else {
        anyhow::Error::new(NodeRequirementRefused(format!(
            "launch refused: the node's signed capability report does not stand: {error:#}"
        )))
    }
}

/// Whether `error` is a node report that could not be read at this moment,
/// as opposed to a refusal or any other failure.
pub fn is_report_unreadable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<NodeReportUnreadable>())
}

/// How long one execution's launch may keep meeting an unreadable report
/// before that unreadability refuses it.
pub const UNREADABLE_REPORT_RETRY_WINDOW: Duration = Duration::from_secs(10 * 60);

static UNREADABLE_REPORT_EPISODES: Mutex<UnreadableReportEpisodes> =
    Mutex::new(UnreadableReportEpisodes::new());

/// Bound the re-drives of one execution row whose launch admission keeps
/// meeting an unreadable report. A [`NodeReportUnreadable`] leaves a
/// recovered row recoverable, and its recovery owner re-drives it; once
/// the row has met one for [`UNREADABLE_REPORT_RETRY_WINDOW`] without a
/// readable report in between, its launch is refused instead.
///
/// The episode is held in this daemon process only. A restarted node
/// records a fresh signed report before it admits any work, so an episode
/// from before the restart says nothing about the report the next attempt
/// reads, and the row starts a new window.
pub fn bound_unreadable_report_retries<T>(row: &str, result: Result<T>) -> Result<T> {
    UNREADABLE_REPORT_EPISODES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .judge(row, Instant::now(), UNREADABLE_REPORT_RETRY_WINDOW, result)
}

/// The rows whose launch admission is meeting an unreadable report, each
/// with when it first met one and how many attempts have met one since.
#[derive(Debug, Default)]
pub struct UnreadableReportEpisodes {
    rows: BTreeMap<String, UnreadableReportEpisode>,
}

#[derive(Debug)]
struct UnreadableReportEpisode {
    since: Instant,
    last: Instant,
    attempts: u32,
}

impl UnreadableReportEpisodes {
    pub const fn new() -> Self {
        Self {
            rows: BTreeMap::new(),
        }
    }

    /// Judge one admission `result` for `row` at `now`. Any outcome other
    /// than an unreadable report ends the row's episode and is returned
    /// unchanged. An unreadable report is returned unchanged until the row
    /// has met one for `window`; it is then refused with how long and how
    /// often the report stayed unreadable.
    pub fn judge<T>(
        &mut self,
        row: &str,
        now: Instant,
        window: Duration,
        result: Result<T>,
    ) -> Result<T> {
        // An episode no attempt has touched for two windows belongs to a
        // row nothing re-drives any more.
        self.rows
            .retain(|_, episode| now.saturating_duration_since(episode.last) < window * 2);
        let error = match result {
            Ok(value) => {
                self.rows.remove(row);
                return Ok(value);
            }
            Err(error) if !is_report_unreadable(&error) => {
                self.rows.remove(row);
                return Err(error);
            }
            Err(error) => error,
        };
        let episode = self
            .rows
            .entry(row.to_owned())
            .or_insert(UnreadableReportEpisode {
                since: now,
                last: now,
                attempts: 0,
            });
        episode.last = now;
        episode.attempts = episode.attempts.saturating_add(1);
        let unreadable_for = now.saturating_duration_since(episode.since);
        if unreadable_for < window {
            return Err(error);
        }
        let attempts = episode.attempts;
        self.rows.remove(row);
        Err(anyhow::Error::new(NodeRequirementRefused(format!(
            "launch refused: the node's signed capability report stayed unreadable for {}s \
             across {attempts} launch attempts of {row}: {error:#}",
            unreadable_for.as_secs()
        ))))
    }
}

/// The node's capability report could not be read at this moment. Unlike a
/// [`NodeRequirementRefused`], it is not a fact about what the node proves:
/// the same launch may be admitted once the report reads again, so it must
/// never permanently fail an execution that is otherwise recoverable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeReportUnreadable(pub String);

impl std::fmt::Display for NodeReportUnreadable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NodeReportUnreadable {}

/// Which proof in the node's report met one requirement entry: the
/// capability name that met it (for a set, the first of its names that is
/// met), the grade it is proven at, and the provider that proved it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RequirementMetBy {
    pub name: String,
    pub grade: String,
    pub provider: String,
}

/// Every admitted requirement entry by list (`requires.node`,
/// `requires.credential_kinds.<kind>`) and then by entry label, with the
/// proof that met it. Empty when the item declares no requirement.
pub type RequirementsMet = BTreeMap<String, BTreeMap<String, RequirementMetBy>>;

/// A launch declares a guarantee this node does not prove. The text names
/// every unmet entry with the report's reason. It is a fact of this node:
/// launching again changes nothing until the node or the declaration does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRequirementRefused(pub String);

impl std::fmt::Display for NodeRequirementRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NodeRequirementRefused {}

/// Admit an item's `requires.node` list and its declared credential kind's
/// list against the node's current report. An item declares at most one
/// credential kind, the one its launch binds, so that kind's list is the one
/// judged and no other kind's ever stands in for it. An item that declares
/// neither list is admitted without reading the report.
pub fn admit_item_requirements(
    state: &impl NodeCapabilitySource,
    item_ref: &str,
    requires: &ryeos_bundle::runtime_authority::RuntimeRequires,
) -> Result<()> {
    admit_item_requirements_met_by(state, item_ref, requires).map(drop)
}

/// [`admit_item_requirements`], returning which proof met each entry so the
/// launch can record it.
pub fn admit_item_requirements_met_by(
    state: &impl NodeCapabilitySource,
    item_ref: &str,
    requires: &ryeos_bundle::runtime_authority::RuntimeRequires,
) -> Result<RequirementsMet> {
    if let Some(reason) =
        ryeos_bundle::runtime_authority::credential_kinds_refusal(&requires.credential_kinds)
    {
        bail!(NodeRequirementRefused(format!("launch refused: {item_ref}: {reason}")));
    }
    if requires.node.is_empty() && requires.credential_kinds.is_empty() {
        return Ok(RequirementsMet::new());
    }
    let view = current_node_capabilities(state)?;
    judge_item_requirements(item_ref, requires, &view)
}

fn judge_item_requirements(
    item_ref: &str,
    requires: &ryeos_bundle::runtime_authority::RuntimeRequires,
    view: &HostCapabilities,
) -> Result<RequirementsMet> {
    let kind_lists: Vec<(String, &[CapabilityRequirement])> = requires
        .credential_kinds
        .iter()
        .map(|(kind, entries)| (format!("requires.credential_kinds.{kind}"), entries.as_slice()))
        .collect();
    let mut lists: Vec<(&str, &[CapabilityRequirement])> =
        vec![("requires.node", requires.node.as_slice())];
    lists.extend(kind_lists.iter().map(|(list, entries)| (list.as_str(), *entries)));
    refuse_unmet(item_ref, &lists, view)
}

/// Admit a structured-session profile's controller requirements while its
/// session is admitted, before any credential binding is known. A profile
/// that declares no placement requirements is admitted without reading the
/// report.
pub fn admit_structured_session_controller(
    state: &impl NodeCapabilitySource,
    contract: &Value,
) -> Result<()> {
    let Some(requirements) =
        ryeos_engine::structured_session_profile::placement_requirements(contract)?
    else {
        return Ok(());
    };
    let view = current_node_capabilities(state)?;
    refuse_unmet(
        &requirements.workload_class,
        &[("requires.controller", requirements.controller.as_slice())],
        &view,
    )
    .map(drop)
}

/// Admit a structured-session start: the controller requirements, and the
/// requirement list of exactly the credential kind the start's binding
/// names. A binding that names no kind, or a kind the profile does not
/// declare, is refused; no other declared kind is ever evaluated in its
/// place.
pub fn admit_structured_session_start(
    state: &impl NodeCapabilitySource,
    contract: &Value,
    binding_kind: Option<&str>,
) -> Result<()> {
    let Some(requirements) =
        ryeos_engine::structured_session_profile::placement_requirements(contract)?
    else {
        return Ok(());
    };
    let kind_requirements = select_credential_kind(&requirements, binding_kind)?;
    let kind_list = format!("requires.credential_kinds.{}", binding_kind.unwrap_or_default());
    let view = current_node_capabilities(state)?;
    refuse_unmet(
        &requirements.workload_class,
        &[
            ("requires.controller", requirements.controller.as_slice()),
            (kind_list.as_str(), kind_requirements),
        ],
        &view,
    )
    .map(drop)
}

fn select_credential_kind<'a>(
    requirements: &'a ryeos_engine::structured_session_profile::PlacementRequirements,
    binding_kind: Option<&str>,
) -> Result<&'a [CapabilityRequirement]> {
    let declared = requirements
        .credential_kinds
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let Some(kind) = binding_kind else {
        return Err(NodeRequirementRefused(format!(
            "launch refused: {} supports credential kinds {declared}, and the launch's credential \
             binding names no credential kind",
            requirements.workload_class
        ))
        .into());
    };
    match requirements.credential_kinds.get(kind) {
        Some(entries) => Ok(entries),
        None => Err(NodeRequirementRefused(format!(
            "launch refused: {} supports credential kinds {declared}; the launch's credential \
             binding names {kind}, which it does not declare",
            requirements.workload_class
        ))
        .into()),
    }
}

/// Refuse when any entry of any list is unmet, naming each with its reason;
/// otherwise return which proof met each entry.
fn refuse_unmet(
    subject: &str,
    lists: &[(&str, &[CapabilityRequirement])],
    view: &HostCapabilities,
) -> Result<RequirementsMet> {
    let mut text = String::new();
    for (list, entries) in lists {
        for (label, reason) in unmet_entries(entries, view) {
            text.push_str(&format!("\n  {list} {label}: {reason}"));
        }
    }
    if text.is_empty() {
        return Ok(lists
            .iter()
            .filter(|(_, entries)| !entries.is_empty())
            .map(|(list, entries)| ((*list).to_owned(), met_entries(entries, view)))
            .collect());
    }
    let proven: Vec<&str> = view.provided.keys().map(String::as_str).collect();
    bail!(NodeRequirementRefused(format!(
        "launch refused: this node cannot prove what {subject} requires{text}\n  this node can \
         prove: {}",
        if proven.is_empty() { "nothing".to_owned() } else { proven.join(", ") }
    )));
}

/// Each unmet entry, by its label, with the reason. A name is met only by
/// the exact named capability at a grade and from a provider the entry
/// accepts; a set is met by any one of its names.
fn unmet_entries(
    entries: &[CapabilityRequirement],
    view: &HostCapabilities,
) -> BTreeMap<String, String> {
    let mut unmet = BTreeMap::new();
    for entry in entries {
        let failures: Vec<(&str, String)> = entry
            .names()
            .iter()
            .map(|name| (name.as_str(), unmet_reason(entry, name, view)))
            .take_while(|(_, reason)| reason.is_some())
            .map(|(name, reason)| (name, reason.unwrap_or_default()))
            .collect();
        if failures.len() < entry.names().len() {
            continue;
        }
        let reason = match failures.as_slice() {
            [(_, reason)] if !entry.is_set() => reason.clone(),
            _ => format!(
                "none of its names is met; {}",
                failures
                    .iter()
                    .map(|(name, reason)| format!("{name}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        };
        unmet.insert(entry.label(), reason);
    }
    unmet
}

/// Each met entry, by its label, with the first of its names that is met
/// and that name's proof.
fn met_entries(
    entries: &[CapabilityRequirement],
    view: &HostCapabilities,
) -> BTreeMap<String, RequirementMetBy> {
    entries
        .iter()
        .filter_map(|entry| {
            entry.names().iter().find_map(|name| {
                let evidence = view.provided.get(name.as_str())?;
                satisfies(entry, name, evidence).ok()?;
                Some((
                    entry.label(),
                    RequirementMetBy {
                        name: name.clone(),
                        grade: evidence.source.as_str().to_owned(),
                        provider: evidence.provider.clone(),
                    },
                ))
            })
        })
        .collect()
}

fn unmet_reason(entry: &CapabilityRequirement, name: &str, view: &HostCapabilities) -> Option<String> {
    match view.provided.get(name) {
        Some(evidence) => satisfies(entry, name, evidence)
            .err()
            .map(|mismatch| format!("{mismatch} ({})", evidence.basis)),
        None => Some(
            view.missing
                .get(name)
                .cloned()
                .unwrap_or_else(|| format!("this node's report does not name {name}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use lillux::process_control::{CapabilityRequirementSpec, HOST_PROVIDER, ProofSource};
    use serde_json::json;

    use crate::extension_state::ExtensionState;

    fn view(provided: &[(&str, ProofSource)], missing: &[(&str, &str)]) -> HostCapabilities {
        let mut view = HostCapabilities::default();
        for (name, source) in provided {
            view.provide(name, source.clone(), HOST_PROVIDER, "fixture basis");
        }
        for (name, reason) in missing {
            view.refuse(name, *reason);
        }
        view
    }

    /// The extension bag the daemon's application state carries, with the
    /// composition root's report reader registered over `report`.
    fn node_reporting(report: HostCapabilities) -> ExtensionState {
        let extensions = ExtensionState::new();
        extensions.insert(Arc::new(NodeCapabilityReportReader::new(move || Ok(report.clone()))));
        extensions
    }

    /// A node whose recorded report no longer describes it.
    fn node_with_stale_report() -> ExtensionState {
        let extensions = ExtensionState::new();
        extensions.insert(Arc::new(NodeCapabilityReportReader::new(|| {
            anyhow::bail!("the recorded node capability report is stale: the host restarted")
        })));
        extensions
    }

    /// A node whose report cannot be read at this moment.
    fn node_with_unreadable_report() -> ExtensionState {
        let extensions = ExtensionState::new();
        extensions.insert(Arc::new(NodeCapabilityReportReader::new(|| {
            Err(anyhow::Error::new(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "the state directory did not answer",
            ))
            .context("open the recorded node capability report"))
        })));
        extensions
    }

    /// The `requires` block of one shipped worker execution, read from the
    /// signed bundle file itself.
    fn shipped_requires(file: &str) -> ryeos_bundle::runtime_authority::RuntimeRequires {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/codex/.ai/worker-executions/codex")
            .join(file);
        let document: serde_yaml::Value =
            serde_yaml::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let requires = serde_json::to_value(&document["requires"]).unwrap();
        ryeos_bundle::runtime_authority::parse_runtime_requires_block(&requires)
            .unwrap_or_else(|reason| panic!("{}: {reason}", path.display()))
    }

    fn login_requires() -> ryeos_bundle::runtime_authority::RuntimeRequires {
        shipped_requires("login.yaml")
    }

    fn refusal(error: anyhow::Error) -> String {
        error
            .downcast_ref::<NodeRequirementRefused>()
            .unwrap_or_else(|| panic!("not a requirement refusal: {error:#}"))
            .0
            .clone()
    }

    fn login_node() -> HostCapabilities {
        view(
            &[
                ("credential:private_home", ProofSource::Trusted),
                ("persistence:durable_state", ProofSource::Trusted),
                ("time", ProofSource::Measured),
            ],
            &[],
        )
    }

    #[test]
    fn the_shipped_logins_declare_one_credential_profile_kind() {
        for file in ["login.yaml", "trusted-login.yaml"] {
            let requires = shipped_requires(file);
            let node: Vec<String> = requires.node.iter().map(|entry| entry.label()).collect();
            assert_eq!(node, ["persistence:durable_state", "time"], "{file}");
            assert_eq!(
                requires.credential_kinds.keys().collect::<Vec<_>>(),
                [CREDENTIAL_PROFILE_KIND],
                "{file}"
            );
            let kind: Vec<String> = requires.credential_kinds[CREDENTIAL_PROFILE_KIND]
                .iter()
                .map(|entry| entry.label())
                .collect();
            assert_eq!(kind, ["credential:private_home"], "{file}");
            admit_item_requirements(&node_reporting(login_node()), file, &requires).unwrap();
        }
    }

    #[test]
    fn admission_records_which_proof_met_each_entry() {
        let mut report = login_node();
        report.provide(
            "recovery:scope",
            ProofSource::ProviderAttested,
            "fixture-provider",
            "fixture basis",
        );
        let mut requires = login_requires();
        requires.node.push(CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: None,
            any_of: vec!["recovery:enclosure".into(), "recovery:scope".into()],
            accept: vec!["provider_attested".into()],
            providers: Vec::new(),
        }));
        let met = admit_item_requirements_met_by(&node_reporting(report), "login", &requires)
            .unwrap();
        let met_by = |list: &str, label: &str| {
            let met = &met[list][label];
            (met.name.as_str(), met.grade.as_str(), met.provider.as_str())
        };
        assert_eq!(
            met_by("requires.node", "time"),
            ("time", "measured", HOST_PROVIDER)
        );
        assert_eq!(
            met_by("requires.node", "persistence:durable_state"),
            ("persistence:durable_state", "trusted", HOST_PROVIDER)
        );
        assert_eq!(
            met_by("requires.node", "any_of(recovery:enclosure, recovery:scope)"),
            ("recovery:scope", "provider_attested", "fixture-provider")
        );
        assert_eq!(
            met_by("requires.credential_kinds.credential_profile", "credential:private_home"),
            ("credential:private_home", "trusted", HOST_PROVIDER)
        );
        assert_eq!(met.len(), 2);
        assert_eq!(met["requires.node"].len(), 3);
        // An item that declares nothing records nothing.
        assert!(
            admit_item_requirements_met_by(
                &ExtensionState::new(),
                "plain",
                &ryeos_bundle::runtime_authority::RuntimeRequires::default(),
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn item_requirements_refuse_with_the_reports_missing_reason() {
        let report = view(
            &[
                ("persistence:durable_state", ProofSource::Trusted),
                ("time", ProofSource::Measured),
            ],
            &[("credential:private_home", "credential home root: group-writable")],
        );
        let text = refusal(
            admit_item_requirements(
                &node_reporting(report),
                "worker_execution:codex/login",
                &login_requires(),
            )
            .unwrap_err(),
        );
        assert!(
            text.starts_with(
                "launch refused: this node cannot prove what worker_execution:codex/login requires"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "requires.credential_kinds.credential_profile credential:private_home: credential \
                 home root: group-writable"
            ),
            "{text}"
        );
        assert!(!text.contains("requires.node"), "{text}");
        assert!(text.contains("this node can prove: persistence:durable_state, time"), "{text}");
    }

    #[test]
    fn a_weaker_grade_does_not_meet_a_measured_entry() {
        let report = view(
            &[
                ("credential:private_home", ProofSource::Trusted),
                ("persistence:durable_state", ProofSource::Trusted),
                ("time", ProofSource::Trusted),
            ],
            &[],
        );
        let text = refusal(
            admit_item_requirements(
                &node_reporting(report),
                "worker_execution:codex/login",
                &login_requires(),
            )
            .unwrap_err(),
        );
        assert!(text.contains("requires.node time:"), "{text}");
        assert!(!text.contains("persistence:durable_state:"), "{text}");
        assert!(text.contains("(fixture basis)"), "{text}");
    }

    #[test]
    fn an_item_with_requirements_is_refused_without_a_current_report() {
        let missing = refusal_text(
            admit_item_requirements(&ExtensionState::new(), "login", &login_requires())
                .unwrap_err(),
        );
        assert!(missing.contains("this node has no signed capability report"), "{missing}");
        let stale = refusal_text(
            admit_item_requirements(&node_with_stale_report(), "login", &login_requires())
                .unwrap_err(),
        );
        assert!(stale.contains("the host restarted"), "{stale}");
        // No report at all, and a report that no longer describes this
        // node, are facts no retry changes while the daemon runs: both are
        // refusals. Only a report that could not be read now is typed
        // apart, so it never stands in for a refusal.
        let missing = admit_item_requirements(&ExtensionState::new(), "login", &login_requires())
            .unwrap_err();
        assert!(missing.downcast_ref::<NodeRequirementRefused>().is_some(), "{missing:#}");
        let stale = admit_item_requirements(&node_with_stale_report(), "login", &login_requires())
            .unwrap_err();
        assert!(stale.downcast_ref::<NodeRequirementRefused>().is_some(), "{stale:#}");
        assert!(!is_report_unreadable(&stale), "{stale:#}");
        let unreadable =
            admit_item_requirements(&node_with_unreadable_report(), "login", &login_requires())
                .unwrap_err();
        assert!(unreadable.downcast_ref::<NodeRequirementRefused>().is_none(), "{unreadable:#}");
        assert!(is_report_unreadable(&unreadable), "{unreadable:#}");
        let text = refusal_text(unreadable);
        assert!(text.contains("read the node's signed capability report"), "{text}");
        assert!(text.contains("the state directory did not answer"), "{text}");
        // An item that declares nothing is admitted without any report.
        admit_item_requirements(
            &ExtensionState::new(),
            "plain",
            &ryeos_bundle::runtime_authority::RuntimeRequires::default(),
        )
        .unwrap();
    }

    #[test]
    fn an_item_declaring_several_credential_kinds_is_refused_unjudged() {
        let mut requires = login_requires();
        requires.credential_kinds.insert(
            "provider_env".to_owned(),
            vec![CapabilityRequirement::from("secrets:provider_env")],
        );
        let text = refusal(
            admit_item_requirements(&node_reporting(login_node()), "login", &requires)
                .unwrap_err(),
        );
        assert!(text.contains("may declare at most 1"), "{text}");
    }

    fn refusal_text(error: anyhow::Error) -> String {
        format!("{error:#}")
    }

    #[test]
    fn a_set_is_met_by_any_name_and_refused_with_every_reason() {
        let entry = CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: None,
            any_of: vec!["recovery:enclosure".into(), "recovery:scope".into()],
            accept: Vec::new(),
            providers: Vec::new(),
        });
        let met = view(&[("recovery:scope", ProofSource::Measured)], &[]);
        assert!(unmet_entries(std::slice::from_ref(&entry), &met).is_empty());
        let unmet = unmet_entries(
            std::slice::from_ref(&entry),
            &view(&[], &[("recovery:enclosure", "no enclosure"), ("recovery:scope", "no scope")]),
        );
        assert_eq!(
            unmet["any_of(recovery:enclosure, recovery:scope)"],
            "none of its names is met; recovery:enclosure: no enclosure; recovery:scope: no scope"
        );
    }

    fn remote_contract() -> Value {
        json!({
            "external_candidate": {"schema": 7},
            "requires": {
                "workload_class": "codex.authoring.remote",
                "controller": [
                    {"name": "persistence:durable_state", "accept": ["measured", "provider_attested", "trusted"]},
                    "time",
                ],
                "credential_kinds": {
                    "credential_profile": [
                        {"name": "credential:private_home", "accept": ["measured", "provider_attested", "trusted"]},
                    ],
                    "provider_env": [{"name": "secrets:provider_env", "accept": ["provider_attested"]}],
                },
                "lease": ["lease.create_bound"],
            },
        })
    }

    #[test]
    fn structured_session_start_evaluates_only_the_bound_kind() {
        let node = node_reporting(view(
            &[
                ("credential:private_home", ProofSource::Trusted),
                ("persistence:durable_state", ProofSource::Trusted),
                ("time", ProofSource::Measured),
            ],
            &[("secrets:provider_env", "declared names absent at startup: OPENAI_API_KEY")],
        ));
        let contract = remote_contract();
        admit_structured_session_start(&node, &contract, Some(CREDENTIAL_PROFILE_KIND)).unwrap();
        // The API-key kind is never satisfied by the profile kind's proof.
        let text = refusal(
            admit_structured_session_start(&node, &contract, Some("provider_env")).unwrap_err(),
        );
        assert!(
            text.contains(
                "requires.credential_kinds.provider_env secrets:provider_env: declared names \
                 absent at startup: OPENAI_API_KEY"
            ),
            "{text}"
        );
        assert!(text.contains("codex.authoring.remote"), "{text}");
        // The lease list is judged at placement by the lease adapter, not
        // here: an unattested lease name does not refuse the start.
        assert!(!text.contains("lease.create_bound"), "{text}");
    }

    #[test]
    fn structured_session_start_refuses_an_unnamed_or_undeclared_kind() {
        let node = node_reporting(view(&[], &[]));
        let contract = remote_contract();
        let text =
            refusal(admit_structured_session_start(&node, &contract, None).unwrap_err());
        assert!(text.contains("names no credential kind"), "{text}");
        let text = refusal(
            admit_structured_session_start(&node, &contract, Some("device_token")).unwrap_err(),
        );
        assert!(
            text.contains(
                "supports credential kinds credential_profile, provider_env; the launch's \
                 credential binding names device_token, which it does not declare"
            ),
            "{text}"
        );
    }

    #[test]
    fn structured_session_controller_refusal_names_the_report_reason() {
        let node = node_reporting(view(
            &[
                ("credential:private_home", ProofSource::Trusted),
                ("time", ProofSource::Measured),
            ],
            &[("persistence:durable_state", "app root is on tmpfs")],
        ));
        let contract = remote_contract();
        for error in [
            admit_structured_session_controller(&node, &contract).unwrap_err(),
            admit_structured_session_start(&node, &contract, Some(CREDENTIAL_PROFILE_KIND))
                .unwrap_err(),
        ] {
            let text = refusal(error);
            assert!(
                text.contains(
                    "requires.controller persistence:durable_state: app root is on tmpfs"
                ),
                "{text}"
            );
        }
        // The controller admission judges no credential kind: the missing
        // provider environment does not refuse it once the controller holds.
        let held = node_reporting(login_node());
        admit_structured_session_controller(&held, &contract).unwrap();
    }

    #[test]
    fn structured_session_admission_is_refused_without_a_current_report() {
        let contract = remote_contract();
        for node in [
            ExtensionState::new(),
            node_with_stale_report(),
            node_with_unreadable_report(),
        ] {
            let controller =
                refusal_text(admit_structured_session_controller(&node, &contract).unwrap_err());
            assert!(controller.starts_with("launch refused:"), "{controller}");
            let start = refusal_text(
                admit_structured_session_start(&node, &contract, Some(CREDENTIAL_PROFILE_KIND))
                    .unwrap_err(),
            );
            assert!(start.starts_with("launch refused:"), "{start}");
        }
        // A profile that declares nothing is admitted without any report.
        admit_structured_session_controller(&ExtensionState::new(), &json!({})).unwrap();
        admit_structured_session_start(&ExtensionState::new(), &json!({}), None).unwrap();
    }

    #[test]
    fn an_unreadable_report_defers_a_row_only_for_the_retry_window() {
        let window = Duration::from_secs(600);
        let start = Instant::now();
        let unreadable = || -> Result<()> {
            admit_item_requirements(&node_with_unreadable_report(), "login", &login_requires())
        };
        let mut episodes = UnreadableReportEpisodes::new();
        // Inside the window the row stays recoverable at every attempt.
        for minute in [0, 4, 9] {
            let error = episodes
                .judge("T-row", start + Duration::from_secs(minute * 60), window, unreadable())
                .unwrap_err();
            assert!(is_report_unreadable(&error), "{error:#}");
        }
        // Another row's episode is its own.
        let other = episodes
            .judge("T-other", start + Duration::from_secs(10 * 60), window, unreadable())
            .unwrap_err();
        assert!(is_report_unreadable(&other), "{other:#}");
        // Once the row has met an unreadable report for the whole window,
        // its launch is refused, plainly and permanently.
        let refused = episodes
            .judge("T-row", start + Duration::from_secs(10 * 60), window, unreadable())
            .unwrap_err();
        assert!(!is_report_unreadable(&refused), "{refused:#}");
        let text = refused
            .downcast_ref::<NodeRequirementRefused>()
            .unwrap_or_else(|| panic!("not a refusal: {refused:#}"))
            .0
            .clone();
        assert!(
            text.contains("stayed unreadable for 600s across 4 launch attempts of T-row"),
            "{text}"
        );
        assert!(text.contains("the state directory did not answer"), "{text}");

        // A readable report in between ends the episode: the next
        // unreadable report opens a new window.
        let mut episodes = UnreadableReportEpisodes::new();
        episodes.judge("T-row", start, window, unreadable()).unwrap_err();
        episodes
            .judge("T-row", start + Duration::from_secs(300), window, Ok(()))
            .unwrap();
        let error = episodes
            .judge("T-row", start + Duration::from_secs(700), window, unreadable())
            .unwrap_err();
        assert!(is_report_unreadable(&error), "{error:#}");
        // So does a refusal, which is returned unchanged.
        let stale = admit_item_requirements(&node_with_stale_report(), "login", &login_requires());
        let error = episodes
            .judge("T-row", start + Duration::from_secs(1400), window, stale)
            .unwrap_err();
        assert!(error.downcast_ref::<NodeRequirementRefused>().is_some(), "{error:#}");
        let error = episodes
            .judge("T-row", start + Duration::from_secs(1500), window, unreadable())
            .unwrap_err();
        assert!(is_report_unreadable(&error), "{error:#}");
    }
}
