//! Signed node capability report: what this node contains, what its host
//! proves, and whether the node's declared capability floor is met.
//!
//! The report is evidence for operators and, later, for peers choosing where
//! work may run. It is never authority: admission re-checks live facts at
//! startup and at invocation. Capability names are open strings so a report
//! can carry names this build does not know, and a requirement that names an
//! unknown capability is refused rather than ignored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use lillux::process_control::{
    CapabilityEvidence, CapabilityRequirement, HostCapabilities, HostFacts, ProofSource,
    ResidencyProof, SCOPE_LIMITS, is_scope_capability,
};
use serde::{Deserialize, Serialize};

pub const CAPABILITY_REPORT_SCHEMA: &str = "ryeos.node.capability-report/v3";
const REPORT_FILE: &str = "node-capability-report.json";
const MAX_REPORT_BYTES: u64 = 1024 * 1024;

/// The host generation the report describes. A report from another boot or
/// enclosure is stale and must not be used to admit anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportGeneration {
    pub boot_id: String,
    pub cgroup_enclosure: Option<u64>,
}

/// What this node contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeComposition {
    /// Initialization profile, when the node recorded one.
    pub profile: Option<String>,
    pub bundles: Vec<String>,
    /// Every optional feature compiled into this daemon binary.
    pub compiled_features: Vec<String>,
    pub policy_generation: String,
}

/// How this node can be reached. Exactly one of `ingress` and
/// `ingress_unattested` is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeExposure {
    /// Addresses the node's listeners are bound to, as bound.
    pub listeners: Vec<String>,
    /// What fronts the listeners, as attested by whoever can see it.
    pub ingress: Option<CapabilityEvidence>,
    /// Why nothing about the ingress is attested.
    pub ingress_unattested: Option<String>,
}

impl NodeExposure {
    /// The exposure of `listeners` when no provider attests what fronts
    /// them: a listener is visible to this node, its ingress is not.
    pub fn unattested(listeners: Vec<String>) -> Self {
        Self {
            listeners,
            ingress: None,
            ingress_unattested: Some("no provider attests what fronts this listener".to_owned()),
        }
    }
}

/// The host does not prove what the signed capability floor requires. It is
/// a deterministic fact of this host and policy: starting again changes
/// nothing until the host or the policy does. The text names every unmet
/// entry with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityFloorRefused(pub String);

impl std::fmt::Display for CapabilityFloorRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CapabilityFloorRefused {}

impl CapabilityFloorRefused {
    /// The floor refusal anywhere in `error`'s chain, if there is one.
    pub fn find(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

/// Result of matching the declared floor against what the host proves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloorAdmission {
    pub requires: Vec<CapabilityRequirement>,
    pub admitted: bool,
    /// Each met `any_of` entry, by its label, with the name that met it, so
    /// an admission on a weaker grade the floor names is always visible.
    pub met_by: BTreeMap<String, String>,
    /// Each unmet requirement, by its label, with the plain-language reason.
    pub unmet: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCapabilityReport {
    pub schema: String,
    pub node_fingerprint: String,
    /// Digest of the daemon executable image that assembled this report.
    pub daemon_digest: String,
    pub generated_at: String,
    pub generation: ReportGeneration,
    /// Who the guarantees apply to. `node` means node-wide; a narrower
    /// principal or job qualifier can be named here without a schema change.
    pub subject: String,
    pub composition: NodeComposition,
    pub exposure: NodeExposure,
    pub host: HostFacts,
    pub provided: BTreeMap<String, CapabilityEvidence>,
    pub missing: BTreeMap<String, String>,
    /// Budgets a placement could reserve against, by open name.
    pub capacity: BTreeMap<String, u64>,
    pub floor: FloorAdmission,
    /// The residency proof the native region actually used, recorded after
    /// the reservation made it resident. `host.residency` is the plan made
    /// before any debit; the two differ only if the kernel refused a lock
    /// the limits allowed and the swap proof was used instead.
    pub established_residency: Option<ResidencyProof>,
}

/// The report, its canonical digest, and the node key's signature over the
/// digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedNodeCapabilityReport {
    pub report: NodeCapabilityReport,
    pub digest: String,
    pub signer: String,
    pub signature: String,
}

/// Inputs from the running node that the host facts cannot know.
pub struct ReportInputs<'a> {
    pub app_root: &'a Path,
    pub node_fingerprint: &'a str,
    /// Digest of the running daemon executable.
    pub daemon_digest: String,
    pub bundles: Vec<String>,
    pub compiled_features: Vec<String>,
    /// Addresses the node's listeners are bound to; empty for a process
    /// that serves no listener.
    pub listeners: Vec<String>,
    pub policy_generation: String,
    pub isolation: &'a ryeos_engine::isolation::IsolationInspection,
    /// Whether a protected host-runtime binding (host service or external
    /// controller) delegated a cgroup subtree to this node.
    pub host_runtime_present: bool,
}

