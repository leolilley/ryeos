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
    CapabilityEvidence, HostCapabilities, HostFacts, ProofSource, ResidencyProof,
};
use serde::{Deserialize, Serialize};

pub const CAPABILITY_REPORT_SCHEMA: &str = "ryeos.node.capability-report/v1";
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
    /// Native services compiled into this daemon.
    pub native_services: Vec<String>,
    pub policy_generation: String,
}

/// Result of matching the declared floor against what the host proves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloorAdmission {
    pub requires: Vec<String>,
    pub admitted: bool,
    /// Each unmet requirement with the plain-language reason.
    pub unmet: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCapabilityReport {
    pub schema: String,
    pub node_fingerprint: String,
    pub generated_at: String,
    pub generation: ReportGeneration,
    /// Who the guarantees apply to. `node` means node-wide; a narrower
    /// principal or job qualifier can be named here without a schema change.
    pub subject: String,
    pub composition: NodeComposition,
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
    pub bundles: Vec<String>,
    pub native_services: Vec<String>,
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
        let host = HostFacts::observe(resident_bytes);
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
            generated_at: lillux::time::iso8601_now(),
            generation: ReportGeneration { boot_id, cgroup_enclosure },
            subject: "node".to_owned(),
            composition: NodeComposition {
                profile: crate::init::load_verified_substrate_identity(inputs.app_root)
                    .ok()
                    .and_then(|identity| identity.node_profile),
                bundles: inputs.bundles,
                native_services: inputs.native_services,
                policy_generation: inputs.policy_generation,
            },
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

    /// Refuse, in plain words, when the declared floor is not met.
    pub fn require_floor(&self) -> Result<()> {
        if self.floor.admitted {
            return Ok(());
        }
        bail!("{}", refusal_text(&self.floor, &self.provided))
    }

    /// The floor for a service that opens no process scopes and admits no
    /// scoped job: every host-level name must be met, and the per-scope
    /// names, which describe only scoped execution, are not checked.
    pub fn require_floor_without_scopes(&self) -> Result<()> {
        let requires: Vec<String> =
            self.floor.requires.iter().filter(|name| !is_scope_capability(name)).cloned().collect();
        let view = HostCapabilities { provided: self.provided.clone(), missing: self.missing.clone() };
        let floor = match_floor(&requires, &view);
        if floor.admitted {
            return Ok(());
        }
        bail!("{}", refusal_text(&floor, &self.provided))
    }
}

/// Per-job scope limits, proven only by a qualified delegated provider.
const SCOPE_LIMITS: &str = "limits:scope";