impl NodeCapabilityReport {
    /// Observe the host now and assemble the report for this node.
    pub fn assemble(inputs: ReportInputs<'_>) -> Result<Self> {
        // A report that cannot name its generation could be taken as fresh
        // on any later host, so an unreadable boot identity refuses it.
        let (boot_id, cgroup_enclosure) = lillux::process_control::current_host_generation()
            .map_err(anyhow::Error::msg)
            .context("read the host generation for the node capability report")?;
        let resident_bytes = resident_region_bytes()?;
        let host = HostFacts::observe(resident_bytes, inputs.app_root);
        if host.boot_id != boot_id || host.container_enclosure != cgroup_enclosure {
            bail!("host generation changed while the node capability report was observed");
        }
        let mut view = host.capabilities();
        add_scope_capabilities(&mut view, inputs.isolation, inputs.host_runtime_present, host.gvisor());
        let mut capacity = BTreeMap::new();
        if let Some(limit) = &host.cgroup.memory_max
            && let Ok(bytes) = limit.value.parse::<u64>()
        {
            capacity.insert("enclosure_memory_max_bytes".to_owned(), bytes);
        }
        if let Some(limit) = &host.cgroup.pids_max
            && let Ok(tasks) = limit.value.parse::<u64>()
        {
            capacity.insert("enclosure_pids_max".to_owned(), tasks);
        }
        let floor = match_floor(&inputs.isolation.requires_capabilities, &view);
        Ok(Self {
            schema: CAPABILITY_REPORT_SCHEMA.to_owned(),
            node_fingerprint: inputs.node_fingerprint.to_owned(),
            daemon_digest: inputs.daemon_digest,
            generated_at: lillux::time::iso8601_now(),
            generation: ReportGeneration { boot_id, cgroup_enclosure },
            subject: "node".to_owned(),
            composition: NodeComposition {
                profile: crate::init::load_verified_substrate_identity(inputs.app_root)
                    .ok()
                    .and_then(|identity| identity.node_profile),
                bundles: inputs.bundles,
                compiled_features: inputs.compiled_features,
                policy_generation: inputs.policy_generation,
            },
            exposure: NodeExposure::unattested(inputs.listeners),
            host,
            provided: view.provided,
            missing: view.missing,
            capacity,
            floor,
            established_residency: None,
        })
    }

    pub fn digest(&self) -> Result<String> {
        let canonical = lillux::canonical_json(&serde_json::to_value(self)?)
            .context("canonicalize node capability report")?;
        Ok(format!("sha256:{}", lillux::sha256_hex(canonical.as_bytes())))
    }

    pub fn sign(self, signing_key: &lillux::crypto::SigningKey) -> Result<SignedNodeCapabilityReport> {
        use lillux::crypto::Signer as _;
        let digest = self.digest()?;
        let signature = signing_key.sign(signed_message(&digest).as_bytes());
        Ok(SignedNodeCapabilityReport {
            signer: format!("fp:{}", self.node_fingerprint),
            report: self,
            digest,
            signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        })
    }

    /// Refuse, in plain words, when the declared floor is not met: before
    /// the native reservation on the planned residency, or after it on the
    /// residency it established.
    pub fn require_floor(&self) -> Result<()> {
        if self.floor.admitted {
            return Ok(());
        }
        Err(CapabilityFloorRefused(refusal_text(self.refusal_stage(), &self.floor, &self.provided)).into())
    }

    /// The floor for a service that opens no process scopes and admits no
    /// scoped job: every host-level entry must be met, and an entry naming
    /// only per-scope capabilities, which describe only scoped execution, is
    /// not checked.
    pub fn require_floor_without_scopes(&self) -> Result<()> {
        let requires: Vec<CapabilityRequirement> = self
            .floor
            .requires
            .iter()
            .filter(|entry| !entry.names().iter().all(|name| is_scope_capability(name)))
            .cloned()
            .collect();
        let view = HostCapabilities { provided: self.provided.clone(), missing: self.missing.clone() };
        let floor = match_floor(&requires, &view);
        if floor.admitted {
            return Ok(());
        }
        Err(CapabilityFloorRefused(refusal_text(self.refusal_stage(), &floor, &self.provided)).into())
    }

    /// Record the residency proof a native reservation or host actually
    /// established, re-deciding every residency name and the floor from it.
    /// The caller checks the floor again before admitting work on it.
    pub fn establish_residency(&mut self, proof: ResidencyProof) {
        let mut view = HostCapabilities {
            provided: std::mem::take(&mut self.provided),
            missing: std::mem::take(&mut self.missing),
        };
        self.host.apply_residency(&mut view, Ok(&proof));
        self.floor = match_floor(&self.floor.requires, &view);
        self.provided = view.provided;
        self.missing = view.missing;
        self.established_residency = Some(proof);
    }

    fn refusal_stage(&self) -> &'static str {
        match self.established_residency {
            Some(_) => "after native reservation, on the residency it established",
            None => "before native reservation",
        }
    }
}

impl SignedNodeCapabilityReport {
    pub fn path(app_root: &Path) -> PathBuf {
        ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf())
            .state()
            .join(REPORT_FILE)
    }

    pub fn write(&self, app_root: &Path) -> Result<PathBuf> {
        let path = Self::path(app_root);
        let bytes = serde_json::to_vec_pretty(self)?;
        lillux::atomic_fs::atomic_write(&path, &bytes)
            .with_context(|| format!("write node capability report {}", path.display()))?;
        Ok(path)
    }

    /// Read the recorded report and verify its digest and signature against
    /// the node's public identity. Freshness is the caller's question.
    pub fn read_verified(app_root: &Path) -> Result<Option<Self>> {
        use std::io::Read as _;
        let path = Self::path(app_root);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
        };
        let mut bytes = Vec::new();
        file.take(MAX_REPORT_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_REPORT_BYTES {
            bail!("node capability report exceeds its bounded size");
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).context("parse node capability report")?;
        if let Some(reason) = stale_format(&value) {
            bail!("stale report: {reason}; the next node start writes a current one");
        }
        let signed: Self = serde_json::from_value(value).context("parse node capability report")?;
        signed.verify(app_root)?;
        Ok(Some(signed))
    }

    fn verify(&self, app_root: &Path) -> Result<()> {
        if self.report.schema != CAPABILITY_REPORT_SCHEMA {
            bail!("stale report: node capability report schema is not {CAPABILITY_REPORT_SCHEMA}");
        }
        if self.report.host.version != lillux::process_control::HOST_FACTS_VERSION {
            bail!(
                "stale report: host facts version {} is not {}",
                self.report.host.version,
                lillux::process_control::HOST_FACTS_VERSION
            );
        }
        if self.report.digest()? != self.digest {
            bail!("node capability report digest does not match its content");
        }
        let identity_path = app_root
            .join(ryeos_engine::AI_DIR)
            .join("node/identity/public-identity.json");
        let identity: ryeos_node_runtime::identity::PublicIdentityDoc = serde_json::from_slice(
            &std::fs::read(&identity_path)
                .with_context(|| format!("read {}", identity_path.display()))?,
        )?;
        let fingerprint = identity.verified_fingerprint()?;
        if fingerprint != self.report.node_fingerprint || self.signer != format!("fp:{fingerprint}") {
            bail!("node capability report was not signed by this node");
        }
        let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(
                identity
                    .signing_key
                    .strip_prefix("ed25519:")
                    .context("public node identity requires an ed25519 key")?,
            )?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid public node key length"))?;
        let key = lillux::crypto::VerifyingKey::from_bytes(&key_bytes)?;
        if !lillux::signature::verify_signature(&signed_message(&self.digest), &self.signature, &key) {
            bail!("node capability report signature is invalid");
        }
        Ok(())
    }

    /// Why this report no longer describes the running node, or `None` when
    /// it is current: the host generation (boot and container enclosure) and
    /// the node policy generation must both match.
    pub fn staleness(&self, current_policy_generation: Option<&str>) -> Result<Option<String>> {
        let (boot_id, cgroup_enclosure) =
            lillux::process_control::current_host_generation().map_err(anyhow::Error::msg)?;
        if boot_id != self.report.generation.boot_id
            || cgroup_enclosure != self.report.generation.cgroup_enclosure
        {
            return Ok(Some("the host restarted since this report".to_owned()));
        }
        match current_policy_generation {
            Some(current) if current == self.report.composition.policy_generation => Ok(None),
            Some(current) => Ok(Some(format!(
                "node policy generation is now {current}, not {}",
                self.report.composition.policy_generation
            ))),
            None => Ok(Some("the current node policy generation is unreadable".to_owned())),
        }
    }

    /// Record the residency proof the reservation actually established,
    /// re-decide every residency name and the floor from it, and re-sign.
    /// The established proof can be weaker than the startup plan when the
    /// kernel refuses a lock the limits allowed, so the caller must check
    /// the floor again before admitting work.
    pub fn with_established_residency(
        self,
        proof: ResidencyProof,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<Self> {
        let mut report = self.report;
        report.establish_residency(proof);
        report.sign(signing_key)
    }
}