/// Whether `name` is a guarantee of per-job process scopes rather than of
/// the host or the node's enclosure.
fn is_scope_capability(name: &str) -> bool {
    name == SCOPE_LIMITS || lillux::ProcessScopeCapability::from_capability_name(name).is_some()
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
        let signed: Self = serde_json::from_slice(&bytes).context("parse node capability report")?;
        signed.verify(app_root)?;
        Ok(Some(signed))
    }

    fn verify(&self, app_root: &Path) -> Result<()> {
        if self.report.schema != CAPABILITY_REPORT_SCHEMA {
            bail!("node capability report schema is not current");
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

    /// Record the residency proof the reservation actually established and
    /// re-sign. Where it differs from the startup plan, `accounting` is
    /// re-based on the actual proof.
    pub fn with_established_residency(
        self,
        proof: ResidencyProof,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<Self> {
        let mut report = self.report;
        if report.host.residency.as_ref() != Some(&proof)
            && let Some(evidence) = report.provided.get_mut("accounting")
        {
            evidence.basis = format!("resident region {}: {}", proof.method(), proof.basis());
        }
        report.established_residency = Some(proof);
        report.sign(signing_key)
    }
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

/// A requirement is met only by the exact named capability, proven by a
/// local measurement, or by trust where the name itself is a trusted grade.
/// A weaker grade is a different name and never stands in for a stronger
/// one; provider and peer attestations never satisfy a local requirement.
pub fn match_floor(requires: &[String], view: &HostCapabilities) -> FloorAdmission {
    let mut unmet = BTreeMap::new();
    for name in requires {
        let reason = match view.provided.get(name) {
            Some(evidence)
                if lillux::process_control::satisfies_local_requirement(name, evidence) =>
            {
                continue;
            }
            Some(evidence) => format!(
                "{name} is only {}-proven here ({}); a local requirement needs a local measurement",
                evidence.source.as_str(),
                evidence.basis
            ),
            None => view.missing.get(name).cloned().unwrap_or_else(|| {
                format!("this node does not know how to prove {name}")
            }),
        };
        unmet.insert(name.clone(), reason);
    }
    FloorAdmission { requires: requires.to_vec(), admitted: unmet.is_empty(), unmet }
}

fn refusal_text(floor: &FloorAdmission, provided: &BTreeMap<String, CapabilityEvidence>) -> String {
    let mut text = String::from("startup refused before native reservation: this host cannot prove what node policy requires");
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

    #[test]
    fn floor_admits_only_exact_named_capabilities() {
        let host = view(
            &["accounting", "termination:enclosure", "time"],
            &[("termination:scope", "no writable cgroup subtree")],
        );
        let admitted = match_floor(&["accounting".to_owned(), "time".to_owned()], &host);
        assert!(admitted.admitted);
        let refused = match_floor(
            &["termination:scope".to_owned(), "time".to_owned(), "warp:drive".to_owned()],
            &host,
        );
        assert!(!refused.admitted);
        assert_eq!(refused.unmet["termination:scope"], "no writable cgroup subtree");
        assert!(refused.unmet["warp:drive"].contains("does not know how to prove"));
        let mut attested = host.clone();
        attested.provide("time", ProofSource::ProviderAttested, "gvisor");
        attested.provide("termination:process_group", ProofSource::Trusted, "pidfd");
        attested.provide("isolation:namespaces", ProofSource::Trusted, "policy");
        let floor = match_floor(
            &[
                "isolation:namespaces".to_owned(),
                "termination:process_group".to_owned(),
                "time".to_owned(),
            ],
            &attested,
        );
        assert!(!floor.unmet.contains_key("termination:process_group"));
        assert!(floor.unmet["time"].contains("provider_attested-proven"));
        assert!(floor.unmet["isolation:namespaces"].contains("trusted-proven"));
        let scoped = NodeCapabilityReport {
            schema: CAPABILITY_REPORT_SCHEMA.to_owned(),
            node_fingerprint: "fixture".to_owned(),
            generated_at: "fixture".to_owned(),
            generation: ReportGeneration { boot_id: "fixture".to_owned(), cgroup_enclosure: None },
            subject: "node".to_owned(),
            composition: NodeComposition {
                profile: None,
                bundles: Vec::new(),
                native_services: Vec::new(),
                policy_generation: "fixture".to_owned(),
            },
            host: HostFacts::observe(0),
            provided: host.provided.clone(),
            missing: host.missing.clone(),
            capacity: BTreeMap::new(),
            floor: match_floor(&["limits:scope".to_owned(), "termination:scope".to_owned(), "time".to_owned()], &host),
            established_residency: None,
        };
        assert!(scoped.require_floor().is_err());
        scoped.require_floor_without_scopes().unwrap();
        let mut host_level = scoped.clone();
        host_level.floor = match_floor(&["limits:enclosure".to_owned(), "termination:scope".to_owned()], &host);
        let error = host_level.require_floor_without_scopes().unwrap_err().to_string();
        assert!(error.contains("requires limits:enclosure") && !error.contains("termination:scope"), "{error}");
        let text = refusal_text(&refused, &host.provided);
        assert!(text.contains("requires termination:scope: no writable cgroup subtree"), "{text}");
        assert!(text.contains("can prove: accounting, termination:enclosure, time"), "{text}");
    }
}