/// Why a recorded report was written in a format this build no longer
/// reads, judged from its schema and host facts version alone so an older
/// layout is refused as stale rather than as unparseable.
fn stale_format(value: &serde_json::Value) -> Option<String> {
    let report = value.get("report");
    let schema = report.and_then(|report| report.get("schema")).and_then(serde_json::Value::as_str);
    if schema != Some(CAPABILITY_REPORT_SCHEMA) {
        return Some(format!(
            "node capability report schema is {}, not {CAPABILITY_REPORT_SCHEMA}",
            schema.unwrap_or("absent")
        ));
    }
    let version = report
        .and_then(|report| report.get("host"))
        .and_then(|host| host.get("version"))
        .and_then(serde_json::Value::as_u64);
    let current = u64::from(lillux::process_control::HOST_FACTS_VERSION);
    if version != Some(current) {
        return Some(format!(
            "host facts version is {}, not {current}",
            version.map_or_else(|| "absent".to_owned(), |version| version.to_string())
        ));
    }
    None
}

/// Signature domain: the schema line, then the digest, so a node signature
/// over this report cannot be replayed as a signature over another document
/// with the same digest text.
fn signed_message(digest: &str) -> String {
    format!("{CAPABILITY_REPORT_SCHEMA}\n{digest}")
}

/// Size of the native resident region whose residency the report proves.
pub fn resident_region_bytes() -> Result<u64> {
    let bytes = lillux_sqlite::original_host_owner::backing::HostStorage::quote_native_region()
        .map_err(anyhow::Error::msg)
        .context("quote the native resident region")?
        .bytes();
    u64::try_from(bytes).context("native resident region size overflow")
}

fn add_scope_capabilities(
    view: &mut HostCapabilities,
    isolation: &ryeos_engine::isolation::IsolationInspection,
    host_runtime_present: bool,
    gvisor: bool,
) {
    let qualified = &isolation.process_scope_capabilities;
    let absent = match (&isolation.process_scopes, host_runtime_present) {
        (ryeos_engine::isolation::IsolationProcessScopePolicy::Unconfigured {}, _) => Some(
            "node policy configures no process scopes, so no per-job cgroup is opened".to_owned(),
        ),
        (_, false) => Some(
            "no host service or external controller delegated a writable cgroup subtree to \
             this node"
                .to_owned(),
        ),
        _ => None,
    };
    let source = if gvisor { ProofSource::ProviderAttested } else { ProofSource::Measured };
    for capability in lillux::ProcessScopeCapability::ALL {
        let name = capability.capability_name();
        if qualified.contains(&capability) {
            view.provide(name, source.clone(), "qualified by a disposable probe scope of the delegated cgroup provider");
        } else {
            view.refuse(
                name,
                absent.clone().unwrap_or_else(|| {
                    "the delegated cgroup provider's probe scope did not qualify it".to_owned()
                }),
            );
        }
    }
    if qualified.contains(&lillux::ProcessScopeCapability::Termination) {
        view.provide(
            SCOPE_LIMITS,
            source,
            "each job scope carries its own memory.max, memory.swap.max of 0 and pids.max",
        );
    } else {
        view.refuse(
            SCOPE_LIMITS,
            absent.unwrap_or_else(|| {
                "the delegated cgroup provider has not qualified per-scope control".to_owned()
            }),
        );
    }
}

/// A name is met only by the exact named capability, proven by a local
/// measurement, or by trust where the name itself is a trusted grade. A
/// weaker grade is a different name and never stands in for a stronger one
/// unless the floor entry names it as an alternative, and then the
/// admission records that it did. Provider and peer attestations never
/// satisfy a local requirement.
pub fn match_floor(requires: &[CapabilityRequirement], view: &HostCapabilities) -> FloorAdmission {
    let mut unmet = BTreeMap::new();
    let mut met_by = BTreeMap::new();
    for entry in requires {
        let mut failures: Vec<(&str, String)> = Vec::new();
        let met = entry.names().iter().find(|name| match unmet_reason(name, view) {
            None => true,
            Some(reason) => {
                failures.push((name.as_str(), reason));
                false
            }
        });
        match (entry, met, failures.as_slice()) {
            (CapabilityRequirement::AnyOf(_), Some(name), _) => {
                met_by.insert(entry.label(), name.clone());
            }
            (CapabilityRequirement::Name(_), Some(_), _) => {}
            (CapabilityRequirement::Name(name), None, [(_, reason)]) => {
                unmet.insert(name.clone(), reason.clone());
            }
            (_, None, _) => {
                let reasons: Vec<String> =
                    failures.iter().map(|(name, reason)| format!("{name}: {reason}")).collect();
                unmet.insert(entry.label(), format!("none of its names is met; {}", reasons.join("; ")));
            }
        }
    }
    FloorAdmission { requires: requires.to_vec(), admitted: unmet.is_empty(), met_by, unmet }
}

/// Why `name` is not met here, or `None` when it is.
fn unmet_reason(name: &str, view: &HostCapabilities) -> Option<String> {
    match view.provided.get(name) {
        Some(evidence) if lillux::process_control::satisfies_local_requirement(name, evidence) => None,
        Some(evidence) => Some(format!(
            "{name} is only {}-proven here ({}); a local requirement needs a local measurement",
            evidence.source.as_str(),
            evidence.basis
        )),
        None => Some(
            view.missing
                .get(name)
                .cloned()
                .unwrap_or_else(|| format!("this node does not know how to prove {name}")),
        ),
    }
}

fn refusal_text(
    stage: &str,
    floor: &FloorAdmission,
    provided: &BTreeMap<String, CapabilityEvidence>,
) -> String {
    let mut text =
        format!("startup refused {stage}: this host cannot prove what node policy requires");
    for (name, reason) in &floor.unmet {
        text.push_str(&format!("\n  requires {name}: {reason}"));
    }
    let proven: Vec<&str> = provided.keys().map(String::as_str).collect();
    text.push_str(&format!(
        "\n  this node can prove: {}",
        if proven.is_empty() { "nothing".to_owned() } else { proven.join(", ") }
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(provided: &[&str], missing: &[(&str, &str)]) -> HostCapabilities {
        let mut view = HostCapabilities::default();
        for name in provided {
            view.provide(name, ProofSource::Measured, "fixture");
        }
        for (name, reason) in missing {
            view.refuse(name, *reason);
        }
        view
    }

    fn floor(names: &[&str]) -> Vec<CapabilityRequirement> {
        names.iter().map(|name| CapabilityRequirement::from(*name)).collect()
    }

    fn any_of(names: &[&str]) -> CapabilityRequirement {
        CapabilityRequirement::AnyOf(lillux::process_control::AnyOfCapabilities {
            any_of: names.iter().map(|name| (*name).to_owned()).collect(),
        })
    }

    fn report(provided: HostCapabilities, requires: Vec<CapabilityRequirement>) -> NodeCapabilityReport {
        NodeCapabilityReport {
            schema: CAPABILITY_REPORT_SCHEMA.to_owned(),
            node_fingerprint: "fixture".to_owned(),
            daemon_digest: "sha256:fixture".to_owned(),
            generated_at: "fixture".to_owned(),
            generation: ReportGeneration { boot_id: "fixture".to_owned(), cgroup_enclosure: None },
            subject: "node".to_owned(),
            composition: NodeComposition {
                profile: None,
                bundles: Vec::new(),
                compiled_features: Vec::new(),
                policy_generation: "fixture".to_owned(),
            },
            exposure: NodeExposure::unattested(vec!["127.0.0.1:7400".to_owned()]),
            host: HostFacts::observe(0, Path::new("/")),
            floor: match_floor(&requires, &provided),
            provided: provided.provided,
            missing: provided.missing,
            capacity: BTreeMap::new(),
            established_residency: None,
        }
    }

    #[test]
    fn floor_admits_only_exact_named_capabilities() {
        let host = view(
            &["accounting", "termination:enclosure", "time"],
            &[("termination:scope", "no writable cgroup subtree")],
        );
        let admitted = match_floor(&floor(&["accounting", "time"]), &host);
        assert!(admitted.admitted);
        let refused = match_floor(&floor(&["termination:scope", "time", "warp:drive"]), &host);
        assert!(!refused.admitted);
        assert_eq!(refused.unmet["termination:scope"], "no writable cgroup subtree");
        assert!(refused.unmet["warp:drive"].contains("does not know how to prove"));
        let mut attested = host.clone();
        attested.provide("time", ProofSource::ProviderAttested, "gvisor");
        attested.provide("termination:process_group", ProofSource::Trusted, "pidfd");
        attested.provide("isolation:namespaces", ProofSource::Trusted, "policy");
        let attested_floor = match_floor(
            &floor(&["isolation:namespaces", "termination:process_group", "time"]),
            &attested,
        );
        assert!(!attested_floor.unmet.contains_key("termination:process_group"));
        assert!(attested_floor.unmet["time"].contains("provider_attested-proven"));
        assert!(attested_floor.unmet["isolation:namespaces"].contains("trusted-proven"));
        let scoped = report(host.clone(), floor(&["limits:scope", "termination:scope", "time"]));
        assert!(scoped.require_floor().is_err());
        scoped.require_floor_without_scopes().unwrap();
        let mut host_level = scoped.clone();
        host_level.floor = match_floor(&floor(&["limits:enclosure", "termination:scope"]), &host);
        let error = host_level.require_floor_without_scopes().unwrap_err().to_string();
        assert!(error.contains("requires limits:enclosure") && !error.contains("termination:scope"), "{error}");
        let text = refusal_text("before native reservation", &refused, &host.provided);
        assert!(text.contains("requires termination:scope: no writable cgroup subtree"), "{text}");
        assert!(text.contains("can prove: accounting, termination:enclosure, time"), "{text}");
    }

    #[test]
    fn a_floor_accepts_a_weaker_grade_only_where_it_names_it() {
        use lillux::process_control::{ACCOUNTING, RESIDENCY_LOCKED, RESIDENCY_SWAP_ABSENT};
        let momentary = view(
            &[RESIDENCY_SWAP_ABSENT, "time"],
            &[(ACCOUNTING, "held only by residency:swap_absent"), (RESIDENCY_LOCKED, "mlock refused")],
        );
        // The strong name alone refuses the momentary basis.
        let strict = match_floor(&floor(&[ACCOUNTING, "time"]), &momentary);
        assert!(!strict.admitted);
        assert_eq!(strict.unmet[ACCOUNTING], "held only by residency:swap_absent");
        // A floor that names the weaker basis admits it, and says so.
        let accepting = vec![any_of(&[ACCOUNTING, RESIDENCY_SWAP_ABSENT]), "time".into()];
        let admitted = match_floor(&accepting, &momentary);
        assert!(admitted.admitted, "{admitted:?}");
        assert_eq!(admitted.met_by["any_of(accounting, residency:swap_absent)"], RESIDENCY_SWAP_ABSENT);
        // The same floor on a standing basis is met by the strong name.
        let standing = view(&[ACCOUNTING, RESIDENCY_LOCKED, "time"], &[]);
        assert_eq!(
            match_floor(&accepting, &standing).met_by["any_of(accounting, residency:swap_absent)"],
            ACCOUNTING
        );
        // Neither name proven: one unmet entry with every name's reason.
        let neither = view(&["time"], &[(ACCOUNTING, "no proof"), (RESIDENCY_SWAP_ABSENT, "swap active")]);
        let refused = match_floor(&accepting, &neither);
        let reason = &refused.unmet["any_of(accounting, residency:swap_absent)"];
        assert!(reason.contains("accounting: no proof") && reason.contains("residency:swap_absent: swap active"), "{reason}");
        // A set's alternatives obey the local-proof rule too.
        let mut attested = momentary.clone();
        attested.provide(RESIDENCY_SWAP_ABSENT, ProofSource::ProviderAttested, "gvisor");
        assert!(!match_floor(&accepting, &attested).admitted);
        // An entry with a host-level alternative is checked without scopes.
        let mixed = report(momentary.clone(), vec![any_of(&[ACCOUNTING, "limits:scope"])]);
        assert!(mixed.require_floor_without_scopes().is_err());
    }

    #[test]
    fn trusted_init_enclosure_names_meet_a_floor_that_names_them() {
        use lillux::process_control::{
            RECOVERY_ENCLOSURE, RECOVERY_ENCLOSURE_TRUSTED_INIT, TERMINATION_ENCLOSURE,
            TERMINATION_ENCLOSURE_TRUSTED_INIT,
        };
        let mut under_tini = view(&["time"], &[]);
        for name in [TERMINATION_ENCLOSURE_TRUSTED_INIT, RECOVERY_ENCLOSURE_TRUSTED_INIT] {
            under_tini.provide(name, ProofSource::Trusted, "pid 1 is tini, named by node policy");
        }
        under_tini.refuse(TERMINATION_ENCLOSURE, "the node is not pid 1");
        under_tini.refuse(RECOVERY_ENCLOSURE, "the node is not pid 1");
        let trusted = match_floor(
            &floor(&[RECOVERY_ENCLOSURE_TRUSTED_INIT, TERMINATION_ENCLOSURE_TRUSTED_INIT, "time"]),
            &under_tini,
        );
        assert!(trusted.admitted, "{trusted:?}");
        let measured = match_floor(&floor(&[RECOVERY_ENCLOSURE, TERMINATION_ENCLOSURE]), &under_tini);
        assert_eq!(measured.unmet[TERMINATION_ENCLOSURE], "the node is not pid 1");
        assert_eq!(measured.unmet[RECOVERY_ENCLOSURE], "the node is not pid 1");
        // Trust offered under a measured name is still refused.
        under_tini.provide(TERMINATION_ENCLOSURE, ProofSource::Trusted, "policy");
        let forged = match_floor(&floor(&[TERMINATION_ENCLOSURE]), &under_tini);
        assert!(forged.unmet[TERMINATION_ENCLOSURE].contains("trusted-proven"));
    }

    #[test]
    fn the_established_residency_re_decides_the_floor() {
        use lillux::process_control::{ACCOUNTING, RESIDENCY_LOCKED, RESIDENCY_SWAP_ABSENT};
        let signing_key = lillux::crypto::SigningKey::from_bytes(&[7; 32]);
        let mut planned = view(&["time"], &[]);
        let lock = ResidencyProof::Locked { basis: "cap".to_owned() };
        let host = HostFacts::observe(0, Path::new("/"));
        host.apply_residency(&mut planned, Ok(&lock));
        let weaker = ResidencyProof::SwapAbsent { basis: "no swap now".to_owned() };

        let strict = report(planned.clone(), floor(&[ACCOUNTING, "time"]));
        strict.require_floor().unwrap();
        let signed = strict.sign(&signing_key).unwrap().with_established_residency(weaker.clone(), &signing_key).unwrap();
        assert!(!signed.report.provided.contains_key(ACCOUNTING));
        assert!(!signed.report.provided.contains_key(RESIDENCY_LOCKED));
        assert!(signed.report.provided.contains_key(RESIDENCY_SWAP_ABSENT));
        assert_eq!(signed.digest, signed.report.digest().unwrap());
        let refusal = signed.report.require_floor().unwrap_err();
        let typed = CapabilityFloorRefused::find(&refusal.context("Phase 1: fixture")).cloned().unwrap();
        let error = typed.to_string();
        assert!(error.contains("after native reservation") && error.contains("requires accounting"), "{error}");

        let accepting = report(planned, vec![any_of(&[ACCOUNTING, RESIDENCY_SWAP_ABSENT]), "time".into()]);
        let signed = accepting.sign(&signing_key).unwrap().with_established_residency(weaker, &signing_key).unwrap();
        signed.report.require_floor().unwrap();
        assert_eq!(
            signed.report.floor.met_by["any_of(accounting, residency:swap_absent)"],
            RESIDENCY_SWAP_ABSENT
        );
    }

    #[test]
    fn a_service_without_scopes_re_decides_its_floor_on_the_established_residency() {
        use lillux::process_control::{ACCOUNTING, RESIDENCY_SWAP_ABSENT};
        let mut planned = view(&["time"], &[]);
        let host = HostFacts::observe(0, Path::new("/"));
        host.apply_residency(&mut planned, Ok(&ResidencyProof::Locked { basis: "cap".to_owned() }));
        let mut service = report(planned, floor(&[ACCOUNTING, "termination:scope", "time"]));
        service.require_floor_without_scopes().unwrap();
        service.establish_residency(ResidencyProof::SwapAbsent { basis: "no swap now".to_owned() });
        assert!(service.provided.contains_key(RESIDENCY_SWAP_ABSENT));
        let error = service.require_floor_without_scopes().unwrap_err().to_string();
        assert!(error.contains("after native reservation") && error.contains("requires accounting"), "{error}");
    }

    #[test]
    fn an_ephemeral_app_root_refuses_a_floor_that_requires_durable_storage() {
        use lillux::process_control::STORAGE_DURABLE_ROOT;
        let reason = "the app root /data/app is on overlay (the container's writable layer), \
                      discarded when the container is replaced";
        let ephemeral = view(&["time"], &[(STORAGE_DURABLE_ROOT, reason)]);
        let durable = report(ephemeral.clone(), floor(&[STORAGE_DURABLE_ROOT, "time"]));
        for error in [durable.require_floor(), durable.require_floor_without_scopes()] {
            let error = error.unwrap_err().to_string();
            assert!(error.contains(&format!("requires {STORAGE_DURABLE_ROOT}: {reason}")), "{error}");
        }
        // A disposable floor that does not name it admits on the same root.
        report(ephemeral, floor(&["time"])).require_floor().unwrap();
        let mut volume = view(&["time"], &[]);
        volume.provide(STORAGE_DURABLE_ROOT, ProofSource::Measured, "ext4 mounted at /data, durable by mount");
        report(volume, floor(&[STORAGE_DURABLE_ROOT, "time"])).require_floor().unwrap();
    }

    #[test]
    fn the_report_carries_the_daemon_digest_and_an_unattested_exposure() {
        let signing_key = lillux::crypto::SigningKey::from_bytes(&[7; 32]);
        let fixture = report(view(&["time"], &[]), floor(&["time"]));
        assert_eq!(fixture.exposure.listeners, ["127.0.0.1:7400"]);
        assert_eq!(fixture.exposure.ingress, None);
        assert_eq!(
            fixture.exposure.ingress_unattested.as_deref(),
            Some("no provider attests what fronts this listener")
        );
        let value = serde_json::to_value(&fixture).unwrap();
        assert_eq!(value["daemon_digest"], "sha256:fixture");
        assert!(value["composition"].get("native_services").is_none());
        assert_eq!(value["composition"]["compiled_features"], serde_json::json!([]));
        let signed = fixture.clone().sign(&signing_key).unwrap();
        let mut renamed = fixture;
        renamed.daemon_digest = "sha256:other".to_owned();
        assert_ne!(renamed.digest().unwrap(), signed.digest, "the digest covers the daemon image");
    }

    #[test]
    fn a_report_in_an_older_format_is_refused_as_stale() {
        let current = serde_json::json!({
            "report": {"schema": CAPABILITY_REPORT_SCHEMA, "host": {"version": lillux::process_control::HOST_FACTS_VERSION}}
        });
        assert_eq!(stale_format(&current), None);
        let older_schema = serde_json::json!({
            "report": {"schema": "ryeos.node.capability-report/v2", "host": {"version": 3}}
        });
        assert!(stale_format(&older_schema).unwrap().contains("capability-report/v2"));
        let older_host = serde_json::json!({
            "report": {"schema": CAPABILITY_REPORT_SCHEMA, "host": {"version": 3}}
        });
        assert!(stale_format(&older_host).unwrap().contains("host facts version is 3"));
        assert!(stale_format(&serde_json::json!({})).unwrap().contains("absent"));

        let app_root = tempfile::tempdir().unwrap();
        let path = SignedNodeCapabilityReport::path(app_root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&older_schema).unwrap()).unwrap();
        let error = SignedNodeCapabilityReport::read_verified(app_root.path()).unwrap_err().to_string();
        assert!(error.starts_with("stale report:"), "{error}");
    }
}
