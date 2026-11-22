//! Read-only observation of what this node's host can prove.
//!
//! Each fact is read from the kernel as this process sees it: the cgroup
//! enclosure, swap and locked-memory limits, namespace availability, pidfd
//! and child-subreaper support, and the supervisor at pid 1. Nothing here
//! changes the current process. Namespace and subreaper probes run in a
//! disposable forked child that exits at once.
//!
//! Facts become capability names here, because the mapping from a kernel
//! observation to the guarantee it proves is mechanism. Which names a node
//! or workload requires is policy and stays in signed data elsewhere.
//! Capability names are open strings so a peer can carry names this build
//! does not know.

use std::collections::BTreeMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

pub const HOST_FACTS_VERSION: u32 = 7;

/// Provider id of every capability these host facts prove: the kernel as
/// this process observes it, with no service or controller in between.
pub const HOST_PROVIDER: &str = "host";

// Residency names match exactly, like every capability name: a proof meets
// only the name of its own basis, so a stronger basis does not meet a
// weaker name. A host that locks its region does not meet a floor naming
// `residency:swap_absent`. A floor that accepts a weaker basis therefore
// names it beside `accounting` in one `any_of` set, which every standing
// basis meets; `floor_entry_refusal` rejects any other placement.

/// The native region is resident by a lock.
pub const RESIDENCY_LOCKED: &str = "residency:locked";
/// The native region is resident because `memory.swap.max` is 0 on an
/// enclosing cgroup the node can neither write nor leave: a standing limit.
pub const RESIDENCY_SWAP_LIMIT: &str = "residency:swap_limit";
/// The native region is resident because no swap device was active when it
/// was observed: true only at that moment, since a later `swapon` undoes it.
pub const RESIDENCY_SWAP_ABSENT: &str = "residency:swap_absent";
/// The native region is resident by a standing basis: a lock or a swap
/// limit. A momentary absence of swap is a weaker grade and is named
/// `RESIDENCY_SWAP_ABSENT` only.
pub const ACCOUNTING: &str = "accounting";
/// Every name the residency proof decides.
pub const RESIDENCY_CAPABILITIES: [&str; 4] = [
    ACCOUNTING,
    RESIDENCY_LOCKED,
    RESIDENCY_SWAP_LIMIT,
    RESIDENCY_SWAP_ABSENT,
];

/// The node's app root is on storage that outlives the node's container:
/// a block or network filesystem on its own mount, or the host's own root
/// filesystem when the node runs in the host's initial pid namespace. A
/// container's writable layer or a memory-backed filesystem never meets it.
pub const STORAGE_DURABLE_ROOT: &str = "storage:durable_root";

/// State written under the app root survives the observing process and is
/// not ended by a declared host lifetime: the app root meets
/// `STORAGE_DURABLE_ROOT` and no `HostLifetimeBound` is declared. The
/// storage half is observed; the lifetime half is declared data, so a
/// proof of this name is never graded above `trusted`.
pub const PERSISTENCE_DURABLE_STATE: &str = "persistence:durable_state";

/// How the node's resident native region is kept in physical memory.
///
/// A lock holds for the mapping's life. A swap-unreachable proof holds only
/// while its basis holds, and its holder re-checks it before admitting new
/// native work. Each basis is a distinct method, so the grade it carries is
/// never inferred from text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResidencyProof {
    /// The pages are locked with mlock.
    Locked { basis: String },
    /// The pages are touched once while `memory.swap.max` is 0 on an
    /// enclosing cgroup the node can neither write nor move out of, so the
    /// kernel has nowhere to page anonymous memory out to for as long as
    /// that limit stands.
    SwapLimit { basis: String },
    /// The pages are touched once while no swap device is active. A later
    /// `swapon` ends this, so it proves residency only at the moment it is
    /// observed.
    SwapAbsent { basis: String },
    /// A method this build does not know, carried from another report with
    /// every field it had, so the report re-serializes exactly. It proves
    /// nothing here.
    Unknown(serde_json::Map<String, serde_json::Value>),
}

/// The strict form of the methods this build knows.
#[derive(Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
enum KnownResidencyProof {
    Locked { basis: String },
    SwapLimit { basis: String },
    SwapAbsent { basis: String },
}

impl Serialize for ResidencyProof {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Locked { basis } => KnownResidencyProof::Locked {
                basis: basis.clone(),
            }
            .serialize(serializer),
            Self::SwapLimit { basis } => KnownResidencyProof::SwapLimit {
                basis: basis.clone(),
            }
            .serialize(serializer),
            Self::SwapAbsent { basis } => KnownResidencyProof::SwapAbsent {
                basis: basis.clone(),
            }
            .serialize(serializer),
            Self::Unknown(original) => original.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ResidencyProof {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let original = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let method = match original.get("method") {
            Some(serde_json::Value::String(method)) => method.as_str(),
            _ => return Err(D::Error::custom("residency proof requires a string method")),
        };
        if !matches!(method, "locked" | "swap_limit" | "swap_absent") {
            return Ok(Self::Unknown(original));
        }
        match serde_json::from_value(serde_json::Value::Object(original))
            .map_err(D::Error::custom)?
        {
            KnownResidencyProof::Locked { basis } => Ok(Self::Locked { basis }),
            KnownResidencyProof::SwapLimit { basis } => Ok(Self::SwapLimit { basis }),
            KnownResidencyProof::SwapAbsent { basis } => Ok(Self::SwapAbsent { basis }),
        }
    }
}

impl ResidencyProof {
    /// Choose the proof this process can establish for `bytes` of resident
    /// memory, or explain in plain words why none is available. Locking is
    /// chosen whenever the effective capability or the remaining
    /// RLIMIT_MEMLOCK allows it, then a standing swap limit, then an
    /// observed absence of swap.
    pub fn plan(bytes: u64) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            linux::plan_residency(bytes)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = bytes;
            Err("resident memory proof is unavailable on this OS".to_owned())
        }
    }

    /// A swap-unreachable proof alone, the standing limit before the
    /// momentary absence, for a holder whose lock attempt the kernel refused
    /// although the limits looked sufficient.
    pub fn plan_without_lock(bytes: u64) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            let _ = bytes;
            linux::swap_unreachable_proof()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = bytes;
            Err("resident memory proof is unavailable on this OS".to_owned())
        }
    }

    pub fn method(&self) -> &'static str {
        match self {
            Self::Locked { basis: _ } => "locked",
            Self::SwapLimit { basis: _ } => "swap_limit",
            Self::SwapAbsent { basis: _ } => "swap_absent",
            Self::Unknown(_) => "unknown",
        }
    }

    pub fn basis(&self) -> &str {
        match self {
            Self::Locked { basis } | Self::SwapLimit { basis } | Self::SwapAbsent { basis } => {
                basis
            }
            Self::Unknown(_) => "",
        }
    }

    /// The capability name this proof's basis provides, or `None` for a
    /// method this build does not know.
    pub fn capability_name(&self) -> Option<&'static str> {
        match self {
            Self::Locked { basis: _ } => Some(RESIDENCY_LOCKED),
            Self::SwapLimit { basis: _ } => Some(RESIDENCY_SWAP_LIMIT),
            Self::SwapAbsent { basis: _ } => Some(RESIDENCY_SWAP_ABSENT),
            Self::Unknown(_) => None,
        }
    }

    /// Whether the basis stands for the region's life rather than only for
    /// the moment it was observed.
    pub fn standing(&self) -> bool {
        match self {
            Self::Locked { basis: _ } | Self::SwapLimit { basis: _ } => true,
            Self::SwapAbsent { basis: _ } | Self::Unknown(_) => false,
        }
    }

    /// Re-check that a swap-unreachable proof still holds at its own grade:
    /// immediately before the pages are touched, and again before new native
    /// work is admitted against them. A swap-limit proof needs the standing
    /// limit still in place; a swap-absent proof is met by either basis. A
    /// locked proof is held by the lock itself.
    pub fn confirm_swap_unreachable(&self) -> Result<(), String> {
        match self {
            Self::Locked { basis: _ } => Ok(()),
            Self::Unknown(_) => Err("an unknown residency method cannot be confirmed".to_owned()),
            #[cfg(target_os = "linux")]
            Self::SwapLimit { basis: _ } => match linux::swap_unreachable_proof()? {
                Self::SwapLimit { basis: _ } => Ok(()),
                current => Err(format!(
                    "the standing swap limit no longer holds; only {} remains ({})",
                    current.capability_name().unwrap_or("an unknown basis"),
                    current.basis()
                )),
            },
            #[cfg(target_os = "linux")]
            Self::SwapAbsent { basis: _ } => linux::swap_unreachable_proof().map(|_| ()),
            #[cfg(not(target_os = "linux"))]
            Self::SwapLimit { basis: _ } | Self::SwapAbsent { basis: _ } => {
                Err("resident memory proof is unavailable on this OS".to_owned())
            }
        }
    }
}

/// Who proved a capability. Carried as an open string so a report naming a
/// source this build does not know still parses; such a source is `Unknown`,
/// keeps its original text so the report re-serializes exactly, and
/// satisfies nothing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ProofSource {
    /// Observed by this node on this host.
    Measured,
    /// Claimed by a provider whose isolation layer this node cannot observe.
    ProviderAttested,
    /// Claimed by another node.
    PeerAttested,
    /// Accepted by policy without enforcement that a workload cannot escape.
    Trusted,
    Unknown(String),
}

impl ProofSource {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Measured => "measured",
            Self::ProviderAttested => "provider_attested",
            Self::PeerAttested => "peer_attested",
            Self::Trusted => "trusted",
            Self::Unknown(original) => original,
        }
    }
}

impl From<String> for ProofSource {
    fn from(value: String) -> Self {
        match value.as_str() {
            "measured" => Self::Measured,
            "provider_attested" => Self::ProviderAttested,
            "peer_attested" => Self::PeerAttested,
            "trusted" => Self::Trusted,
            _ => Self::Unknown(value),
        }
    }
}

impl From<ProofSource> for String {
    fn from(value: ProofSource) -> Self {
        match value {
            ProofSource::Unknown(original) => original,
            known => known.as_str().to_owned(),
        }
    }
}

/// One entry of a capability floor. A bare name is met only by a measured
/// proof of that name, from any provider. The object form names one
/// capability or a set of alternatives any one of which meets the entry,
/// and may widen the grades it accepts or narrow the providers it accepts.
/// Nothing the entry does not name meets it, and admission records which
/// name, grade and provider did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CapabilityRequirement {
    Name(String),
    Spec(CapabilityRequirementSpec),
}

/// The object form of a floor entry. Exactly one of `name` and `any_of` is
/// given. `accept` lists the proof grades that meet the entry, as the text
/// `ProofSource` carries; absent, only a measured proof does. `providers`
/// lists the provider ids whose proof meets the entry; absent, any does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRequirementSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any_of: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
}

/// Grades a floor entry may accept. A peer's claim is not evidence about
/// this host, so `peer_attested` is refused by name.
const ACCEPTABLE_GRADES: [&str; 3] = ["measured", "provider_attested", "trusted"];

impl CapabilityRequirement {
    /// The names that can meet this entry, in the order written.
    pub fn names(&self) -> &[String] {
        match self {
            Self::Name(name) => std::slice::from_ref(name),
            Self::Spec(spec) => match &spec.name {
                Some(name) => std::slice::from_ref(name),
                None => &spec.any_of,
            },
        }
    }

    /// Whether this entry is a set of alternatives rather than one name.
    pub fn is_set(&self) -> bool {
        matches!(self, Self::Spec(spec) if spec.name.is_none())
    }

    /// How the entry is shown in an admission: the name itself, or
    /// `any_of(a, b)` for a set.
    pub fn label(&self) -> String {
        if self.is_set() {
            format!("any_of({})", self.names().join(", "))
        } else {
            self.names().first().cloned().unwrap_or_default()
        }
    }

    /// The grades the entry accepts; empty means a measured proof only.
    pub fn accepted_grades(&self) -> &[String] {
        match self {
            Self::Name(_) => &[],
            Self::Spec(spec) => &spec.accept,
        }
    }

    /// The provider ids the entry accepts; empty means any provider.
    pub fn providers(&self) -> &[String] {
        match self {
            Self::Name(_) => &[],
            Self::Spec(spec) => &spec.providers,
        }
    }

    /// Why the entry's own shape is malformed, or `None` when it is well
    /// formed: it must name exactly one of `name` and `any_of`, every string
    /// in it must be non-empty and unique within its list, and every grade
    /// must be one this node can judge.
    pub fn spec_refusal(&self) -> Option<String> {
        let spec = match self {
            Self::Name(name) if name.is_empty() => {
                return Some("entry names an empty capability".to_owned());
            }
            Self::Name(_) => return None,
            Self::Spec(spec) => spec,
        };
        match (&spec.name, spec.any_of.is_empty()) {
            (Some(_), false) => {
                return Some(format!("entry {} gives both name and any_of", self.label()));
            }
            (None, true) => return Some("entry gives neither name nor any_of".to_owned()),
            _ => {}
        }
        for (list, what) in [
            (self.names(), "capability"),
            (spec.accept.as_slice(), "grade"),
            (spec.providers.as_slice(), "provider"),
        ] {
            if list.iter().any(String::is_empty) {
                return Some(format!("entry {} names an empty {what}", self.label()));
            }
            if let Some(repeated) = list
                .iter()
                .enumerate()
                .find_map(|(index, item)| list[..index].contains(item).then_some(item))
            {
                return Some(format!(
                    "entry {} names {what} {repeated} twice",
                    self.label()
                ));
            }
        }
        if spec.accept.iter().any(|grade| grade == "peer_attested") {
            return Some(format!(
                "entry {} accepts peer_attested, which no node admits: a peer's claim is not \
                 evidence about this host",
                self.label()
            ));
        }
        if let Some(unknown) = spec
            .accept
            .iter()
            .find(|grade| !ACCEPTABLE_GRADES.contains(&grade.as_str()))
        {
            return Some(format!(
                "entry {} accepts grade {unknown}, which this node cannot judge; the grades are {}",
                self.label(),
                ACCEPTABLE_GRADES.join(", ")
            ));
        }
        None
    }
}

/// Whether `evidence` for `name`, one of `entry`'s names, meets the entry.
/// The refusal names the exact mismatch: the grade the proof carries
/// against the grades the entry accepts, or the provider that proved it
/// against the providers the entry accepts. A grade the entry does not list
/// never meets it, however the name reads, so a floor that wants a trusted
/// or provider-attested proof says so in data.
pub fn satisfies(
    entry: &CapabilityRequirement,
    name: &str,
    evidence: &CapabilityEvidence,
) -> Result<(), String> {
    let grade = evidence.source.as_str();
    let accepted = entry.accepted_grades();
    let grade_accepted = if accepted.is_empty() {
        evidence.source == ProofSource::Measured
    } else {
        accepted.iter().any(|accept| accept == grade)
    };
    if !grade_accepted {
        return Err(format!(
            "{name} is only {grade}-proven here by {}; the floor accepts {}",
            evidence.provider,
            if accepted.is_empty() {
                "measured".to_owned()
            } else {
                accepted.join(", ")
            }
        ));
    }
    let providers = entry.providers();
    if !providers.is_empty() && !providers.contains(&evidence.provider) {
        return Err(format!(
            "{name} is proven here by {} ({grade}); the floor accepts only {}",
            evidence.provider,
            providers.join(", ")
        ));
    }
    Ok(())
}

/// Per-job scope limits, proven only by a qualified delegated provider.
pub const SCOPE_LIMITS: &str = "limits:scope";

/// Whether `name` is a guarantee of per-job process scopes rather than of
/// the host or the node's enclosure.
pub fn is_scope_capability(name: &str) -> bool {
    name == SCOPE_LIMITS || super::ProcessScopeCapability::from_capability_name(name).is_some()
}

/// Residency names that a stronger basis does not meet, so a floor may name
/// them only as an alternative to `accounting`.
const WEAKER_RESIDENCY: [&str; 2] = [RESIDENCY_SWAP_LIMIT, RESIDENCY_SWAP_ABSENT];

/// Pairs of names where the first is a weaker grade of the second: every
/// guarantee the first gives, the second gives at least as firmly.
const WEAKER_GRADES: [(&str, &str); 4] = [
    (RESIDENCY_SWAP_ABSENT, ACCOUNTING),
    (RESIDENCY_SWAP_ABSENT, RESIDENCY_LOCKED),
    (RESIDENCY_SWAP_ABSENT, RESIDENCY_SWAP_LIMIT),
    (RESIDENCY_SWAP_LIMIT, RESIDENCY_LOCKED),
];

/// Whether `name` is a weaker grade of `than`.
pub fn is_weaker_grade(name: &str, than: &str) -> bool {
    WEAKER_GRADES.contains(&(name, than))
}

/// Whether `met` is the strongest name of `entry`: no other name the entry
/// lists is a stronger grade of it. An admission met by a weaker grade is
/// worth showing to an operator.
pub fn met_by_strongest(entry: &CapabilityRequirement, met: &str) -> bool {
    !entry
        .names()
        .iter()
        .any(|other| is_weaker_grade(met, other))
}

/// Nested sandboxing of a job: user, pid and mount namespaces can be
/// unshared on this host.
pub const ISOLATION_NAMESPACES: &str = "isolation:namespaces";

/// Why one floor entry is malformed in its shape or in what it names, or
/// `None` when it is well formed and well placed. A per-scope name decides
/// how scopes are configured, so it is never conditional inside a set.
/// Durable app-root storage decides whether the node's identity and records
/// survive a restart, and namespace isolation decides whether jobs are
/// launched into a nested sandbox; no alternative replaces either, so each
/// stands alone. A weaker residency basis is not met by a stronger one, so
/// it appears only in a set beside `accounting`, which every standing basis
/// meets.
pub fn floor_entry_refusal(entry: &CapabilityRequirement) -> Option<String> {
    if let Some(refusal) = entry.spec_refusal() {
        return Some(refusal);
    }
    let names = entry.names();
    if entry.is_set()
        && let Some(scoped) = names.iter().find(|name| is_scope_capability(name))
    {
        return Some(format!(
            "entry {} names per-scope capability {scoped}, which must be required on its own",
            entry.label()
        ));
    }
    if entry.is_set()
        && let Some(alone) = names
            .iter()
            .find(|name| *name == STORAGE_DURABLE_ROOT || *name == ISOLATION_NAMESPACES)
    {
        return Some(format!(
            "entry {} names {alone}, which must be required on its own",
            entry.label()
        ));
    }
    let beside_accounting = entry.is_set() && names.iter().any(|name| name == ACCOUNTING);
    match names
        .iter()
        .find(|name| WEAKER_RESIDENCY.contains(&name.as_str()))
    {
        Some(weaker) if !beside_accounting => Some(format!(
            "entry {} names {weaker}, which a stronger residency basis does not meet; name it \
             only beside {ACCOUNTING} in one any_of set",
            entry.label()
        )),
        _ => None,
    }
}

impl From<&str> for CapabilityRequirement {
    fn from(name: &str) -> Self {
        Self::Name(name.to_owned())
    }
}

impl From<String> for CapabilityRequirement {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}

/// One proof of a capability: how strongly it holds, who proved it, and
/// on what basis. The provider is an open id naming the mechanism that
/// produced the proof, so a floor can accept or refuse proofs by it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityEvidence {
    pub source: ProofSource,
    pub provider: String,
    pub basis: String,
}

/// Descendants of one execution die with its retained PID-namespace init.
pub const TERMINATION_PID_NAMESPACE: &str = "termination:pid_namespace";
/// The same retained init lets a restarted node recover and settle them.
pub const RECOVERY_PID_NAMESPACE: &str = "recovery:pid_namespace";
/// Never provided: namespace membership is not an atomic writer freeze.
pub const QUIESCENCE_PID_NAMESPACE: &str = "quiescence:pid_namespace";
/// Provider id of the retained PID-namespace init mechanism.
pub const PID_NAMESPACE_PROVIDER: &str = "pid_namespace_init";

/// Whether a `/proc/version` text names an emulated (gVisor) kernel. The one
/// definition behind the host facts and every proof graded from them.
pub fn kernel_version_is_gvisor(version: &str) -> bool {
    version.to_ascii_lowercase().contains("gvisor")
}

/// The host's gVisor fact, read once from the kernel version text the host
/// facts themselves read.
pub fn host_kernel_is_gvisor() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|version| kernel_version_is_gvisor(&version))
        .unwrap_or(false)
}

/// The host makes sealed executable memfds: its kernel accepts `MFD_EXEC`, or
/// does not know the flag and so makes every memfd executable. The basis
/// carries `memfd_exec_flag=supported` or
/// `memfd_exec_flag=unsupported_executable_by_default`. Missing when a
/// noexec memfd policy forbids executable memfds.
pub const EXEC_SEALED_MEMFD: &str = "exec:sealed_memfd";

/// The kernel authenticates a Unix socket peer with a pidfd (`SO_PEERPIDFD`,
/// Linux 6.5), which runtime attachment and `ryeos stop` rely on.
pub const IDENTITY_PEER_PIDFD: &str = "identity:peer_pidfd";
/// `pidfd_send_signal` accepts `PIDFD_SIGNAL_PROCESS_GROUP` (Linux 6.9), which
/// exact process-group cancellation, recovery and shutdown rely on.
pub const SIGNAL_PIDFD_PROCESS_GROUP: &str = "signal:pidfd_process_group";

/// Jobs die with the node because its parent is pid 1 of a private pid
/// namespace, and node policy trusts that init to exit when its only child
/// exits. The node itself is never pid 1: process identity refuses pid 1 and
/// process group 1, so a node that is a namespace's init cannot start.
pub const TERMINATION_ENCLOSURE_TRUSTED_INIT: &str = "termination:enclosure_trusted_init";
/// A new container enclosure on the same boot proves the old one's jobs
/// dead, with the grade of `TERMINATION_ENCLOSURE_TRUSTED_INIT`.
pub const RECOVERY_ENCLOSURE_TRUSTED_INIT: &str = "recovery:enclosure_trusted_init";

static ENCLOSURE_INITS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Admit the executable names of inits that the node's signed policy trusts
/// to exit when their only child exits, replacing any earlier list in this
/// process. Until this is called no init is trusted, so no node can prove its
/// exit ends its enclosure.
pub fn admit_enclosure_inits(names: &[String]) -> Result<(), String> {
    for name in names {
        if name.is_empty() || name.len() > 255 || name.contains('/') || name.contains('\0') {
            return Err(format!(
                "trusted enclosure init {name:?} is not an executable file name"
            ));
        }
    }
    let mut admitted = ENCLOSURE_INITS
        .write()
        .map_err(|_| "trusted enclosure inits are unavailable after a panic".to_owned())?;
    *admitted = names.to_vec();
    Ok(())
}

/// The admitted list; empty, trusting no init, if it is unreadable.
fn enclosure_inits() -> Vec<String> {
    ENCLOSURE_INITS
        .read()
        .map(|admitted| admitted.clone())
        .unwrap_or_default()
}

/// Why this node's exit ends its pid namespace, from what the process sees:
/// whether its pid namespace is private, its own pid and its parent's in
/// that namespace, and pid 1's executable name (from its executable link
/// when readable, else its command name).
fn decide_enclosure_exit(
    private_pid_namespace: Option<bool>,
    node_pid: u32,
    parent_pid: u32,
    pid1: Option<&str>,
    trusted_inits: &[String],
) -> Result<CapabilityEvidence, String> {
    match private_pid_namespace {
        None => return Err("this process's pid namespace is unreadable".to_owned()),
        Some(false) => {
            return Err(
                "the node runs in the host's initial pid namespace, so its jobs do not die with it"
                    .to_owned(),
            );
        }
        Some(true) => {}
    }
    if node_pid == 1 {
        return Err(format!(
            "the node is pid 1 of its private pid namespace, which process identity refuses; \
             run it as the only child of an init that node policy trusts to exit with it ({})",
            if trusted_inits.is_empty() {
                "node policy names none".to_owned()
            } else {
                trusted_inits.join(", ")
            }
        ));
    }
    if parent_pid != 1 {
        return Err(format!(
            "the node is pid {node_pid} with parent pid {parent_pid} in its pid namespace; \
             nothing proves the namespace ends when the node exits"
        ));
    }
    match pid1 {
        Some(init) if trusted_inits.iter().any(|trusted| trusted == init) => {
            Ok(CapabilityEvidence {
                source: ProofSource::Trusted,
                provider: HOST_PROVIDER.to_owned(),
                basis: format!(
                    "the node's parent is pid 1 ({init}), which node policy trusts to exit when its \
                 only child exits"
                ),
            })
        }
        Some(init) => Err(format!(
            "the node's parent is pid 1 ({init}), which node policy does not name as an init \
             that exits with its only child"
        )),
        None => Err(
            "the node's parent is pid 1, whose executable is unreadable, so it cannot be \
             matched against trusted inits"
                .to_owned(),
        ),
    }
}

/// Read the answer of `pidfd_send_signal(pidfd, 0, PIDFD_SIGNAL_PROCESS_GROUP)`
/// against this process's own group. Success, or ESRCH (the flag was
/// understood but this process leads no group), proves the flag is known; a
/// kernel older than 6.9 answers EINVAL, and any other errno is a refusal too.
#[cfg(target_os = "linux")]
fn classify_group_signal(result: Result<(), i32>) -> ProbeOutcome {
    match result {
        Ok(()) => ProbeOutcome::Allowed,
        Err(errno) if errno == libc::ESRCH => ProbeOutcome::Allowed,
        Err(errno) => ProbeOutcome::Refused {
            errno,
            reason: std::io::Error::from_raw_os_error(errno).to_string(),
        },
    }
}

/// Outcome of one disposable probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProbeOutcome {
    Allowed,
    Refused { errno: i32, reason: String },
    Unobservable { reason: String },
}

impl ProbeOutcome {
    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    fn describe(&self) -> String {
        match self {
            Self::Allowed => "allowed".to_owned(),
            Self::Refused { errno: _, reason } => format!("refused: {reason}"),
            Self::Unobservable { reason } => format!("unobservable: {reason}"),
        }
    }
}

/// A limit file value and the cgroup it was read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CgroupLimit {
    pub value: String,
    pub cgroup: String,
    pub writable_by_node: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CgroupFacts {
    /// Filesystem type of `/sys/fs/cgroup`: `cgroup2`, another type's name,
    /// or `absent`.
    pub filesystem: String,
    /// This process's unified cgroup path, relative to its cgroup namespace.
    pub membership: Option<String>,
    /// Inode of the cgroup root this process can see. It changes when the
    /// enclosing container is recreated, even on the same boot.
    pub enclosure_inode: Option<u64>,
    pub mount_read_only: Option<bool>,
    pub nsdelegate: Option<bool>,
    /// Whether this process may create a child cgroup beside itself.
    pub membership_writable: bool,
    /// Nearest finite ceiling on this process's cgroup or an ancestor.
    pub memory_max: Option<CgroupLimit>,
    pub memory_swap_max: Option<CgroupLimit>,
    pub pids_max: Option<CgroupLimit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemlockFacts {
    pub soft: Option<u64>,
    pub hard: Option<u64>,
    pub locked_bytes: Option<u64>,
    pub cap_ipc_lock: bool,
}

/// Where the node's app root is stored, as this process sees it. Every
/// field is a measurement, or `None` when it could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootStorageFacts {
    /// The path observed.
    pub path: String,
    /// Filesystem type the mount table names for the path's mount.
    pub filesystem: Option<String>,
    /// Filesystem magic number `fstatfs` reports for the path.
    pub f_type: Option<u64>,
    /// Mount identity of the path, as numbered in `/proc/self/mountinfo`.
    pub mount_id: Option<u64>,
    pub mount_point: Option<String>,
    pub mount_source: Option<String>,
    /// Mount identity of `/` in this process's mount namespace.
    pub root_mount_id: Option<u64>,
    pub private_pid_namespace: Option<bool>,
    /// Whether this kernel runs under a hypervisor: `Some(true)` when any
    /// unprivileged sign names one, `Some(false)` only when every sign was
    /// readable and none does, `None` when that cannot be told.
    pub hypervisor: Option<bool>,
    /// What the hypervisor answer rests on.
    pub hypervisor_basis: String,
    /// `native` or `gvisor`, as in [`HostFacts::kernel_kind`].
    pub kernel_kind: String,
}

impl RootStorageFacts {
    /// Observe where `path` is stored. Never fails: an unreadable fact is
    /// recorded as `None`.
    pub fn observe(path: &std::path::Path) -> Self {
        #[cfg(target_os = "linux")]
        {
            linux::observe_root_storage(path)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {
                path: path.display().to_string(),
                filesystem: None,
                f_type: None,
                mount_id: None,
                mount_point: None,
                mount_source: None,
                root_mount_id: None,
                private_pid_namespace: None,
                hypervisor: None,
                hypervisor_basis: "virtualization is not observable on this OS".to_owned(),
                kernel_kind: "unknown".to_owned(),
            }
        }
    }
}

/// What this process can measure about who it shares a host with. Raw values
/// that identify a boot, an account and a backing filesystem: they stay in
/// the process and are only ever digested by the caller, never served.
///
/// Every field that could not be measured is `None`; nothing here is a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalityObservation {
    /// `native` or `gvisor`, as in [`HostFacts::kernel_kind`]. `unknown` on an
    /// OS where the kernel version text is unreadable.
    pub kernel_kind: String,
    /// The kernel's boot id, when it reads as a UUID.
    pub boot_id: Option<String>,
    /// The effective uid, when this process is in the initial user namespace.
    /// Inside a nested user namespace a uid names an account only relative to
    /// that namespace, so two such nodes could share a uid and not an account.
    pub account_uid: Option<u32>,
    /// `Some(false)` when the process is in a nested user namespace, `None`
    /// when that could not be read.
    pub initial_user_namespace: Option<bool>,
    /// The device number of the filesystem holding the app root. The kernel
    /// numbers each live superblock once, across every mount namespace, so
    /// two roots on the same superblock report the same number and roots on
    /// different superblocks report different ones. It is not a statement
    /// that different numbers are different disks: two overlay mounts over one
    /// disk, or two btrfs subvolumes, report different numbers.
    pub storage_device: Option<u64>,
}

impl LocalityObservation {
    /// Observe this host for a node whose app root is `app_root`. Never fails.
    pub fn observe(app_root: &std::path::Path) -> Self {
        #[cfg(target_os = "linux")]
        {
            linux::observe_locality(app_root)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = app_root;
            Self {
                kernel_kind: "unknown".to_owned(),
                boot_id: None,
                account_uid: None,
                initial_user_namespace: None,
                storage_device: None,
            }
        }
    }
}

// Firmware vendor and product names that only virtual machines report.
const VIRTUAL_MACHINE_FIRMWARE: [&str; 10] = [
    "QEMU",
    "KVM",
    "Amazon EC2",
    "Google Compute Engine",
    "Virtual Machine",
    "VMware",
    "VirtualBox",
    "innotek",
    "Xen",
    "Firecracker",
];

/// Whether this kernel runs under a hypervisor, from facts readable without
/// privilege: the CPU's `hypervisor` flag in `/proc/cpuinfo` (x86 only),
/// `/sys/hypervisor/type`, and the firmware's system vendor and product
/// name. Any sign of one decides `true`. `false` needs the x86 flag line
/// read without the flag, no hypervisor type and firmware naming no
/// virtual machine; anything less, such as an architecture whose CPU does
/// not report the flag, is undecided.
fn decide_hypervisor(
    cpuinfo: Option<&str>,
    hypervisor_type: Option<&str>,
    sys_vendor: Option<&str>,
    product_name: Option<&str>,
) -> (Option<bool>, String) {
    let flags = cpuinfo.and_then(|cpuinfo| {
        cpuinfo.lines().find_map(|line| {
            line.strip_prefix("flags")
                .and_then(|rest| rest.trim_start().strip_prefix(':'))
        })
    });
    if let Some(flags) = flags
        && flags.split_whitespace().any(|flag| flag == "hypervisor")
    {
        return (Some(true), "the CPU reports the hypervisor flag".to_owned());
    }
    if let Some(kind) = hypervisor_type
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
    {
        return (Some(true), format!("/sys/hypervisor/type is {kind}"));
    }
    let firmware = [sys_vendor, product_name]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| {
            VIRTUAL_MACHINE_FIRMWARE
                .iter()
                .any(|known| name.contains(known))
        });
    if let Some(name) = firmware {
        return (
            Some(true),
            format!("the firmware names a virtual machine ({name})"),
        );
    }
    match (flags, sys_vendor) {
        (Some(_), Some(vendor)) => (
            Some(false),
            format!(
                "the CPU does not report the hypervisor flag, no hypervisor type is exposed, and the \
                 firmware names {} {}",
                vendor.trim(),
                product_name.map(str::trim).unwrap_or("")
            )
            .trim_end()
            .to_owned(),
        ),
        (Some(_), None) => (
            None,
            "the CPU does not report the hypervisor flag but the firmware identity is unreadable".to_owned(),
        ),
        (None, _) => (
            None,
            "this CPU does not report whether it runs under a hypervisor".to_owned(),
        ),
    }
}

/// How a filesystem keeps what is written to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageClass {
    /// A union or memory filesystem whose content goes with its container
    /// or its host's memory: what it is, and when its content is lost.
    Ephemeral(&'static str, &'static str),
    /// A filesystem on a block device.
    Block,
    /// A filesystem served over the network.
    Network,
    Unclassified,
}

// Filesystem names as the mount table writes them, and the magic numbers
// from <linux/magic.h> that `fstatfs` reports for the same filesystems.
const EPHEMERAL_FILESYSTEMS: [(&str, u64, &str, &str); 4] = [
    (
        "overlay",
        0x794c_7630,
        "the container's writable layer",
        "discarded when the container is replaced",
    ),
    (
        "aufs",
        0x6175_6673,
        "a container's union layer",
        "discarded when the container is replaced",
    ),
    (
        "tmpfs",
        0x0102_1994,
        "memory-backed",
        "lost when its host or container stops",
    ),
    (
        "ramfs",
        0x8584_58f6,
        "memory-backed",
        "lost when its host or container stops",
    ),
];
const BLOCK_FILESYSTEMS: [(&str, u64); 8] = [
    ("ext2", 0xef53),
    ("ext3", 0xef53),
    ("ext4", 0xef53),
    ("xfs", 0x5846_5342),
    ("btrfs", 0x9123_683e),
    ("zfs", 0x2fc1_2fc1),
    ("f2fs", 0xf2f5_2010),
    ("bcachefs", 0xca45_1a4e),
];
const NETWORK_FILESYSTEMS: [(&str, u64); 5] = [
    ("nfs", 0x6969),
    ("nfs4", 0x6969),
    ("cifs", 0xff53_4d42),
    ("smb3", 0xfe53_4d42),
    ("ceph", 0x00c3_6400),
];

/// The class of the filesystem named `filesystem` with magic `f_type`. A
/// union or memory filesystem named by either measurement is ephemeral even
/// when the other says otherwise.
fn classify_storage(filesystem: Option<&str>, f_type: Option<u64>) -> StorageClass {
    for (name, magic, what, lost) in EPHEMERAL_FILESYSTEMS {
        if filesystem == Some(name) || f_type == Some(magic) {
            return StorageClass::Ephemeral(what, lost);
        }
    }
    let known = |table: &[(&str, u64)]| match filesystem {
        Some(filesystem) => table.iter().any(|(name, _)| *name == filesystem),
        None => f_type.is_some_and(|f_type| table.iter().any(|(_, magic)| *magic == f_type)),
    };
    if known(&BLOCK_FILESYSTEMS) {
        StorageClass::Block
    } else if known(&NETWORK_FILESYSTEMS) {
        StorageClass::Network
    } else {
        StorageClass::Unclassified
    }
}

/// Whether the app root's storage outlives the node's container. A union
/// or memory filesystem is refused: it is discarded with its container or
/// its host's memory. A block or network filesystem on a mount of its own
/// is durable by mount, which proves only that the mount is not part of the
/// container's layers. The root filesystem itself is durable only for a
/// node in the host's initial pid namespace on a machine with no hypervisor
/// detected: a container's root filesystem can be discarded when the
/// container is replaced, and a virtual machine's root disk with the
/// machine, which nothing inside it can measure. Under gVisor every
/// answer is emulated, so a durable finding is provider-attested.
pub fn decide_durable_root(facts: &RootStorageFacts) -> Result<CapabilityEvidence, String> {
    let path = &facts.path;
    let name = facts
        .filesystem
        .clone()
        .or_else(|| {
            facts
                .f_type
                .map(|f_type| format!("filesystem 0x{f_type:x}"))
        })
        .ok_or_else(|| format!("the filesystem of the app root {path} is unreadable"))?;
    let class = classify_storage(facts.filesystem.as_deref(), facts.f_type);
    if let StorageClass::Ephemeral(what, lost) = class {
        return Err(format!("the app root {path} is on {name} ({what}), {lost}"));
    }
    if class == StorageClass::Unclassified {
        return Err(format!(
            "the app root {path} is on {name}, which this node cannot classify as durable storage"
        ));
    }
    let (Some(mount_id), Some(root_mount_id)) = (facts.mount_id, facts.root_mount_id) else {
        return Err(format!(
            "the mount of the app root {path} or of / is unreadable, so whether it is the \
             container's own filesystem is undecided"
        ));
    };
    let source = facts
        .mount_source
        .as_deref()
        .unwrap_or("an unreadable source");
    let basis = if mount_id != root_mount_id {
        let kind = if class == StorageClass::Network {
            "network"
        } else {
            "block"
        };
        format!(
            "the app root {path} is on {name}, a {kind} filesystem mounted at {} from {source}, \
             distinct from /; durable by mount: the mount outlives the process; nothing here \
             proves the provider keeps it across redeploys",
            facts
                .mount_point
                .as_deref()
                .unwrap_or("an unreadable mount point")
        )
    } else {
        match (facts.private_pid_namespace, facts.hypervisor) {
            (Some(false), Some(false)) => format!(
                "the app root {path} is on the root filesystem ({name} from {source}), the node \
                 runs in the host's initial pid namespace, and no hypervisor is detected ({}), so \
                 it is the machine's own disk",
                facts.hypervisor_basis
            ),
            (Some(false), Some(true)) => {
                return Err(format!(
                    "the app root {path} is on the virtual machine's root disk ({name}; {}), which \
                     may be discarded with the machine; place it on a separately attached volume",
                    facts.hypervisor_basis
                ));
            }
            (Some(false), None) => {
                return Err(format!(
                    "the app root {path} is on the root filesystem and whether this machine is \
                     virtual is undecided ({}), so its root disk may be discarded with it; place \
                     the app root on a separately attached volume",
                    facts.hypervisor_basis
                ));
            }
            (Some(true), _) => {
                return Err(format!(
                    "the app root {path} is on the container's root filesystem ({name}), which \
                     can be discarded when the container is replaced"
                ));
            }
            (None, _) => {
                return Err(format!(
                    "the app root {path} is on the root filesystem and this process's pid \
                     namespace is unreadable, so whether that is a container's is undecided"
                ));
            }
        }
    };
    if facts.kernel_kind == "gvisor" {
        return Ok(CapabilityEvidence {
            source: ProofSource::ProviderAttested,
            provider: HOST_PROVIDER.to_owned(),
            basis: format!("{basis} (observed under gVisor emulation)"),
        });
    }
    Ok(CapabilityEvidence {
        source: ProofSource::Measured,
        provider: HOST_PROVIDER.to_owned(),
        basis,
    })
}

/// A hard lifetime a hosting provider imposes on every host of its class,
/// as signed policy declares it. Nothing inside a host can measure such a
/// bound, so it is declared data, never an observation. A declared bound
/// withholds `PERSISTENCE_DURABLE_STATE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostLifetimeBound {
    /// Open id of the provider whose host class carries the bound.
    pub provider: String,
    /// Seconds after creation at which the provider ends the host.
    pub seconds: u64,
}

impl HostLifetimeBound {
    /// Why the declaration is malformed, or `None` when it is well formed:
    /// the provider id is a non-empty printable token and the bound is
    /// positive.
    pub fn refusal(&self) -> Option<String> {
        if self.provider.is_empty()
            || self.provider.len() > 128
            || !self.provider.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Some(format!(
                "host lifetime bound provider {:?} is not a printable token",
                self.provider
            ));
        }
        if self.seconds == 0 {
            return Some("host lifetime bound must be positive".to_owned());
        }
        None
    }

    /// The bound in the largest whole unit that states it exactly.
    pub fn describe(&self) -> String {
        match self.seconds {
            seconds if seconds % 3600 == 0 => format!("{} h", seconds / 3600),
            seconds if seconds % 60 == 0 => format!("{} min", seconds / 60),
            seconds => format!("{seconds} s"),
        }
    }
}

/// Whether state under the app root outlives the observing process with no
/// hard deadline. The storage half is `decide_durable_root` exactly, from
/// the mount facts observed for the app root; the lifetime half is the
/// bound signed policy declares, if any. A refusal leads with the plain
/// reasons ("no persistent disk", "<bound> hard lifetime") and then gives
/// the facts each rests on.
///
/// The absence of a declared bound is declared data, not a measurement: a
/// host can be ended at a deadline nobody declared. A proof therefore never
/// carries `measured`: measured storage with no declared bound is
/// `trusted`, and a storage proof already weaker than that (provider
/// attested under gVisor) keeps its own grade.
pub fn decide_durable_state(
    storage: &RootStorageFacts,
    declared_lifetime: Option<&HostLifetimeBound>,
) -> Result<CapabilityEvidence, String> {
    let storage = decide_durable_root(storage);
    let mut plain = Vec::new();
    let mut detail = Vec::new();
    if let Err(reason) = &storage {
        plain.push("no persistent disk".to_owned());
        detail.push(reason.clone());
    }
    if let Some(bound) = declared_lifetime {
        plain.push(format!("{} hard lifetime", bound.describe()));
        detail.push(format!(
            "signed policy declares that provider {} ends this host {} ({} s) after it is created",
            bound.provider,
            bound.describe(),
            bound.seconds
        ));
    }
    match storage {
        Ok(evidence) if plain.is_empty() => Ok(CapabilityEvidence {
            source: match evidence.source {
                ProofSource::Measured => ProofSource::Trusted,
                weaker => weaker,
            },
            provider: evidence.provider,
            basis: format!(
                "{}; no hard lifetime is declared for this host, and that absence is declared \
                 data, not measured",
                evidence.basis
            ),
        }),
        _ => Err(format!("{} ({})", plain.join("; "), detail.join("; "))),
    }
}

/// Deepest walk `PrivateDirectoryFacts::observe` makes below its anchor, in
/// directory names. A deeper path is refused rather than walked.
pub const MAX_PRIVATE_DIRECTORY_DEPTH: usize = 16;

/// One directory on the walk from an anchor down to a private directory,
/// measured from a descriptor opened without following a symbolic link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryStepFacts {
    pub path: String,
    pub owner_uid: u32,
    /// Permission bits, including the set-id and sticky bits.
    pub mode: u32,
    /// Mount identity, as numbered in `/proc/self/mountinfo`.
    pub mount_id: u64,
}

/// A directory meant to be private to the observing account, and every
/// directory between it and an anchor above it, as this process sees them.
/// The caller passes both paths; nothing here knows what the directory
/// holds.
///
/// The walk opens the anchor, then each name below it relative to the
/// descriptor of the directory before, never following a symbolic link,
/// and takes every fact from the opened descriptor. The facts therefore
/// describe one chain of inodes, not whatever each path names when it is
/// looked up again. Components above the anchor are resolved as the kernel
/// resolves them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateDirectoryFacts {
    /// The directory judged private.
    pub path: String,
    /// Where the walk starts; `path` lies beneath it or is it.
    pub anchor: String,
    /// Effective uid of the observing process.
    pub process_uid: Option<u32>,
    /// Each directory from the anchor down to `path`, in order. It ends at
    /// `path` exactly when `walk_refusal` is `None`.
    pub chain: Vec<DirectoryStepFacts>,
    /// Why the walk stopped before measuring `path`.
    pub walk_refusal: Option<String>,
}

impl PrivateDirectoryFacts {
    /// Observe `path` and every directory from `anchor` down to it, without
    /// changing anything. Never fails: a walk that cannot finish records
    /// why in `walk_refusal`.
    pub fn observe(anchor: &std::path::Path, path: &std::path::Path) -> Self {
        #[cfg(target_os = "linux")]
        {
            linux::observe_private_directory(anchor, path)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {
                path: path.display().to_string(),
                anchor: anchor.display().to_string(),
                process_uid: None,
                chain: Vec::new(),
                walk_refusal: Some("directory observation is unavailable on this OS".to_owned()),
            }
        }
    }
}

/// Whether `facts` describe a directory private to the observing account.
/// The directory itself must be owned by the process's effective uid with
/// permission bits exactly 0700. Every directory from the anchor down to
/// it must be owned by that uid or by root, must not be writable by group
/// or other unless its sticky bit is set, and must be on the anchor's
/// mount, so a bind mount anywhere on the walk is refused. No step is a
/// symbolic link, because the walk never follows one. The result is the
/// basis of a proof, or the reason there is none, naming the first
/// directory that fails.
pub fn decide_private_directory(facts: &PrivateDirectoryFacts) -> Result<String, String> {
    let path = &facts.path;
    if let Some(refusal) = &facts.walk_refusal {
        return Err(refusal.clone());
    }
    let process = facts
        .process_uid
        .ok_or_else(|| "this process's uid is unreadable".to_owned())?;
    let (Some(anchor), Some(target)) = (facts.chain.first(), facts.chain.last()) else {
        return Err(format!("nothing on the walk to {path} was measured"));
    };
    if &target.path != path {
        return Err(format!("the walk to {path} ended at {}", target.path));
    }
    for step in &facts.chain {
        if step.mount_id != anchor.mount_id {
            return Err(format!(
                "{} is on mount {}, not the mount {} of {}",
                step.path, step.mount_id, anchor.mount_id, anchor.path
            ));
        }
    }
    let ancestors = &facts.chain[..facts.chain.len() - 1];
    for step in ancestors {
        if step.owner_uid != process && step.owner_uid != 0 {
            return Err(format!(
                "{}, above {path}, is owned by uid {}, neither this process's uid {process} nor root",
                step.path, step.owner_uid
            ));
        }
        if step.mode & 0o022 != 0 && step.mode & 0o1000 == 0 {
            return Err(format!(
                "{}, above {path}, has mode {:04o}: writable by group or other without the sticky bit",
                step.path, step.mode
            ));
        }
    }
    if target.owner_uid != process {
        return Err(format!(
            "{path} is owned by uid {}, not this process's uid {process}",
            target.owner_uid
        ));
    }
    if target.mode != 0o700 {
        return Err(format!("{path} has mode {:04o}, not 0700", target.mode));
    }
    let (account, owners) = if process == 0 {
        ("uid 0 (root)".to_owned(), "root".to_owned())
    } else {
        (format!("uid {process}"), format!("uid {process} or root"))
    };
    Ok(format!(
        "{path} is private to {account}: a directory it owns with mode 0700 on mount {}; every \
         directory from {} down to it was opened without following a symbolic link, is owned \
         by {owners}, and is not writable by group or other without the sticky bit",
        target.mount_id, anchor.path
    ))
}

/// Capability bits of `CapEff` and `CapPrm` that let a process alter bytes
/// it has no write permission on, or lift a read-only mount.
const CAP_CHOWN: u64 = 1 << 0;
const CAP_DAC_OVERRIDE: u64 = 1 << 1;
const CAP_FOWNER: u64 = 1 << 3;
const CAP_SYS_ADMIN: u64 = 1 << 21;

/// Name a policy-declared in-place import root's proof is admitted under:
/// this process cannot alter the bytes under the root. Roots are named by
/// policy data, so no per-root capability name exists; this one names the
/// claim, and a root's `accept` list says which grades meet it.
pub const STORAGE_IMMUTABLE_ROOT: &str = "storage:immutable_root";

/// The floor entry a root's declared `accept` grades make: absent grades
/// admit only a measured proof, exactly like a capability floor entry.
pub fn immutable_root_requirement(accept: &[String]) -> CapabilityRequirement {
    CapabilityRequirement::Spec(CapabilityRequirementSpec {
        name: Some(STORAGE_IMMUTABLE_ROOT.to_owned()),
        any_of: Vec::new(),
        accept: accept.to_vec(),
        providers: Vec::new(),
    })
}

/// Whether `evidence` for a root meets the grades `accept` lists. The error
/// names the grade the proof carries against the grades accepted.
pub fn admit_immutable_root_proof(
    accept: &[String],
    evidence: &CapabilityEvidence,
) -> Result<(), String> {
    let requirement = immutable_root_requirement(accept);
    if let Some(refusal) = requirement.spec_refusal() {
        return Err(refusal);
    }
    satisfies(&requirement, STORAGE_IMMUTABLE_ROOT, evidence)
}

/// Most regular files one root measurement visits before it gives up.
pub const MAX_IMMUTABLE_ROOT_FILES: u64 = 200_000;
/// Deepest directory nesting one root measurement walks.
pub const MAX_IMMUTABLE_ROOT_DEPTH: usize = 64;

/// The account the observing process acts as, as the kernel reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessCredentialFacts {
    /// Effective uid.
    pub uid: u32,
    /// Effective capability mask (`CapEff`).
    pub effective_caps: u64,
    /// Permitted capability mask (`CapPrm`): what the process can raise.
    pub permitted_caps: u64,
}

impl ProcessCredentialFacts {
    fn holds(&self, bit: u64) -> bool {
        (self.effective_caps | self.permitted_caps) & bit != 0
    }
}

/// Regular files under a root that share one owner, group and permission
/// bits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileClassFacts {
    pub owner_uid: u32,
    pub owner_gid: u32,
    /// Permission bits, including the set-id and sticky bits.
    pub mode: u32,
    /// How many files fall in the class.
    pub files: u64,
    /// One file of the class, relative to the root, for a refusal to name.
    pub example: String,
}

/// Whether this process can alter the bytes of the regular files under one
/// directory root, as it sees them. Every field is a measurement, or the
/// reason it could not be taken.
///
/// The walk opens no file, follows no symbolic link, and refuses a root
/// that holds another mount, because that mount's own state would decide
/// the bytes beneath it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableRootFacts {
    /// The path observed.
    pub path: String,
    pub mount_id: Option<u64>,
    pub mount_point: Option<String>,
    pub filesystem: Option<String>,
    /// Whether the mount carries the `ro` option.
    pub mount_read_only: Option<bool>,
    /// Whether the filesystem's superblock is read-only.
    pub superblock_read_only: Option<bool>,
    pub credentials: Option<ProcessCredentialFacts>,
    /// Regular files grouped by owner, group and mode.
    pub file_classes: Vec<FileClassFacts>,
    pub files_measured: u64,
    /// Why the walk stopped before it measured every file.
    pub walk_refusal: Option<String>,
    /// `native` or `gvisor`, as in [`HostFacts::kernel_kind`].
    pub kernel_kind: String,
}

impl ImmutableRootFacts {
    /// Observe the directory `path` and the regular files under it, without
    /// changing anything. Never fails: what cannot be measured is recorded
    /// as `None` or in `walk_refusal`.
    pub fn observe(path: &std::path::Path) -> Self {
        #[cfg(target_os = "linux")]
        {
            linux::observe_immutable_root(path)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {
                path: path.display().to_string(),
                mount_id: None,
                mount_point: None,
                filesystem: None,
                mount_read_only: None,
                superblock_read_only: None,
                credentials: None,
                file_classes: Vec::new(),
                files_measured: 0,
                walk_refusal: Some("root observation is unavailable on this OS".to_owned()),
                kernel_kind: "unknown".to_owned(),
            }
        }
    }
}

/// Whether this process cannot alter the bytes of any regular file under
/// the root `facts` describe, and on what basis.
///
/// Two routes prove it. The mount is read-only and the process holds no
/// capability to remount it (`CAP_SYS_ADMIN`). Or no file can be written by
/// this process: none is owned by its uid, because an owner can restore its
/// own write permission, and none has a group or other write bit, because a
/// group the process belongs to, or an access list the mode bits do not
/// show, could grant it; and the process holds none of `CAP_DAC_OVERRIDE`,
/// `CAP_FOWNER`, `CAP_CHOWN` and `CAP_SYS_ADMIN`, each of which lets it
/// write or take the permission regardless. The first route that holds
/// decides; when neither does the reason names both.
///
/// The decision concerns this process's credentials only. A setuid helper
/// the process could run, a writable block device node, or another mount
/// of the same filesystem elsewhere are not measured. Under gVisor every
/// answer is emulated, so a finding is provider-attested.
pub fn decide_immutable_root(facts: &ImmutableRootFacts) -> Result<CapabilityEvidence, String> {
    let path = &facts.path;
    let credentials = facts
        .credentials
        .as_ref()
        .ok_or_else(|| format!("this process's uid and capabilities are unreadable, so whether it can alter {path} is undecided"))?;
    let attested = |basis: String| CapabilityEvidence {
        source: if facts.kernel_kind == "gvisor" {
            ProofSource::ProviderAttested
        } else {
            ProofSource::Measured
        },
        provider: HOST_PROVIDER.to_owned(),
        basis: if facts.kernel_kind == "gvisor" {
            format!("{basis} (observed under gVisor emulation)")
        } else {
            basis
        },
    };
    let read_only = facts.mount_read_only == Some(true) || facts.superblock_read_only == Some(true);
    let mount = format!(
        "{} mounted at {}",
        facts
            .filesystem
            .as_deref()
            .unwrap_or("an unnamed filesystem"),
        facts
            .mount_point
            .as_deref()
            .unwrap_or("an unreadable mount point")
    );
    let mount_reason = if read_only {
        if credentials.holds(CAP_SYS_ADMIN) {
            format!(
                "{path} is on a read-only mount but this process holds CAP_SYS_ADMIN, which can remount it"
            )
        } else {
            return Ok(attested(format!(
                "{path} is on a read-only mount ({mount}) and this process holds no \
                 CAP_SYS_ADMIN to remount it"
            )));
        }
    } else if facts.mount_read_only.is_none() && facts.superblock_read_only.is_none() {
        format!("whether {path} is on a read-only mount is unreadable")
    } else {
        format!("{path} is on a writable mount ({mount})")
    };
    let permission_reason = decide_unwritable_files(facts, credentials);
    match permission_reason {
        Ok(basis) => Ok(attested(basis)),
        Err(reason) => Err(format!("{mount_reason}; {reason}")),
    }
}

fn decide_unwritable_files(
    facts: &ImmutableRootFacts,
    credentials: &ProcessCredentialFacts,
) -> Result<String, String> {
    let path = &facts.path;
    if let Some(refusal) = &facts.walk_refusal {
        return Err(format!(
            "the files under {path} were not all measured: {refusal}"
        ));
    }
    for (bit, name, effect) in [
        (CAP_DAC_OVERRIDE, "CAP_DAC_OVERRIDE", "write any file"),
        (
            CAP_FOWNER,
            "CAP_FOWNER",
            "restore write permission on any file",
        ),
        (
            CAP_CHOWN,
            "CAP_CHOWN",
            "take ownership of any file and then write it",
        ),
        (
            CAP_SYS_ADMIN,
            "CAP_SYS_ADMIN",
            "override file permission by remounting or re-owning",
        ),
    ] {
        if credentials.holds(bit) {
            return Err(format!("this process holds {name}, which can {effect}"));
        }
    }
    if facts.files_measured == 0 || facts.file_classes.is_empty() {
        return Err(format!("{path} holds no regular file to measure"));
    }
    for class in &facts.file_classes {
        if class.owner_uid == credentials.uid {
            return Err(format!(
                "{} under {path} is owned by this process's uid {}, and an owner can restore its \
                 own write permission",
                class.example, credentials.uid
            ));
        }
        if class.mode & 0o022 != 0 {
            return Err(format!(
                "{} under {path} has mode {:04o}: writable by group or other, which this process \
                 may belong to or be granted by an access list",
                class.example,
                class.mode & 0o7777
            ));
        }
    }
    Ok(format!(
        "none of the {} regular files under {path} is owned by uid {} or writable by group or \
         other, and this process holds none of CAP_DAC_OVERRIDE, CAP_FOWNER, CAP_CHOWN or \
         CAP_SYS_ADMIN",
        facts.files_measured, credentials.uid
    ))
}

/// Raw host observations. Every field is a measurement as this process sees
/// it, or the reason it could not be measured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostFacts {
    pub version: u32,
    pub kernel_release: String,
    /// `native` or `gvisor`. Under gVisor every kernel answer is emulated.
    pub kernel_kind: String,
    pub boot_id: String,
    pub pid_namespace: Option<u64>,
    pub initial_pid_namespace: bool,
    pub cgroup_namespace: Option<u64>,
    pub initial_cgroup_namespace: bool,
    /// The enclosure cgroup inode visible to this process when it runs in a
    /// private cgroup namespace whose root is its own cgroup, inside a
    /// private pid namespace. Visibility alone proves nothing about the
    /// node's exit; `enclosure_exit` says whether that exit ends it.
    pub container_enclosure: Option<u64>,
    /// This process's pid and its parent's pid, as seen in its own pid
    /// namespace.
    pub node_pid: u32,
    pub parent_pid: u32,
    /// Name of the pid namespace's pid 1, from `/proc/1/comm`.
    pub pid1: Option<String>,
    /// File name of pid 1's executable, from `/proc/1/exe`, or `None` when
    /// the link is unreadable to this process.
    pub pid1_executable: Option<String>,
    /// Why the node's exit ends its pid namespace and every job in it:
    /// measured when the node is pid 1, trusted when its parent is pid 1
    /// and node policy names that init as one that exits with its only
    /// child. Exactly one of this and `enclosure_exit_refusal` is set.
    pub enclosure_exit: Option<CapabilityEvidence>,
    pub enclosure_exit_refusal: Option<String>,
    pub uid_map: Option<String>,
    pub cgroup: CgroupFacts,
    /// Active swap devices, or `None` when `/proc/swaps` is unreadable.
    pub swap_devices: Option<Vec<String>>,
    pub memlock: MemlockFacts,
    pub unshare_user: ProbeOutcome,
    pub unshare_user_pid_mount: ProbeOutcome,
    pub pidfd: ProbeOutcome,
    /// `getsockopt(SO_PEERPIDFD)` on a connected socket pair of this process.
    pub peer_pidfd: ProbeOutcome,
    /// Signal zero to this process's own group through a pidfd with
    /// `PIDFD_SIGNAL_PROCESS_GROUP`; nothing is delivered.
    pub pidfd_signal_process_group: ProbeOutcome,
    /// Whether the host permits this node to become a child subreaper,
    /// probed in a disposable child. The node does not set it: no reaper of
    /// adopted orphans exists, so this records only what the host allows.
    pub subreaper: ProbeOutcome,
    /// One `memfd_create(MFD_EXEC)`: `Allowed` when the kernel accepts the
    /// flag, `EINVAL` when it does not know it, `EACCES` under
    /// `vm.memfd_noexec=2`.
    pub memfd_exec: ProbeOutcome,
    pub resident_bytes: u64,
    pub residency: Option<ResidencyProof>,
    pub residency_refusal: Option<String>,
    /// Where the node's app root is stored.
    pub app_root_storage: RootStorageFacts,
    /// The hard host lifetime the node's signed policy declares, if any.
    /// Declared data, not a measurement: observation leaves it `None` and
    /// the node sets it from its policy before deciding capabilities.
    pub declared_lifetime: Option<HostLifetimeBound>,
}

/// Most names one capability view may hold, provided and missing together.
/// Every name a view holds is chosen by code, never by data, so the bound
/// is kept by the producers and checked by their tests.
pub const MAX_CAPABILITY_ENTRIES: usize = 32;
/// Longest provider id a view keeps, in bytes. Capability names are code
/// constants and are held to the same bound by their tests.
pub const MAX_CAPABILITY_NAME_BYTES: usize = 128;
/// Longest basis or refusal reason a view keeps, in bytes.
pub const MAX_CAPABILITY_TEXT_BYTES: usize = 512;

/// `text` as a view keeps it: printable ASCII with no character JSON must
/// escape, so its encoded length is its length plus two quotes, cut to
/// `limit` bytes with a visible mark when it is longer. A double quote
/// becomes a single quote, a backslash a slash, and any other character
/// outside printable ASCII a question mark.
fn bounded_text(text: &str, limit: usize) -> String {
    const CUT: &str = " [cut]";
    let mut kept: String = text
        .chars()
        .map(|character| match character {
            '"' => '\'',
            '\\' => '/',
            ' '..='~' => character,
            _ => '?',
        })
        .collect();
    if kept.len() > limit {
        kept.truncate(limit.saturating_sub(CUT.len()));
        kept.push_str(CUT);
    }
    kept
}

/// Host-level capability view derived from facts. Scope capabilities come
/// from an opened process-scope provider and are added by its owner.
///
/// Every provider id, basis and reason is kept through `bounded_text`, so
/// with at most `MAX_CAPABILITY_ENTRIES` names the encoded view has a fixed
/// worst case that fits one guest channel frame with room for its envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilities {
    pub provided: BTreeMap<String, CapabilityEvidence>,
    pub missing: BTreeMap<String, String>,
}

impl HostCapabilities {
    /// Record that `provider` proved `name` at grade `source` on `basis`,
    /// replacing any earlier decision for the name.
    pub fn provide(
        &mut self,
        name: &str,
        source: ProofSource,
        provider: &str,
        basis: impl Into<String>,
    ) {
        let basis: String = basis.into();
        self.missing.remove(name);
        self.provided.insert(
            name.to_owned(),
            CapabilityEvidence {
                source,
                provider: bounded_text(provider, MAX_CAPABILITY_NAME_BYTES),
                basis: bounded_text(&basis, MAX_CAPABILITY_TEXT_BYTES),
            },
        );
    }

    pub fn refuse(&mut self, name: &str, reason: impl Into<String>) {
        let reason: String = reason.into();
        if !self.provided.contains_key(name) {
            self.missing.insert(
                name.to_owned(),
                bounded_text(&reason, MAX_CAPABILITY_TEXT_BYTES),
            );
        }
    }

    /// Forget whatever this view held for `name`, so it can be decided again
    /// from newer evidence.
    pub fn withdraw(&mut self, name: &str) {
        self.provided.remove(name);
        self.missing.remove(name);
    }

    /// Whether `entry` is met by this view: the name that met it, or why none
    /// of its names did. A name is met only by exact evidence at a grade and
    /// from a provider the entry accepts; the reason of a refused single name
    /// is its own, and a set of alternatives lists every name's reason.
    pub fn meets<'a>(&'a self, entry: &'a CapabilityRequirement) -> Result<&'a str, String> {
        let mut failures: Vec<(&str, String)> = Vec::new();
        for name in entry.names() {
            let reason = match self.provided.get(name) {
                Some(evidence) => match satisfies(entry, name, evidence) {
                    Ok(()) => return Ok(name),
                    Err(mismatch) => format!("{mismatch} ({})", evidence.basis),
                },
                None => self
                    .missing
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| format!("this node does not know how to prove {name}")),
            };
            failures.push((name, reason));
        }
        match failures.as_slice() {
            [(_, reason)] if !entry.is_set() => Err(reason.clone()),
            _ => Err(format!(
                "none of its names is met; {}",
                failures
                    .iter()
                    .map(|(name, reason)| format!("{name}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
        }
    }
}

impl HostFacts {
    /// Observe this host for a node whose resident region is
    /// `resident_bytes` and whose app root is `app_root`. Never fails: an
    /// unreadable fact is recorded as such.
    pub fn observe(resident_bytes: u64, app_root: &std::path::Path) -> Self {
        #[cfg(target_os = "linux")]
        {
            linux::observe(resident_bytes, app_root)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let unobservable = || ProbeOutcome::Unobservable {
                reason: "host observation is unavailable on this OS".to_owned(),
            };
            Self {
                version: HOST_FACTS_VERSION,
                kernel_release: String::new(),
                kernel_kind: "unknown".to_owned(),
                boot_id: String::new(),
                pid_namespace: None,
                initial_pid_namespace: false,
                cgroup_namespace: None,
                initial_cgroup_namespace: false,
                container_enclosure: None,
                node_pid: std::process::id(),
                parent_pid: 0,
                pid1: None,
                pid1_executable: None,
                enclosure_exit: None,
                enclosure_exit_refusal: Some(
                    "host observation is unavailable on this OS".to_owned(),
                ),
                uid_map: None,
                cgroup: CgroupFacts {
                    filesystem: "absent".to_owned(),
                    membership: None,
                    enclosure_inode: None,
                    mount_read_only: None,
                    nsdelegate: None,
                    membership_writable: false,
                    memory_max: None,
                    memory_swap_max: None,
                    pids_max: None,
                },
                swap_devices: None,
                memlock: MemlockFacts {
                    soft: None,
                    hard: None,
                    locked_bytes: None,
                    cap_ipc_lock: false,
                },
                unshare_user: unobservable(),
                unshare_user_pid_mount: unobservable(),
                pidfd: unobservable(),
                peer_pidfd: unobservable(),
                pidfd_signal_process_group: unobservable(),
                subreaper: unobservable(),
                memfd_exec: unobservable(),
                resident_bytes,
                residency: None,
                residency_refusal: Some(
                    "resident memory proof is unavailable on this OS".to_owned(),
                ),
                app_root_storage: RootStorageFacts::observe(app_root),
                declared_lifetime: None,
            }
        }
    }

    pub fn gvisor(&self) -> bool {
        self.kernel_kind == "gvisor"
    }

    /// Decide every residency name from `proof`, or refuse them all with the
    /// reason no proof exists, replacing whatever `view` held for them. The
    /// basis name of the proof is provided exactly; `accounting` is provided
    /// only for a standing basis and carries that basis name. It runs for the
    /// startup plan and again for the proof the reservation established,
    /// which can be weaker when the kernel refuses a lock the limits allowed.
    pub fn apply_residency(
        &self,
        view: &mut HostCapabilities,
        proof: Result<&ResidencyProof, &str>,
    ) {
        for name in RESIDENCY_CAPABILITIES {
            view.withdraw(name);
        }
        let known = proof.and_then(|proof| match proof.capability_name() {
            Some(held) => Ok((proof, held)),
            None => Err("the resident region's proof uses a method this build does not know"),
        });
        let (proof, held) = match known {
            Ok(known) => known,
            Err(reason) => {
                for name in RESIDENCY_CAPABILITIES {
                    view.refuse(name, reason);
                }
                return;
            }
        };
        let source = if self.gvisor() {
            ProofSource::ProviderAttested
        } else {
            ProofSource::Measured
        };
        let mut basis = format!("resident region {}: {}", proof.method(), proof.basis());
        if self.gvisor() {
            basis.push_str(" (observed under gVisor emulation)");
        }
        for name in [
            RESIDENCY_LOCKED,
            RESIDENCY_SWAP_LIMIT,
            RESIDENCY_SWAP_ABSENT,
        ] {
            if name == held {
                view.provide(name, source.clone(), HOST_PROVIDER, basis.clone());
            } else {
                view.refuse(
                    name,
                    format!("the resident region is held by {held}, not {name}"),
                );
            }
        }
        if proof.standing() {
            view.provide(
                ACCOUNTING,
                source,
                HOST_PROVIDER,
                format!("{held}: {basis}"),
            );
        } else {
            view.refuse(
                ACCOUNTING,
                format!(
                    "the resident region is held only by {held} ({}), which lasts only until swap \
                     is enabled; {ACCOUNTING} needs {RESIDENCY_LOCKED} or {RESIDENCY_SWAP_LIMIT}",
                    proof.basis()
                ),
            );
        }
    }

    /// Host-level capabilities these facts prove. Under gVisor a kernel
    /// answer is emulated, so what would be measured is provider-attested.
    ///
    /// The enclosure guarantees come in two grades under distinct names. The
    /// plain names need the node to be pid 1 of its pid namespace, so the
    /// kernel ends the namespace when the node exits. The `trusted_init`
    /// names also accept a node whose parent is pid 1 and whose pid 1 node
    /// policy trusts to exit with its only child; a measured proof satisfies
    /// both.
    pub fn capabilities(&self) -> HostCapabilities {
        let observed = if self.gvisor() {
            ProofSource::ProviderAttested
        } else {
            ProofSource::Measured
        };
        let emulated = |basis: String| {
            if self.gvisor() {
                format!("{basis} (observed under gVisor emulation)")
            } else {
                basis
            }
        };
        let mut view = HostCapabilities::default();
        view.provide(
            "time",
            ProofSource::Measured,
            HOST_PROVIDER,
            "the node enforces each deadline and then kills",
        );
        let unproven = "resident memory is not proven".to_owned();
        self.apply_residency(
            &mut view,
            self.residency.as_ref().ok_or(
                self.residency_refusal
                    .as_ref()
                    .unwrap_or(&unproven)
                    .as_str(),
            ),
        );
        // Trust in a mechanism's semantics holds only where the kernel itself
        // answered; under gVisor the facts it rests on are emulated, so the
        // grade falls to provider-attested.
        let trusted = if self.gvisor() {
            ProofSource::ProviderAttested
        } else {
            ProofSource::Trusted
        };
        match &self.peer_pidfd {
            ProbeOutcome::Allowed => view.provide(
                IDENTITY_PEER_PIDFD,
                observed.clone(),
                HOST_PROVIDER,
                emulated("getsockopt(SO_PEERPIDFD) returned a pidfd".to_owned()),
            ),
            other => view.refuse(
                IDENTITY_PEER_PIDFD,
                format!("SO_PEERPIDFD {}", other.describe()),
            ),
        }
        match &self.pidfd_signal_process_group {
            ProbeOutcome::Allowed => view.provide(
                SIGNAL_PIDFD_PROCESS_GROUP,
                observed.clone(),
                HOST_PROVIDER,
                emulated("pidfd_send_signal accepted PIDFD_SIGNAL_PROCESS_GROUP".to_owned()),
            ),
            other => view.refuse(
                SIGNAL_PIDFD_PROCESS_GROUP,
                format!("PIDFD_SIGNAL_PROCESS_GROUP {}", other.describe()),
            ),
        }
        // Process-group termination is exact only when the group is signaled
        // through a pidfd, so it needs both the pidfd and the group flag.
        if self.pidfd.allowed() && self.pidfd_signal_process_group.allowed() {
            view.provide(
                "termination:process_group",
                trusted.clone(),
                HOST_PROVIDER,
                emulated(
                    "process group plus pidfd; a job that calls setsid or double-forks escapes it"
                        .to_owned(),
                ),
            );
        } else {
            let reason = if !self.pidfd.allowed() {
                format!("pidfd_open {}", self.pidfd.describe())
            } else {
                format!(
                    "PIDFD_SIGNAL_PROCESS_GROUP {}",
                    self.pidfd_signal_process_group.describe()
                )
            };
            view.refuse("termination:process_group", reason);
        }
        #[cfg(target_os = "linux")]
        match crate::exec::memfd_exec_flag_from_probe(&self.memfd_exec) {
            Ok(flag) => view.provide(
                EXEC_SEALED_MEMFD,
                observed.clone(),
                HOST_PROVIDER,
                emulated(format!(
                    "memfd_exec_flag={}; executable memfds are sealed and executable",
                    flag.value()
                )),
            ),
            Err(refusal) => view.refuse(EXEC_SEALED_MEMFD, refusal.code()),
        }
        #[cfg(not(target_os = "linux"))]
        view.refuse(EXEC_SEALED_MEMFD, "executable memfds exist only on Linux");
        // The grade the enclosure names carry, or why they cannot. The only
        // proof is trust in the init whose only child the node is.
        let trusted_exit: Result<(ProofSource, String), String> = match &self.enclosure_exit {
            Some(evidence) => Ok((trusted.clone(), emulated(evidence.basis.clone()))),
            None => Err(self.enclosure_exit_refusal.clone().unwrap_or_else(|| {
                "nothing proves the node's exit ends its pid namespace".to_owned()
            })),
        };
        match &trusted_exit {
            Ok((source, basis)) => view.provide(
                TERMINATION_ENCLOSURE_TRUSTED_INIT,
                source.clone(),
                HOST_PROVIDER,
                format!("{basis}; when the pid namespace ends the kernel kills every job"),
            ),
            Err(reason) => view.refuse(TERMINATION_ENCLOSURE_TRUSTED_INIT, reason.clone()),
        }
        match &self.cgroup.memory_max {
            Some(limit) if !limit.writable_by_node => view.provide(
                "limits:enclosure",
                observed.clone(),
                HOST_PROVIDER,
                emulated(format!(
                    "memory.max {} on cgroup {}, not writable by the node",
                    limit.value, limit.cgroup
                )),
            ),
            Some(limit) => view.refuse(
                "limits:enclosure",
                format!(
                    "memory.max {} on cgroup {} is writable by the node itself",
                    limit.value, limit.cgroup
                ),
            ),
            None => view.refuse(
                "limits:enclosure",
                format!(
                    "no enclosing cgroup sets a finite memory.max (cgroup filesystem: {})",
                    self.cgroup.filesystem
                ),
            ),
        }
        match (self.container_enclosure, &trusted_exit) {
            (Some(inode), Ok((source, basis))) => view.provide(
                RECOVERY_ENCLOSURE_TRUSTED_INIT,
                source.clone(),
                HOST_PROVIDER,
                format!(
                    "boot {} and container enclosure cgroup inode {inode}; {basis}, so a \
                     different inode on this boot is a new generation whose jobs all died \
                     with the old one",
                    self.boot_id
                ),
            ),
            (_, Err(reason)) => view.refuse(
                RECOVERY_ENCLOSURE_TRUSTED_INIT,
                format!(
                    "jobs are not proven to die with the node's enclosure ({reason}), so the \
                     generation is undecided beyond the boot"
                ),
            ),
            (None, Ok(_)) => view.refuse(
                RECOVERY_ENCLOSURE_TRUSTED_INIT,
                format!(
                    "the node is not the whole of a container enclosure (cgroup namespace {}, \
                     membership {}, /sys/fs/cgroup is {}), so a restart cannot be told apart \
                     from the same generation",
                    if self.initial_cgroup_namespace {
                        "initial"
                    } else {
                        "private"
                    },
                    self.cgroup.membership.as_deref().unwrap_or("unreadable"),
                    self.cgroup.filesystem
                ),
            ),
        }
        match decide_durable_root(&self.app_root_storage) {
            Ok(evidence) => view.provide(
                STORAGE_DURABLE_ROOT,
                evidence.source,
                &evidence.provider,
                evidence.basis,
            ),
            Err(reason) => view.refuse(STORAGE_DURABLE_ROOT, reason),
        }
        match decide_durable_state(&self.app_root_storage, self.declared_lifetime.as_ref()) {
            Ok(evidence) => view.provide(
                PERSISTENCE_DURABLE_STATE,
                evidence.source,
                &evidence.provider,
                evidence.basis,
            ),
            Err(reason) => view.refuse(PERSISTENCE_DURABLE_STATE, reason),
        }
        if self.unshare_user_pid_mount.allowed() {
            view.provide(
                ISOLATION_NAMESPACES,
                observed,
                HOST_PROVIDER,
                emulated("unshare of user, pid and mount namespaces is allowed".to_owned()),
            );
        } else {
            view.refuse(
                ISOLATION_NAMESPACES,
                format!(
                    "unshare of user, pid and mount namespaces {}",
                    self.unshare_user_pid_mount.describe()
                ),
            );
        }
        view
    }
}

/// Current boot identity and visible enclosure cgroup inode, without probes.
/// Use it to decide whether a recorded observation still describes this host
/// generation; it says nothing about whether jobs die with the node.
pub fn current_host_generation() -> Result<(String, Option<u64>), String> {
    #[cfg(target_os = "linux")]
    {
        let boot_id = super::linux::read_boot_id()?;
        if boot_id.len() != 36
            || !boot_id
                .bytes()
                .all(|byte| byte == b'-' || byte.is_ascii_hexdigit())
        {
            return Err("the kernel boot identity is unreadable or malformed".to_owned());
        }
        Ok((boot_id, linux::container_enclosure()?))
    }
    #[cfg(not(target_os = "linux"))]
    Err("host generation observation is unavailable on this OS".to_owned())
}

/// Inode of the container enclosure visible to this process, or `None`
/// when it is not alone in a private cgroup and pid namespace. This is the
/// current side of a generation comparison.
#[cfg(target_os = "linux")]
pub(super) fn read_cgroup_enclosure() -> Result<Option<u64>, String> {
    linux::container_enclosure()
}

/// The enclosure inode a lifetime witness may record: the visible enclosure,
/// only when the node's exit is proven to end it (its parent is a trusted
/// init). Otherwise `None`, so the witness decides by
/// boot alone and a same-boot restart is never taken as proof of death.
#[cfg(target_os = "linux")]
pub(super) fn read_witnessed_enclosure() -> Result<Option<u64>, String> {
    let (node_pid, parent_pid, pid1) = linux::pid_facts();
    let private = linux::private_pid_namespace();
    match decide_enclosure_exit(
        private,
        node_pid,
        parent_pid,
        pid1.as_deref(),
        &enclosure_inits(),
    ) {
        Ok(_) => linux::container_enclosure(),
        Err(_) => Ok(None),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeSet;
    use std::ffi::CString;
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{
        CgroupFacts, CgroupLimit, HOST_FACTS_VERSION, HostFacts, MemlockFacts, ProbeOutcome,
        ResidencyProof,
    };

    const CGROUP_ROOT: &str = "/sys/fs/cgroup";
    const CAP_IPC_LOCK: u32 = 14;
    // The kernel's fixed inode for the initial pid namespace.
    const INITIAL_PID_NAMESPACE_INODE: u64 = 0xEFFF_FFFC;
    // The kernel's fixed inode for the initial cgroup namespace.
    const INITIAL_CGROUP_NAMESPACE_INODE: u64 = 0xEFFF_FFFB;
    const MAX_PROC_FILE_BYTES: u64 = 256 * 1024;

    fn read_small(path: &str) -> Result<String, String> {
        use std::io::Read as _;
        let file = File::open(path).map_err(|error| format!("{path} is unreadable ({error})"))?;
        let mut raw = String::new();
        file.take(MAX_PROC_FILE_BYTES)
            .read_to_string(&mut raw)
            .map_err(|error| format!("{path} is unreadable ({error})"))?;
        Ok(raw)
    }

    fn status_field(status: &str, name: &str) -> Option<String> {
        status.lines().find_map(|line| {
            line.strip_prefix(name)?
                .strip_prefix(':')
                .map(|value| value.trim().to_owned())
        })
    }

    fn cap_ipc_lock(status: &str) -> bool {
        status_field(status, "CapEff")
            .and_then(|value| u64::from_str_radix(&value, 16).ok())
            .is_some_and(|mask| mask & (1 << CAP_IPC_LOCK) != 0)
    }

    fn locked_bytes(status: &str) -> Option<u64> {
        let value = status_field(status, "VmLck")?;
        value
            .strip_suffix("kB")?
            .trim()
            .parse::<u64>()
            .ok()?
            .checked_mul(1024)
    }

    fn memlock_limit() -> Option<(u64, u64)> {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) } != 0 {
            return None;
        }
        Some((limit.rlim_cur, limit.rlim_max))
    }

    fn display_limit(value: u64) -> String {
        if value == libc::RLIM_INFINITY {
            "unlimited".to_owned()
        } else {
            format!("{value} bytes")
        }
    }

    /// CAP_IPC_LOCK lifts RLIMIT_MEMLOCK only when held in the initial user
    /// namespace; inside a nested one it does not.
    fn identity_uid_map() -> bool {
        read_small("/proc/self/uid_map").is_ok_and(|map| {
            let mut lines = map.lines();
            matches!(
                (lines.next(), lines.next()),
                (Some(line), None) if line.split_whitespace().eq(["0", "0", "4294967295"])
            )
        })
    }

    pub(super) fn plan_residency(bytes: u64) -> Result<ResidencyProof, String> {
        let status = read_small("/proc/self/status").ok();
        let mut lock_notes: Vec<String> = Vec::new();
        match status.as_deref().map(cap_ipc_lock) {
            Some(true) if identity_uid_map() => {
                return Ok(ResidencyProof::Locked {
                    basis: "CAP_IPC_LOCK is effective in the initial user namespace".to_owned(),
                });
            }
            Some(true) => lock_notes.push(
                "CAP_IPC_LOCK is held only inside a nested user namespace, where it does not lift \
                 RLIMIT_MEMLOCK"
                    .to_owned(),
            ),
            Some(false) => lock_notes.push("CAP_IPC_LOCK is absent".to_owned()),
            None => lock_notes.push("/proc/self/status is unreadable".to_owned()),
        }
        match (status.as_deref().and_then(locked_bytes), memlock_limit()) {
            (Some(locked), Some((soft, hard))) => {
                if soft == libc::RLIM_INFINITY || soft.saturating_sub(locked) >= bytes {
                    return Ok(ResidencyProof::Locked {
                        basis: format!(
                            "RLIMIT_MEMLOCK {} with {locked} bytes already locked leaves room for {bytes} bytes",
                            display_limit(soft)
                        ),
                    });
                }
                lock_notes.push(format!(
                    "RLIMIT_MEMLOCK is {} (hard {}) with {locked} bytes already locked",
                    display_limit(soft),
                    display_limit(hard)
                ));
            }
            _ => lock_notes
                .push("RLIMIT_MEMLOCK or the locked-memory count is unreadable".to_owned()),
        }
        swap_unreachable_proof()
            .map_err(|swap| {
                format!(
                    "the resident region needs {bytes} bytes locked or swap proven unreachable; {}, and {swap}",
                    lock_notes.join(", ")
                )
            })
    }

    fn swap_devices() -> Result<Vec<String>, String> {
        Ok(read_small("/proc/swaps")?
            .lines()
            .skip(1)
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect())
    }

    /// The proof that no swap is reachable for this process, or the reason
    /// it cannot be proven. The standing cgroup limit is preferred; an empty
    /// swap list is only what is true now.
    pub(super) fn swap_unreachable_proof() -> Result<ResidencyProof, String> {
        if let Some(cgroup) = zero_swap_cgroup() {
            return Ok(ResidencyProof::SwapLimit {
                basis: format!(
                    "memory.swap.max is 0 on enclosing cgroup {cgroup}, which the node can neither \
                     write nor move itself out of"
                ),
            });
        }
        let devices = swap_devices();
        if let Ok(devices) = &devices
            && devices.is_empty()
        {
            return Ok(ResidencyProof::SwapAbsent {
                basis: "no swap device is active now (/proc/swaps lists none); a point-in-time \
                        observation, re-checked before new native work is admitted"
                    .to_owned(),
            });
        }
        Err(match devices {
            Ok(devices) => format!(
                "/proc/swaps lists {} and no enclosing cgroup sets memory.swap.max to 0",
                devices.join(", ")
            ),
            Err(error) => format!("{error} and no enclosing cgroup sets memory.swap.max to 0"),
        })
    }

    fn cgroup2_root() -> Result<Option<File>, String> {
        let root = match File::open(CGROUP_ROOT) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("open {CGROUP_ROOT}: {error}")),
        };
        // SAFETY: `libc::statfs` is a plain C struct of integers and integer
        // arrays, for which all-zero bytes are a valid value, so every field
        // read is defined whether or not the call below fills it.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(root.as_raw_fd(), &mut stat) } != 0 {
            return Err(format!(
                "statfs {CGROUP_ROOT}: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok((stat.f_type == super::super::cgroup::CGROUP2_SUPER_MAGIC).then_some(root))
    }

    fn filesystem_name() -> String {
        // SAFETY: `libc::statfs` is a plain C struct of integers and integer
        // arrays, for which all-zero bytes are a valid value, so every field
        // read is defined whether or not the call below fills it.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        let Ok(path) = CString::new(CGROUP_ROOT) else {
            return "absent".to_owned();
        };
        if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
            return "absent".to_owned();
        }
        // Magic numbers from <linux/magic.h>.
        match stat.f_type as u64 {
            0x6367_7270 => "cgroup2".to_owned(),
            0x0027_e0eb => "cgroup".to_owned(),
            0x0102_1994 => "tmpfs".to_owned(),
            0x6265_6572 => "sysfs".to_owned(),
            other => format!("0x{other:x}"),
        }
    }

    fn namespace_inode(name: &str) -> Option<u64> {
        std::fs::metadata(format!("/proc/self/ns/{name}"))
            .ok()
            .map(|metadata| metadata.ino())
    }

    pub(super) fn private_pid_namespace() -> Option<bool> {
        namespace_inode("pid").map(|inode| inode != INITIAL_PID_NAMESPACE_INODE)
    }

    /// This process's pid and parent pid in its own pid namespace, and the
    /// executable name of that namespace's pid 1: the file name of
    /// `/proc/1/exe` when readable, else `/proc/1/comm`. `None` when pid 1
    /// is unreadable, or when the mounted `/proc` shows a pid 1 of another
    /// pid namespace.
    pub(super) fn pid_facts() -> (u32, u32, Option<String>) {
        let node_pid = std::process::id();
        let parent_pid = u32::try_from(unsafe { libc::getppid() }).unwrap_or(0);
        let foreign_proc = match (
            std::fs::metadata("/proc/1/ns/pid")
                .ok()
                .map(|metadata| metadata.ino()),
            namespace_inode("pid"),
        ) {
            (Some(pid1_namespace), Some(own)) => pid1_namespace != own,
            _ => false,
        };
        let pid1 = if foreign_proc {
            None
        } else {
            pid1_executable().or_else(pid1_comm)
        };
        (node_pid, parent_pid, pid1)
    }

    fn pid1_executable() -> Option<String> {
        let target = std::fs::read_link("/proc/1/exe").ok()?;
        target.file_name()?.to_str().map(str::to_owned)
    }

    fn pid1_comm() -> Option<String> {
        read_small("/proc/1/comm")
            .ok()
            .map(|comm| comm.trim().to_owned())
    }

    /// The enclosure inode only when the node is the whole of a container:
    /// a private cgroup namespace whose root is this process's own cgroup,
    /// and a private pid namespace whose init exit kills every member.
    pub(super) fn container_enclosure() -> Result<Option<u64>, String> {
        let private_cgroup =
            namespace_inode("cgroup").is_some_and(|inode| inode != INITIAL_CGROUP_NAMESPACE_INODE);
        let private_pid =
            namespace_inode("pid").is_some_and(|inode| inode != INITIAL_PID_NAMESPACE_INODE);
        if !private_cgroup || !private_pid {
            return Ok(None);
        }
        match membership() {
            Ok(relative) if relative == Path::new("/") => cgroup_enclosure(),
            Ok(_) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn cgroup_enclosure() -> Result<Option<u64>, String> {
        match cgroup2_root()? {
            Some(root) => Ok(Some(
                root.metadata()
                    .map_err(|error| format!("stat {CGROUP_ROOT}: {error}"))?
                    .ino(),
            )),
            None => Ok(None),
        }
    }

    fn membership() -> Result<PathBuf, String> {
        let file = File::open("/proc/self/cgroup")
            .map_err(|error| format!("open current process cgroup membership: {error}"))?;
        super::super::cgroup::unified_cgroup_from_file(
            &file,
            super::super::cgroup::CURRENT_PROCESS_SUBJECT,
        )
    }

    /// This process's cgroup and each ancestor up to the visible root, as
    /// (relative name, directory) pairs, nearest first.
    fn enclosing_cgroups() -> Vec<(String, PathBuf)> {
        let Ok(Some(_)) = cgroup2_root() else {
            return Vec::new();
        };
        let Ok(relative) = membership() else {
            return Vec::new();
        };
        relative
            .ancestors()
            .filter_map(|ancestor| {
                let inside = ancestor.strip_prefix("/").ok()?;
                Some((
                    ancestor.display().to_string(),
                    Path::new(CGROUP_ROOT).join(inside),
                ))
            })
            .collect()
    }

    /// Whether this process may write `path`, judged with its effective
    /// identity, which is the one the kernel checks when it writes.
    fn writable(path: &Path) -> bool {
        let Ok(path) = CString::new(path.as_os_str().as_encoded_bytes()) else {
            return false;
        };
        unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::W_OK, libc::AT_EACCESS) == 0 }
    }

    fn nearest_finite(name: &str) -> Option<CgroupLimit> {
        enclosing_cgroups()
            .into_iter()
            .find_map(|(cgroup, directory)| {
                let file = directory.join(name);
                let value = std::fs::read_to_string(&file).ok()?.trim().to_owned();
                (value != "max").then(|| CgroupLimit {
                    value,
                    cgroup,
                    writable_by_node: writable(&file),
                })
            })
    }

    /// Any enclosing cgroup whose `memory.swap.max` is 0, which this
    /// process cannot write, and which encloses every cgroup the process
    /// could move itself into. A limit the node could raise proves nothing,
    /// and neither does one the node could leave: whoever can write an
    /// ancestor's `cgroup.procs` can migrate into any cgroup beneath it, so
    /// the limit must sit at or above the highest such ancestor.
    fn zero_swap_cgroup() -> Option<String> {
        let enclosing = enclosing_cgroups();
        let highest_movable = enclosing
            .iter()
            .rposition(|(_, directory)| writable(&directory.join("cgroup.procs")));
        zero_swap_at_or_above(&enclosing, highest_movable, |directory| {
            let file = directory.join("memory.swap.max");
            let value = std::fs::read_to_string(&file).ok()?;
            Some(value.trim() == "0" && !writable(&file))
        })
    }

    /// The first cgroup, nearest first, at or above index `highest_movable`
    /// for which `proves` holds.
    fn zero_swap_at_or_above(
        enclosing: &[(String, PathBuf)],
        highest_movable: Option<usize>,
        proves: impl Fn(&Path) -> Option<bool>,
    ) -> Option<String> {
        enclosing
            .iter()
            .enumerate()
            .find_map(|(index, (cgroup, directory))| {
                if highest_movable.is_some_and(|highest| index < highest) {
                    return None;
                }
                proves(directory)?.then(|| cgroup.clone())
            })
    }

    fn cgroup_mount() -> (Option<bool>, Option<bool>) {
        let Ok(raw) = read_small("/proc/self/mountinfo") else {
            return (None, None);
        };
        for line in raw.lines() {
            let Some((left, right)) = line.split_once(" - ") else {
                continue;
            };
            let fields: Vec<&str> = left.split_whitespace().collect();
            if fields.get(4) != Some(&CGROUP_ROOT) {
                continue;
            }
            let read_only = fields
                .get(5)
                .map(|options| options.split(',').any(|option| option == "ro"));
            let nsdelegate = right
                .split_whitespace()
                .nth(2)
                .map(|options| options.split(',').any(|option| option == "nsdelegate"));
            return (read_only, nsdelegate);
        }
        (None, None)
    }

    #[derive(Clone, Copy)]
    enum ChildProbe {
        Unshare(libc::c_int),
        Subreaper,
    }

    /// How long one probe child may hold the fork window. It performs a
    /// single syscall and exits, so this is a liveness cap only.
    const PROBE_CHILD_BOUND: Duration = Duration::from_secs(5);

    /// Run one syscall in a disposable child that exits at once, so the
    /// current process's namespaces and parentage never change. The child
    /// performs only the descriptor scrub, the syscall and `_exit`.
    ///
    /// The child never execs, so close-on-exec is no descriptor boundary: it
    /// is created under the fork window and first closes every registered
    /// descriptor, as every other non-executing child does. The window is
    /// held only within a fixed bound; an overrunning child is killed.
    fn probe_in_child(probe: ChildProbe) -> ProbeOutcome {
        let unobservable = |reason: String| ProbeOutcome::Unobservable { reason };
        let Some(quiesce_deadline) = Instant::now().checked_add(PROBE_CHILD_BOUND) else {
            return unobservable("probe fork bound overflow".to_owned());
        };
        let fork_guard = match crate::exec::quiesce_fork_sensitive_descriptors(quiesce_deadline) {
            Ok(guard) => guard,
            Err(error) => return unobservable(format!("fork window for probe: {error}")),
        };
        let child_close_fds = match fork_guard.fork_child_close_fds(&BTreeSet::new()) {
            Ok(fds) => fds,
            Err(error) => return unobservable(format!("fork snapshot for probe: {error}")),
        };
        // The parent write end stays open until the fork has happened or
        // failed; the child's copy closes when it exits.
        let witnessed = match fork_guard.witness_creation("host capability probe creation") {
            Ok(witnessed) => witnessed,
            Err(error) => return unobservable(format!("creation witness for probe: {error}")),
        };
        // Declared before the write end, so that on any early return the
        // write end drops first and the custody's own settlement observes it.
        let fork_custody = witnessed.0;
        let fork_custody_witness = witnessed.1;
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return unobservable(format!(
                "fork for probe: {}",
                std::io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            for fd in &child_close_fds {
                // Never retry close: the descriptor number may be consumed.
                unsafe { libc::close(*fd) };
            }
            let result = unsafe {
                match probe {
                    ChildProbe::Unshare(flags) => libc::unshare(flags),
                    ChildProbe::Subreaper => libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0),
                }
            };
            let code = if result == 0 {
                0
            } else {
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(255)
                    .clamp(1, 255)
            };
            unsafe { libc::_exit(code) }
        }
        drop(fork_custody_witness);
        let reaped = Instant::now()
            .checked_add(PROBE_CHILD_BOUND)
            .ok_or_else(|| "probe bound overflow".to_owned())
            .and_then(|bound| {
                crate::exec::reap_fork_holder_until(pid, bound, "host capability probe")
            });
        // A reaped probe has closed its witness copy, so this reopens at once.
        // Without that proof the window strands with its fact.
        let custody = fork_custody.settle();
        let status = match (reaped, custody) {
            (Ok(Some(status)), Ok(())) => status,
            (Ok(None), Ok(())) => {
                return unobservable("probe child was reaped by another owner".to_owned());
            }
            (Err(error), Ok(())) => return unobservable(format!("wait for probe: {error}")),
            (Ok(_), Err(fact)) => return unobservable(fact),
            (Err(error), Err(fact)) => {
                return unobservable(format!("wait for probe: {error}; {fact}"));
            }
        };
        if !libc::WIFEXITED(status) {
            return ProbeOutcome::Unobservable {
                reason: "probe child did not exit normally".to_owned(),
            };
        }
        match libc::WEXITSTATUS(status) {
            0 => ProbeOutcome::Allowed,
            errno => ProbeOutcome::Refused {
                errno,
                reason: std::io::Error::from_raw_os_error(errno).to_string(),
            },
        }
    }

    fn probe_pidfd() -> ProbeOutcome {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return ProbeOutcome::Refused {
                errno: error.raw_os_error().unwrap_or(0),
                reason: error.to_string(),
            };
        }
        unsafe { libc::close(fd as libc::c_int) };
        ProbeOutcome::Allowed
    }

    /// One `SO_PEERPIDFD` read on a connected socket pair of this process.
    fn probe_peer_pidfd() -> ProbeOutcome {
        let mut fds = [0 as libc::c_int; 2];
        let paired = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if paired != 0 {
            return ProbeOutcome::Unobservable {
                reason: format!("socketpair for probe: {}", std::io::Error::last_os_error()),
            };
        }
        let mut pidfd: libc::c_int = -1;
        let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fds[0],
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&mut pidfd as *mut libc::c_int).cast(),
                &mut length,
            )
        };
        let error = std::io::Error::last_os_error();
        if result == 0 && pidfd >= 0 {
            unsafe { libc::close(pidfd) };
        }
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        if result == 0 && pidfd >= 0 {
            ProbeOutcome::Allowed
        } else if result == 0 {
            ProbeOutcome::Unobservable {
                reason: "SO_PEERPIDFD returned an invalid descriptor".to_owned(),
            }
        } else {
            ProbeOutcome::Refused {
                errno: error.raw_os_error().unwrap_or(0),
                reason: error.to_string(),
            }
        }
    }

    /// Signal zero to this process's own group through a pidfd. A kernel that
    /// does not know the flag answers EINVAL; nothing is delivered.
    fn probe_pidfd_signal_process_group() -> ProbeOutcome {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
        if fd < 0 {
            return ProbeOutcome::Unobservable {
                reason: format!("pidfd_open for probe: {}", std::io::Error::last_os_error()),
            };
        }
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd as libc::c_int,
                0,
                std::ptr::null::<libc::siginfo_t>(),
                libc::PIDFD_SIGNAL_PROCESS_GROUP,
            )
        };
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(fd as libc::c_int) };
        super::classify_group_signal(if result == 0 {
            Ok(())
        } else {
            Err(error.raw_os_error().unwrap_or(0))
        })
    }

    fn kernel_kind(version: &str) -> &'static str {
        if super::kernel_version_is_gvisor(version) {
            "gvisor"
        } else {
            "native"
        }
    }

    pub(super) fn observe(resident_bytes: u64, app_root: &Path) -> HostFacts {
        let status = read_small("/proc/self/status").unwrap_or_default();
        let version = read_small("/proc/version").unwrap_or_default();
        let kernel_kind = kernel_kind(&version);
        let pid_namespace = std::fs::metadata("/proc/self/ns/pid")
            .ok()
            .map(|metadata| metadata.ino());
        let (soft, hard) =
            memlock_limit().map_or((None, None), |(soft, hard)| (Some(soft), Some(hard)));
        let membership_path = membership().ok();
        let membership_writable = membership_path
            .as_ref()
            .and_then(|relative| {
                relative
                    .strip_prefix("/")
                    .ok()
                    .map(|inside| Path::new(CGROUP_ROOT).join(inside))
            })
            .is_some_and(|directory| writable(&directory));
        let (mount_read_only, nsdelegate) = cgroup_mount();
        let residency = ResidencyProof::plan(resident_bytes);
        let (node_pid, parent_pid, pid1) = pid_facts();
        let enclosure_exit = super::decide_enclosure_exit(
            private_pid_namespace(),
            node_pid,
            parent_pid,
            pid1.as_deref(),
            &super::enclosure_inits(),
        );
        HostFacts {
            version: HOST_FACTS_VERSION,
            kernel_release: version
                .split_whitespace()
                .nth(2)
                .unwrap_or_default()
                .to_owned(),
            kernel_kind: kernel_kind.to_owned(),
            boot_id: super::super::linux::read_boot_id().unwrap_or_default(),
            pid_namespace,
            initial_pid_namespace: pid_namespace == Some(INITIAL_PID_NAMESPACE_INODE),
            cgroup_namespace: namespace_inode("cgroup"),
            initial_cgroup_namespace: namespace_inode("cgroup")
                == Some(INITIAL_CGROUP_NAMESPACE_INODE),
            container_enclosure: container_enclosure().ok().flatten(),
            node_pid,
            parent_pid,
            pid1: pid1_comm(),
            pid1_executable: pid1_executable(),
            enclosure_exit_refusal: enclosure_exit.as_ref().err().cloned(),
            enclosure_exit: enclosure_exit.ok(),
            uid_map: read_small("/proc/self/uid_map").ok().map(|map| {
                map.lines()
                    .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                    .collect::<Vec<_>>()
                    .join("; ")
            }),
            cgroup: CgroupFacts {
                filesystem: filesystem_name(),
                membership: membership_path.map(|path| path.display().to_string()),
                enclosure_inode: cgroup_enclosure().ok().flatten(),
                mount_read_only,
                nsdelegate,
                membership_writable,
                memory_max: nearest_finite("memory.max"),
                memory_swap_max: nearest_finite("memory.swap.max"),
                pids_max: nearest_finite("pids.max"),
            },
            swap_devices: swap_devices().ok(),
            memlock: MemlockFacts {
                soft,
                hard,
                locked_bytes: locked_bytes(&status),
                cap_ipc_lock: cap_ipc_lock(&status),
            },
            unshare_user: probe_in_child(ChildProbe::Unshare(libc::CLONE_NEWUSER)),
            unshare_user_pid_mount: probe_in_child(ChildProbe::Unshare(
                libc::CLONE_NEWUSER | libc::CLONE_NEWPID | libc::CLONE_NEWNS,
            )),
            pidfd: probe_pidfd(),
            peer_pidfd: probe_peer_pidfd(),
            pidfd_signal_process_group: probe_pidfd_signal_process_group(),
            subreaper: probe_in_child(ChildProbe::Subreaper),
            memfd_exec: crate::exec::memfd_exec_flag_probe(),
            resident_bytes,
            residency_refusal: residency.as_ref().err().cloned(),
            residency: residency.ok(),
            app_root_storage: observe_root_storage(app_root),
            declared_lifetime: None,
        }
    }

    /// Open `path` for observation only, following symbolic links, so the
    /// measurement is of the directory that writes actually reach.
    fn open_path(path: &Path) -> Option<File> {
        use std::os::fd::FromRawFd as _;
        let path = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        // SAFETY: a non-negative return is a fresh descriptor owned here.
        (fd >= 0).then(|| unsafe { File::from_raw_fd(fd) })
    }

    fn mount_identity(file: &File) -> Option<u64> {
        // SAFETY: `libc::statx` is a plain C struct of integers and integer
        // arrays, for which all-zero bytes are a valid value, so every field
        // read is defined whether or not the call below fills it.
        let mut observation: libc::statx = unsafe { std::mem::zeroed() };
        let empty = c"";
        let status = unsafe {
            libc::statx(
                file.as_raw_fd(),
                empty.as_ptr(),
                libc::AT_EMPTY_PATH,
                libc::STATX_MNT_ID,
                &mut observation,
            )
        };
        (status == 0 && observation.stx_mask & libc::STATX_MNT_ID != 0)
            .then_some(observation.stx_mnt_id)
    }

    fn filesystem_magic(file: &File) -> Option<u64> {
        // SAFETY: `libc::statfs` is a plain C struct of integers and integer
        // arrays, for which all-zero bytes are a valid value, so every field
        // read is defined whether or not the call below fills it.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        (unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } == 0).then_some(stat.f_type as u64)
    }

    /// Undo the octal escapes mountinfo writes for space, tab, newline and
    /// backslash.
    fn unescape_mount_field(field: &str) -> String {
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            let octal = bytes.get(index + 1..index + 4).and_then(|digits| {
                let text = std::str::from_utf8(digits).ok()?;
                u8::from_str_radix(text, 8).ok()
            });
            match (bytes[index], octal) {
                (b'\\', Some(byte)) => {
                    out.push(byte);
                    index += 4;
                }
                (byte, _) => {
                    out.push(byte);
                    index += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The mount point, filesystem type and source of mount `mount_id` in
    /// `mountinfo`.
    pub(super) fn mount_entry(mountinfo: &str, mount_id: u64) -> Option<(String, String, String)> {
        mountinfo.lines().find_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let fields: Vec<&str> = left.split_whitespace().collect();
            if fields.first()?.parse::<u64>().ok()? != mount_id {
                return None;
            }
            let mut right = right.split_whitespace();
            let filesystem = right.next()?;
            let source = right.next()?;
            Some((
                unescape_mount_field(fields.get(4)?),
                unescape_mount_field(filesystem),
                unescape_mount_field(source),
            ))
        })
    }

    pub(super) fn observe_private_directory(
        anchor: &Path,
        path: &Path,
    ) -> super::PrivateDirectoryFacts {
        // SAFETY: geteuid has no pointer arguments or caller-owned memory.
        let process_uid = unsafe { libc::geteuid() };
        let mut chain = Vec::new();
        let walk_refusal = walk_without_links(anchor, path, &mut chain).err();
        super::PrivateDirectoryFacts {
            path: path.display().to_string(),
            anchor: anchor.display().to_string(),
            process_uid: Some(process_uid),
            chain,
            walk_refusal,
        }
    }

    /// Open the anchor and then each name from it down to `path`, each
    /// relative to the descriptor before it and never following a symbolic
    /// link, recording each directory's facts from its own descriptor.
    fn walk_without_links(
        anchor: &Path,
        path: &Path,
        chain: &mut Vec<super::DirectoryStepFacts>,
    ) -> Result<(), String> {
        if !anchor.is_absolute() {
            return Err(format!("{} is not an absolute path", anchor.display()));
        }
        let below = path.strip_prefix(anchor).map_err(|_| {
            format!(
                "{} does not lie beneath {}",
                path.display(),
                anchor.display()
            )
        })?;
        let mut names = Vec::new();
        for component in below.components() {
            match component {
                std::path::Component::Normal(name) => names.push(name),
                _ => {
                    return Err(format!(
                        "{} is not reached from {} by plain directory names",
                        path.display(),
                        anchor.display()
                    ));
                }
            }
        }
        if names.len() > super::MAX_PRIVATE_DIRECTORY_DEPTH {
            return Err(format!(
                "{} lies {} directories below {}, deeper than the {} this walk measures",
                path.display(),
                names.len(),
                anchor.display(),
                super::MAX_PRIVATE_DIRECTORY_DEPTH
            ));
        }
        let mut at = anchor.to_path_buf();
        let mut current = open_directory_without_link(None, anchor.as_os_str(), &at)?;
        chain.push(directory_step(&current, &at)?);
        for name in names {
            at.push(name);
            current = open_directory_without_link(Some(&current), name, &at)?;
            chain.push(directory_step(&current, &at)?);
        }
        Ok(())
    }

    /// Open `name`, relative to `parent` or as given when there is none, as a
    /// directory without following a symbolic link in its last component.
    /// `shown` is the full path named in a refusal.
    fn open_directory_without_link(
        parent: Option<&File>,
        name: &std::ffi::OsStr,
        shown: &Path,
    ) -> Result<File, String> {
        use std::os::fd::FromRawFd as _;
        let shown = shown.display();
        let name = CString::new(name.as_encoded_bytes())
            .map_err(|_| format!("{shown} contains a NUL byte"))?;
        let directory = parent.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd);
        // SAFETY: `name` is NUL-terminated and outlives the call; `directory`
        // is AT_FDCWD or a descriptor `parent` keeps open for the call.
        let fd = unsafe {
            libc::openat(
                directory,
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            // SAFETY: a non-negative return is a fresh descriptor owned here.
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOENT) => Err(format!("{shown} is absent")),
            Some(libc::ENOTDIR | libc::ELOOP) => {
                // The open already refused; this look names what stands
                // there for the refusal text and decides nothing.
                // SAFETY: `libc::stat` is a plain C struct of integers and integer
                // arrays, for which all-zero bytes are a valid value, so every field
                // read is defined whether or not the call below fills it.
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                // SAFETY: `name` is NUL-terminated, `stat` is writable and
                // sized for the call, and `directory` is open for it.
                let looked = unsafe {
                    libc::fstatat(
                        directory,
                        name.as_ptr(),
                        &mut stat,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if looked == 0 && stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
                    Err(format!("{shown} is a symbolic link"))
                } else {
                    Err(format!("{shown} is not a directory"))
                }
            }
            _ => Err(format!("{shown} is unreadable ({error})")),
        }
    }

    /// The type, owner, mode and mount of the directory `file` holds, read
    /// from the descriptor itself.
    fn directory_step(file: &File, shown: &Path) -> Result<super::DirectoryStepFacts, String> {
        // SAFETY: `libc::statx` is a plain C struct of integers and integer
        // arrays, for which all-zero bytes are a valid value, so every field
        // read is defined whether or not the call below fills it.
        let mut observation: libc::statx = unsafe { std::mem::zeroed() };
        let wanted = libc::STATX_TYPE | libc::STATX_MODE | libc::STATX_UID | libc::STATX_MNT_ID;
        // SAFETY: the empty path with AT_EMPTY_PATH names the open
        // descriptor, and `observation` is writable and sized for the call.
        let status = unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                wanted,
                &mut observation,
            )
        };
        if status != 0 {
            return Err(format!(
                "{} is unreadable ({})",
                shown.display(),
                std::io::Error::last_os_error()
            ));
        }
        if observation.stx_mask & wanted != wanted {
            return Err(format!(
                "the owner, mode or mount of {} is unreadable",
                shown.display()
            ));
        }
        let mode = u32::from(observation.stx_mode);
        if mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(format!("{} is not a directory", shown.display()));
        }
        Ok(super::DirectoryStepFacts {
            path: shown.display().to_string(),
            owner_uid: observation.stx_uid,
            mode: mode & 0o7777,
            mount_id: observation.stx_mnt_id,
        })
    }

    pub(super) fn observe_locality(app_root: &Path) -> super::LocalityObservation {
        use std::os::unix::fs::MetadataExt as _;
        let initial_user_namespace = read_small("/proc/self/uid_map")
            .ok()
            .map(|_| identity_uid_map());
        super::LocalityObservation {
            kernel_kind: kernel_kind(&read_small("/proc/version").unwrap_or_default()).to_owned(),
            boot_id: super::super::linux::read_boot_id().ok().filter(|id| {
                id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
            }),
            account_uid: (initial_user_namespace == Some(true)).then(|| {
                // SAFETY: geteuid has no pointer arguments or caller-owned memory.
                unsafe { libc::geteuid() }
            }),
            initial_user_namespace,
            storage_device: std::fs::metadata(app_root)
                .ok()
                .map(|metadata| metadata.dev())
                .filter(|device| *device != 0),
        }
    }

    pub(super) fn observe_root_storage(path: &Path) -> super::RootStorageFacts {
        let opened = open_path(path);
        let mount_id = opened.as_ref().and_then(mount_identity);
        let entry = match (mount_id, read_small("/proc/self/mountinfo")) {
            (Some(mount_id), Ok(mountinfo)) => mount_entry(&mountinfo, mount_id),
            _ => None,
        };
        let (mount_point, filesystem, mount_source) = match entry {
            Some((point, filesystem, source)) => (Some(point), Some(filesystem), Some(source)),
            None => (None, None, None),
        };
        let firmware = |name: &str| read_small(&format!("/sys/class/dmi/id/{name}")).ok();
        let (hypervisor, hypervisor_basis) = super::decide_hypervisor(
            read_small("/proc/cpuinfo").ok().as_deref(),
            read_small("/sys/hypervisor/type").ok().as_deref(),
            firmware("sys_vendor").as_deref(),
            firmware("product_name").as_deref(),
        );
        super::RootStorageFacts {
            path: path.display().to_string(),
            filesystem,
            f_type: opened.as_ref().and_then(filesystem_magic),
            mount_id,
            mount_point,
            mount_source,
            root_mount_id: open_path(Path::new("/")).as_ref().and_then(mount_identity),
            private_pid_namespace: private_pid_namespace(),
            hypervisor,
            hypervisor_basis,
            kernel_kind: kernel_kind(&read_small("/proc/version").unwrap_or_default()).to_owned(),
        }
    }

    /// The mount options and superblock options of mount `mount_id` in
    /// `mountinfo`: whether each names `ro`.
    pub(super) fn mount_read_only(mountinfo: &str, mount_id: u64) -> Option<(bool, bool)> {
        mountinfo.lines().find_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let fields: Vec<&str> = left.split_whitespace().collect();
            if fields.first()?.parse::<u64>().ok()? != mount_id {
                return None;
            }
            let has_ro = |options: &str| options.split(',').any(|option| option == "ro");
            let super_options = right.split_whitespace().nth(2)?;
            Some((has_ro(fields.get(5)?), has_ro(super_options)))
        })
    }

    fn status_mask(status: &str, name: &str) -> Option<u64> {
        u64::from_str_radix(&status_field(status, name)?, 16).ok()
    }

    fn process_credentials() -> Option<super::ProcessCredentialFacts> {
        let status = read_small("/proc/self/status").ok()?;
        Some(super::ProcessCredentialFacts {
            // SAFETY: geteuid has no pointer arguments or caller-owned memory.
            uid: unsafe { libc::geteuid() },
            effective_caps: status_mask(&status, "CapEff")?,
            permitted_caps: status_mask(&status, "CapPrm")?,
        })
    }

    /// The mount of the directory or file at `path`, read without following
    /// a symbolic link in its last component.
    fn mount_of_path(path: &Path) -> Option<u64> {
        let name = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        // SAFETY: `libc::statx` is a plain C struct of integers and integer
        // arrays, for which all-zero bytes are a valid value, so every field
        // read is defined whether or not the call below fills it.
        let mut observation: libc::statx = unsafe { std::mem::zeroed() };
        // SAFETY: `name` is NUL-terminated and outlives the call;
        // `observation` is writable and sized for it.
        let status = unsafe {
            libc::statx(
                libc::AT_FDCWD,
                name.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_MNT_ID,
                &mut observation,
            )
        };
        (status == 0 && observation.stx_mask & libc::STATX_MNT_ID != 0)
            .then_some(observation.stx_mnt_id)
    }

    pub(super) fn observe_immutable_root(path: &Path) -> super::ImmutableRootFacts {
        let opened = open_path(path);
        let mount_id = opened.as_ref().and_then(mount_identity);
        let mountinfo = read_small("/proc/self/mountinfo").ok();
        let entry = match (mount_id, mountinfo.as_deref()) {
            (Some(mount_id), Some(mountinfo)) => mount_entry(mountinfo, mount_id),
            _ => None,
        };
        let read_only = match (mount_id, mountinfo.as_deref()) {
            (Some(mount_id), Some(mountinfo)) => mount_read_only(mountinfo, mount_id),
            _ => None,
        };
        let (mount_point, filesystem) = match entry {
            Some((point, filesystem, _)) => (Some(point), Some(filesystem)),
            None => (None, None),
        };
        let mut classes: std::collections::BTreeMap<(u32, u32, u32), (u64, String)> =
            std::collections::BTreeMap::new();
        let mut files = 0u64;
        let walk_refusal = (|| -> Result<(), String> {
            let root = path.to_path_buf();
            let root_mount = mount_id.ok_or("the mount of the root is unreadable")?;
            let metadata = std::fs::metadata(&root)
                .map_err(|error| format!("{} is unreadable ({error})", root.display()))?;
            if !metadata.is_dir() {
                return Err(format!("{} is not a directory", root.display()));
            }
            let mut pending = vec![(root.clone(), 0usize)];
            while let Some((directory, depth)) = pending.pop() {
                if depth > super::MAX_IMMUTABLE_ROOT_DEPTH {
                    return Err(format!(
                        "{} lies deeper than the {} directory levels this walk measures",
                        directory.display(),
                        super::MAX_IMMUTABLE_ROOT_DEPTH
                    ));
                }
                let listing = std::fs::read_dir(&directory).map_err(|error| {
                    format!("{} is unreadable ({error})", directory.display())
                })?;
                for item in listing {
                    let item = item.map_err(|error| {
                        format!("{} is unreadable ({error})", directory.display())
                    })?;
                    let entry_path = item.path();
                    let metadata = std::fs::symlink_metadata(&entry_path).map_err(|error| {
                        format!("{} is unreadable ({error})", entry_path.display())
                    })?;
                    let kind = metadata.file_type();
                    if kind.is_dir() {
                        if mount_of_path(&entry_path) != Some(root_mount) {
                            return Err(format!(
                                "{} is on a different mount than the root, so its state is not the root's",
                                entry_path.display()
                            ));
                        }
                        pending.push((entry_path, depth + 1));
                    } else if kind.is_file() {
                        files += 1;
                        if files > super::MAX_IMMUTABLE_ROOT_FILES {
                            return Err(format!(
                                "more than {} regular files",
                                super::MAX_IMMUTABLE_ROOT_FILES
                            ));
                        }
                        let key = (metadata.uid(), metadata.gid(), metadata.mode() & 0o7777);
                        let relative = entry_path
                            .strip_prefix(&root)
                            .unwrap_or(&entry_path)
                            .display()
                            .to_string();
                        classes.entry(key).or_insert((0, relative)).0 += 1;
                    }
                }
            }
            Ok(())
        })()
        .err();
        super::ImmutableRootFacts {
            path: path.display().to_string(),
            mount_id,
            mount_point,
            filesystem,
            mount_read_only: read_only.map(|(mount, _)| mount),
            superblock_read_only: read_only.map(|(_, superblock)| superblock),
            credentials: process_credentials(),
            file_classes: classes
                .into_iter()
                .map(
                    |((owner_uid, owner_gid, mode), (count, example))| super::FileClassFacts {
                        owner_uid,
                        owner_gid,
                        mode,
                        files: count,
                        example,
                    },
                )
                .collect(),
            files_measured: files,
            walk_refusal,
            kernel_kind: kernel_kind(&read_small("/proc/version").unwrap_or_default()).to_owned(),
        }
    }

    #[cfg(test)]
    mod tests {
        /// Hold a descriptor lease on another thread for `hold`, returning
        /// once it is held. Process creation must wait for it to drop.
        fn lease_held_elsewhere_for(hold: std::time::Duration) -> std::thread::JoinHandle<()> {
            let (held, observed) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let lease = crate::exec::try_retain_fork_sensitive_descriptors().unwrap();
                held.send(()).unwrap();
                std::thread::sleep(hold);
                drop(lease);
            });
            observed.recv().unwrap();
            holder
        }

        #[test]
        fn probe_child_is_created_only_under_the_fork_window() {
            // An unguarded fork ignores descriptor leases; this one refuses
            // a caller that holds one, and waits for one held elsewhere.
            let lease = crate::exec::try_retain_fork_sensitive_descriptors().unwrap();
            match super::probe_in_child(super::ChildProbe::Subreaper) {
                super::ProbeOutcome::Unobservable { reason } => {
                    assert!(
                        reason.contains("retains fork-sensitive descriptor authority"),
                        "{reason}"
                    );
                }
                other => {
                    panic!("probe forked while this thread held a descriptor lease: {other:?}")
                }
            }
            drop(lease);
            let hold = std::time::Duration::from_millis(300);
            let holder = lease_held_elsewhere_for(hold);
            let started = std::time::Instant::now();
            let outcome = super::probe_in_child(super::ChildProbe::Subreaper);
            let waited = started.elapsed();
            holder.join().unwrap();
            assert_eq!(outcome, super::ProbeOutcome::Allowed);
            assert!(
                waited >= hold - std::time::Duration::from_millis(50),
                "{waited:?}"
            );
            // The window reopened: a descriptor lease is granted at once.
            drop(crate::exec::try_retain_fork_sensitive_descriptors().unwrap());
        }

        #[test]
        fn mountinfo_entry_is_found_by_mount_id() {
            let mountinfo = "22 1 0:21 / / rw,relatime shared:1 - overlay overlay rw,lowerdir=/l\n\
                             31 22 8:16 / /data\\040app rw - ext4 /dev/vdb rw\n";
            assert_eq!(
                super::mount_entry(mountinfo, 31),
                Some((
                    "/data app".to_owned(),
                    "ext4".to_owned(),
                    "/dev/vdb".to_owned()
                ))
            );
            assert_eq!(
                super::mount_entry(mountinfo, 22),
                Some(("/".to_owned(), "overlay".to_owned(), "overlay".to_owned()))
            );
            assert_eq!(super::mount_entry(mountinfo, 7), None);
        }

        #[test]
        fn status_fields_parse_capability_and_locked_memory() {
            let status = "Name:\tx\nVmLck:\t    8192 kB\nCapEff:\t0000000000004000\n";
            assert!(super::cap_ipc_lock(status));
            assert_eq!(super::locked_bytes(status), Some(8 * 1024 * 1024));
            assert!(!super::cap_ipc_lock("CapEff:\t00000000a80405fb\n"));
            assert!(super::cap_ipc_lock("CapEff:\t000001ffffffffff\n"));
        }

        #[test]
        fn zero_swap_proof_must_enclose_every_cgroup_the_node_could_enter() {
            let enclosing: Vec<(String, std::path::PathBuf)> = ["/a/b/c", "/a/b", "/a", "/"]
                .iter()
                .map(|name| (name.to_string(), std::path::PathBuf::from(name)))
                .collect();
            let zero_at = |zero: &'static [&'static str]| {
                move |directory: &std::path::Path| {
                    Some(
                        zero.iter()
                            .any(|name| std::path::Path::new(name) == directory),
                    )
                }
            };
            // Nothing movable: the nearest zero limit proves.
            assert_eq!(
                super::zero_swap_at_or_above(&enclosing, None, zero_at(&["/a/b"])).as_deref(),
                Some("/a/b")
            );
            // The node can write /a's cgroup.procs, so a limit on /a/b can be left.
            assert_eq!(
                super::zero_swap_at_or_above(&enclosing, Some(2), zero_at(&["/a/b"])),
                None
            );
            assert_eq!(
                super::zero_swap_at_or_above(&enclosing, Some(2), zero_at(&["/a/b", "/a"]))
                    .as_deref(),
                Some("/a")
            );
            assert_eq!(
                super::zero_swap_at_or_above(&enclosing, Some(2), zero_at(&["/"])).as_deref(),
                Some("/")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn locality_storage_device_is_shared_by_one_filesystem_and_absent_when_unreadable() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        let outer = LocalityObservation::observe(root.path());
        let nested = LocalityObservation::observe(&inner);
        assert!(outer.storage_device.is_some());
        assert_eq!(outer.storage_device, nested.storage_device);
        let missing = LocalityObservation::observe(&root.path().join("missing"));
        assert_eq!(missing.storage_device, None);
        // The boot is the kernel's, so it is the same for every root.
        assert_eq!(outer.boot_id, missing.boot_id);
        assert_eq!(outer.kernel_kind, missing.kernel_kind);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn locality_account_is_reported_only_from_the_initial_user_namespace() {
        let observed = LocalityObservation::observe(std::path::Path::new("/"));
        assert_eq!(
            observed.account_uid.is_some(),
            observed.initial_user_namespace == Some(true)
        );
    }

    fn facts(
        enclosure_exit: Result<CapabilityEvidence, String>,
        container_enclosure: Option<u64>,
    ) -> HostFacts {
        let unobservable = || ProbeOutcome::Unobservable {
            reason: "fixture".to_owned(),
        };
        HostFacts {
            version: HOST_FACTS_VERSION,
            kernel_release: "6.0".to_owned(),
            kernel_kind: "native".to_owned(),
            boot_id: "00000000-0000-0000-0000-000000000000".to_owned(),
            pid_namespace: Some(7),
            initial_pid_namespace: false,
            cgroup_namespace: Some(8),
            initial_cgroup_namespace: false,
            container_enclosure,
            node_pid: 7,
            parent_pid: 1,
            pid1: Some("tini".to_owned()),
            pid1_executable: Some("tini".to_owned()),
            enclosure_exit_refusal: enclosure_exit.as_ref().err().cloned(),
            enclosure_exit: enclosure_exit.ok(),
            uid_map: None,
            cgroup: CgroupFacts {
                filesystem: "cgroup2".to_owned(),
                membership: Some("/".to_owned()),
                enclosure_inode: container_enclosure,
                mount_read_only: Some(true),
                nsdelegate: Some(false),
                membership_writable: false,
                memory_max: None,
                memory_swap_max: None,
                pids_max: None,
            },
            swap_devices: Some(Vec::new()),
            memlock: MemlockFacts {
                soft: None,
                hard: None,
                locked_bytes: None,
                cap_ipc_lock: false,
            },
            unshare_user: unobservable(),
            unshare_user_pid_mount: unobservable(),
            pidfd: unobservable(),
            peer_pidfd: unobservable(),
            pidfd_signal_process_group: unobservable(),
            subreaper: unobservable(),
            memfd_exec: unobservable(),
            resident_bytes: 0,
            residency: None,
            residency_refusal: Some("fixture".to_owned()),
            app_root_storage: storage("ext4", 0xef53, 40, 1, Some(true)),
            declared_lifetime: None,
        }
    }

    fn storage(
        filesystem: &str,
        f_type: u64,
        mount_id: u64,
        root_mount_id: u64,
        private_pid_namespace: Option<bool>,
    ) -> RootStorageFacts {
        RootStorageFacts {
            path: "/data/app".to_owned(),
            filesystem: Some(filesystem.to_owned()),
            f_type: Some(f_type),
            mount_id: Some(mount_id),
            mount_point: Some(
                if mount_id == root_mount_id {
                    "/"
                } else {
                    "/data"
                }
                .to_owned(),
            ),
            mount_source: Some("/dev/vdb".to_owned()),
            root_mount_id: Some(root_mount_id),
            private_pid_namespace,
            hypervisor: Some(false),
            hypervisor_basis: "fixture: bare metal".to_owned(),
            kernel_kind: "native".to_owned(),
        }
    }

    fn in_vm(mut facts: RootStorageFacts) -> RootStorageFacts {
        facts.hypervisor = Some(true);
        facts.hypervisor_basis = "the CPU reports the hypervisor flag".to_owned();
        facts
    }

    #[test]
    fn durable_root_is_decided_from_filesystem_and_mount() {
        let in_container = Some(true);
        let on_host = Some(false);
        let refused = [
            (
                storage("overlay", 0x794c_7630, 1, 1, in_container),
                "overlay (the container's writable layer)",
            ),
            (
                storage("tmpfs", 0x0102_1994, 40, 1, in_container),
                "memory-backed",
            ),
            (
                storage("ramfs", 0x8584_58f6, 40, 1, on_host),
                "memory-backed",
            ),
            // The magic number alone names a union filesystem.
            (
                storage("ext4", 0x794c_7630, 40, 1, in_container),
                "the container's writable layer",
            ),
            (
                storage("ext4", 0xef53, 1, 1, in_container),
                "container's root filesystem",
            ),
            (storage("ext4", 0xef53, 1, 1, None), "undecided"),
            (
                storage("9p", 0x0102_1997, 40, 1, in_container),
                "cannot classify",
            ),
        ];
        for (facts, reason) in refused {
            let refusal = decide_durable_root(&facts).unwrap_err();
            assert!(refusal.contains(reason), "{facts:?}: {refusal}");
            assert!(refusal.contains("/data/app"), "{refusal}");
        }
        let own_mount = decide_durable_root(&storage("ext4", 0xef53, 40, 1, in_container)).unwrap();
        assert_eq!(own_mount.source, ProofSource::Measured);
        assert!(
            own_mount.basis.contains("durable by mount"),
            "{}",
            own_mount.basis
        );
        assert!(
            own_mount
                .basis
                .contains("nothing here proves the provider keeps it"),
            "{}",
            own_mount.basis
        );
        assert!(own_mount.basis.contains("/dev/vdb"), "{}", own_mount.basis);
        let network = decide_durable_root(&storage("nfs4", 0x6969, 40, 1, in_container)).unwrap();
        assert_eq!(network.source, ProofSource::Measured);
        assert!(
            network.basis.contains("network filesystem"),
            "{}",
            network.basis
        );
        let host_root = decide_durable_root(&storage("ext4", 0xef53, 1, 1, on_host)).unwrap();
        assert_eq!(host_root.source, ProofSource::Measured);
        assert!(
            host_root.basis.contains("initial pid namespace"),
            "{}",
            host_root.basis
        );
        assert!(
            host_root.basis.contains("no hypervisor is detected"),
            "{}",
            host_root.basis
        );

        // A virtual machine's root disk is refused; its separate volume is not.
        let vm_root =
            decide_durable_root(&in_vm(storage("ext4", 0xef53, 1, 1, on_host))).unwrap_err();
        assert!(vm_root.contains("virtual machine's root disk"), "{vm_root}");
        assert!(vm_root.contains("separately attached volume"), "{vm_root}");
        let vm_volume =
            decide_durable_root(&in_vm(storage("ext4", 0xef53, 40, 1, on_host))).unwrap();
        assert_eq!(vm_volume.source, ProofSource::Measured);
        assert!(
            vm_volume.basis.contains("durable by mount"),
            "{}",
            vm_volume.basis
        );
        let mut undecided = storage("xfs", 0x5846_5342, 1, 1, on_host);
        undecided.hypervisor = None;
        assert!(
            decide_durable_root(&undecided)
                .unwrap_err()
                .contains("virtual is undecided")
        );

        let mut unreadable = storage("ext4", 0xef53, 40, 1, in_container);
        unreadable.filesystem = None;
        unreadable.f_type = None;
        assert!(
            decide_durable_root(&unreadable)
                .unwrap_err()
                .contains("unreadable")
        );
        // The magic number classifies when the mount table is unreadable.
        unreadable.f_type = Some(0x5846_5342);
        assert!(decide_durable_root(&unreadable).is_ok());
        unreadable.root_mount_id = None;
        assert!(
            decide_durable_root(&unreadable)
                .unwrap_err()
                .contains("undecided")
        );

        let mut emulated = storage("ext4", 0xef53, 40, 1, in_container);
        emulated.kernel_kind = "gvisor".to_owned();
        let attested = decide_durable_root(&emulated).unwrap();
        assert_eq!(attested.source, ProofSource::ProviderAttested);
        assert!(attested.basis.contains("gVisor"), "{}", attested.basis);
        assert_eq!(attested.provider, HOST_PROVIDER);
        let bare = CapabilityRequirement::from(STORAGE_DURABLE_ROOT);
        let refusal = satisfies(&bare, STORAGE_DURABLE_ROOT, &attested).unwrap_err();
        assert_eq!(
            refusal,
            "storage:durable_root is only provider_attested-proven here by host; the floor accepts measured"
        );
        emulated.filesystem = Some("overlay".to_owned());
        assert!(decide_durable_root(&emulated).is_err());
    }

    #[test]
    fn hypervisor_is_decided_from_unprivileged_signs() {
        let bare = "processor\t: 0\nflags\t\t: fpu vme sse2 avx2\n";
        let guest = "processor\t: 0\nflags\t\t: fpu vme sse2 hypervisor avx2\n";
        let arm = "processor\t: 0\nFeatures\t: fp asimd\n";
        let decided = decide_hypervisor(Some(bare), None, Some("Dell Inc.\n"), Some("Inspiron\n"));
        assert_eq!(decided.0, Some(false), "{}", decided.1);
        assert!(decided.1.contains("Dell Inc."), "{}", decided.1);
        assert_eq!(
            decide_hypervisor(Some(guest), None, None, None).0,
            Some(true)
        );
        assert_eq!(
            decide_hypervisor(Some(bare), Some("xen\n"), None, None).0,
            Some(true)
        );
        let firmware = decide_hypervisor(Some(bare), None, Some("QEMU"), Some("Standard PC"));
        assert_eq!(firmware.0, Some(true));
        assert!(firmware.1.contains("QEMU"), "{}", firmware.1);
        assert_eq!(
            decide_hypervisor(Some(arm), None, Some("Amazon EC2"), None).0,
            Some(true)
        );
        // Without the x86 flag line or the firmware identity nothing is decided.
        assert_eq!(
            decide_hypervisor(Some(arm), None, Some("Ampere"), None).0,
            None
        );
        assert_eq!(decide_hypervisor(Some(bare), None, None, None).0, None);
        assert_eq!(
            decide_hypervisor(None, None, Some("Dell Inc."), None).0,
            None
        );
    }

    #[test]
    fn host_capabilities_carry_the_durable_root_decision() {
        let mut host = facts(Err("fixture".to_owned()), None);
        let view = host.capabilities();
        assert_eq!(
            view.provided[STORAGE_DURABLE_ROOT].source,
            ProofSource::Measured
        );
        host.app_root_storage = storage("overlay", 0x794c_7630, 1, 1, Some(true));
        let view = host.capabilities();
        assert!(
            view.missing[STORAGE_DURABLE_ROOT].contains("overlay"),
            "{view:?}"
        );
        assert!(!view.provided.contains_key(STORAGE_DURABLE_ROOT));
    }

    fn bounded_day() -> HostLifetimeBound {
        HostLifetimeBound {
            provider: "example-host".to_owned(),
            seconds: 24 * 3600,
        }
    }

    #[test]
    fn durable_state_needs_durable_storage_and_no_declared_lifetime() {
        let volume = storage("ext4", 0xef53, 40, 1, Some(true));
        // The storage half is measured, but the absence of a lifetime bound
        // is only declared, so the whole is trusted and says so.
        assert_eq!(
            decide_durable_root(&volume).unwrap().source,
            ProofSource::Measured
        );
        let held = decide_durable_state(&volume, None).unwrap();
        assert_eq!(
            (held.source, held.provider.as_str()),
            (ProofSource::Trusted, HOST_PROVIDER)
        );
        assert!(held.basis.contains("durable by mount"), "{}", held.basis);
        assert!(
            held.basis.contains("no hard lifetime is declared")
                && held
                    .basis
                    .contains("that absence is declared data, not measured"),
            "{}",
            held.basis
        );
        // A disposable virtual machine: its root disk, and a declared 24 h bound.
        let mut machine_root = storage("ext4", 0xef53, 1, 1, Some(false));
        machine_root.hypervisor = Some(true);
        machine_root.hypervisor_basis =
            "the firmware names a virtual machine (Firecracker)".to_owned();
        let refused = decide_durable_state(&machine_root, Some(&bounded_day())).unwrap_err();
        assert!(
            refused.starts_with("no persistent disk; 24 h hard lifetime ("),
            "{refused}"
        );
        assert!(refused.contains("virtual machine's root disk"), "{refused}");
        assert!(
            refused.contains("provider example-host ends this host 24 h (86400 s)"),
            "{refused}"
        );
        // Either half alone refuses with its own plain reason.
        let overlay = storage("overlay", 0x794c_7630, 1, 1, Some(true));
        assert!(
            decide_durable_state(&overlay, None)
                .unwrap_err()
                .starts_with("no persistent disk (")
        );
        let bounded = decide_durable_state(&volume, Some(&bounded_day())).unwrap_err();
        assert!(bounded.starts_with("24 h hard lifetime ("), "{bounded}");
        // Under gVisor the storage half is provider-attested, and the whole
        // keeps that grade rather than rising to trusted.
        let mut emulated = volume.clone();
        emulated.kernel_kind = "gvisor".to_owned();
        assert_eq!(
            decide_durable_state(&emulated, None).unwrap().source,
            ProofSource::ProviderAttested
        );
        assert_eq!(
            HostLifetimeBound {
                provider: "p".to_owned(),
                seconds: 90
            }
            .describe(),
            "90 s"
        );
        assert_eq!(
            HostLifetimeBound {
                provider: "p".to_owned(),
                seconds: 14_400
            }
            .describe(),
            "4 h"
        );
        assert!(
            HostLifetimeBound {
                provider: "p".to_owned(),
                seconds: 0
            }
            .refusal()
            .is_some()
        );
        assert!(
            HostLifetimeBound {
                provider: "a b".to_owned(),
                seconds: 1
            }
            .refusal()
            .is_some()
        );
        assert_eq!(bounded_day().refusal(), None);
    }

    #[test]
    fn host_capabilities_carry_the_durable_state_decision() {
        let mut host = facts(Err("fixture".to_owned()), None);
        let view = host.capabilities();
        assert_eq!(
            view.provided[STORAGE_DURABLE_ROOT].source,
            ProofSource::Measured
        );
        assert_eq!(
            view.provided[PERSISTENCE_DURABLE_STATE].source,
            ProofSource::Trusted
        );
        host.declared_lifetime = Some(bounded_day());
        let view = host.capabilities();
        // The storage proof stands; the declared bound withholds durable state.
        assert!(view.provided.contains_key(STORAGE_DURABLE_ROOT));
        assert!(
            view.missing[PERSISTENCE_DURABLE_STATE].starts_with("24 h hard lifetime"),
            "{view:?}"
        );
        let value = serde_json::to_value(&host).unwrap();
        assert_eq!(value["declared_lifetime"]["seconds"], 86_400);
        assert_eq!(serde_json::from_value::<HostFacts>(value).unwrap(), host);
    }

    fn step(path: &str, owner: u32, mode: u32, mount: u64) -> DirectoryStepFacts {
        DirectoryStepFacts {
            path: path.to_owned(),
            owner_uid: owner,
            mode,
            mount_id: mount,
        }
    }

    /// A walk from `/data/app` down to `/data/app/state/homes` by uid 1000,
    /// every step on mount 40.
    fn private(home_mode: u32, home_owner: u32, home_mount: u64) -> PrivateDirectoryFacts {
        PrivateDirectoryFacts {
            path: "/data/app/state/homes".to_owned(),
            anchor: "/data/app".to_owned(),
            process_uid: Some(1000),
            chain: vec![
                step("/data/app", 1000, 0o755, 40),
                step("/data/app/state", 0, 0o755, 40),
                step("/data/app/state/homes", home_owner, home_mode, home_mount),
            ],
            walk_refusal: None,
        }
    }

    #[test]
    fn a_private_directory_is_owned_0700_on_the_anchor_mount_below_safe_ancestors() {
        let basis = decide_private_directory(&private(0o700, 1000, 40)).unwrap();
        assert!(
            basis.contains("private to uid 1000")
                && basis.contains("mode 0700")
                && basis.contains("mount 40"),
            "{basis}"
        );
        assert!(
            basis.contains("from /data/app down") && basis.contains("uid 1000 or root"),
            "{basis}"
        );
        assert!(
            decide_private_directory(&private(0o750, 1000, 40))
                .unwrap_err()
                .contains("mode 0750")
        );
        assert!(
            decide_private_directory(&private(0o1700, 1000, 40))
                .unwrap_err()
                .contains("mode 1700")
        );
        assert!(
            decide_private_directory(&private(0o700, 0, 40))
                .unwrap_err()
                .contains("owned by uid 0")
        );
        // A bind mount at the directory, or anywhere on the walk, is refused.
        let bound = decide_private_directory(&private(0o700, 1000, 41)).unwrap_err();
        assert_eq!(
            bound,
            "/data/app/state/homes is on mount 41, not the mount 40 of /data/app"
        );
        // The first failing ancestor is named: another account's directory,
        // then a group-writable one without the sticky bit.
        let mut foreign = private(0o700, 1000, 40);
        foreign.chain[1].owner_uid = 1001;
        let refused = decide_private_directory(&foreign).unwrap_err();
        assert!(
            refused
                .starts_with("/data/app/state, above /data/app/state/homes, is owned by uid 1001"),
            "{refused}"
        );
        let mut shared = private(0o700, 1000, 40);
        shared.chain[0].mode = 0o775;
        shared.chain[1].mode = 0o777;
        let refused = decide_private_directory(&shared).unwrap_err();
        assert!(
            refused.starts_with("/data/app, above /data/app/state/homes, has mode 0775"),
            "{refused}"
        );
        // A sticky shared directory cannot have our entries replaced.
        let mut sticky = private(0o700, 1000, 40);
        sticky.chain[1].mode = 0o1777;
        decide_private_directory(&sticky).unwrap();
        // A walk that stopped is refused with its own reason.
        let mut stopped = private(0o700, 1000, 40);
        stopped.chain.truncate(1);
        stopped.walk_refusal = Some("/data/app/state is a symbolic link".to_owned());
        assert_eq!(
            decide_private_directory(&stopped).unwrap_err(),
            "/data/app/state is a symbolic link"
        );
        let mut short = private(0o700, 1000, 40);
        short.chain.pop();
        assert!(
            decide_private_directory(&short)
                .unwrap_err()
                .contains("ended at /data/app/state")
        );
        // As root the directory is private to uid 0, and the basis says so.
        let mut root = private(0o700, 0, 40);
        root.process_uid = Some(0);
        root.chain[0].owner_uid = 0;
        let basis = decide_private_directory(&root).unwrap();
        assert!(
            basis.starts_with("/data/app/state/homes is private to uid 0 (root):"),
            "{basis}"
        );
        assert!(basis.contains("is owned by root,"), "{basis}");
    }

    #[cfg(target_os = "linux")]
    fn private_tree() -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        let home = state.join("homes");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        (root, home)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_directory_observes_a_real_walk() {
        use std::os::unix::fs::PermissionsExt as _;
        let (root, home) = private_tree();
        let facts = PrivateDirectoryFacts::observe(root.path(), &home);
        assert_eq!(facts.walk_refusal, None, "{facts:?}");
        assert_eq!(facts.chain.len(), 3, "{facts:?}");
        assert_eq!(facts.chain[2].mode, 0o700);
        assert_eq!(
            Some(facts.chain[2].owner_uid),
            facts.process_uid,
            "{facts:?}"
        );
        let storage = RootStorageFacts::observe(root.path());
        assert_eq!(Some(facts.chain[0].mount_id), storage.mount_id, "{facts:?}");
        decide_private_directory(&facts).unwrap();
        // A group-writable ancestor is named.
        let state = root.path().join("state");
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o775)).unwrap();
        let refused = decide_private_directory(&PrivateDirectoryFacts::observe(root.path(), &home))
            .unwrap_err();
        assert!(
            refused.contains(&format!("{}, above", state.display())) && refused.contains("0775"),
            "{refused}"
        );
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        let open = PrivateDirectoryFacts::observe(root.path(), &home);
        assert!(
            decide_private_directory(&open)
                .unwrap_err()
                .contains("0755")
        );
        let missing = PrivateDirectoryFacts::observe(root.path(), &root.path().join("absent"));
        assert_eq!(
            missing.walk_refusal,
            Some(format!(
                "{} is absent",
                root.path().join("absent").display()
            ))
        );
        assert!(decide_private_directory(&missing).is_err());
        let outside = PrivateDirectoryFacts::observe(&state, root.path());
        assert!(
            outside
                .walk_refusal
                .unwrap()
                .contains("does not lie beneath")
        );
        let climbing = PrivateDirectoryFacts::observe(root.path(), &state.join("..").join("state"));
        assert!(
            climbing
                .walk_refusal
                .unwrap()
                .contains("plain directory names")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_directory_walk_never_follows_a_symbolic_link() {
        // The private directory itself is a link to a private directory.
        let (root, home) = private_tree();
        let real = root.path().join("state").join("homes-real");
        std::fs::rename(&home, &real).unwrap();
        std::os::unix::fs::symlink(&real, &home).unwrap();
        let facts = PrivateDirectoryFacts::observe(root.path(), &home);
        assert_eq!(
            facts.walk_refusal,
            Some(format!("{} is a symbolic link", home.display()))
        );
        assert_eq!(facts.chain.len(), 2, "{facts:?}");
        // An ancestor is a link.
        let (root, home) = private_tree();
        let state = root.path().join("state");
        let moved = root.path().join("state-real");
        std::fs::rename(&state, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &state).unwrap();
        let facts = PrivateDirectoryFacts::observe(root.path(), &home);
        assert_eq!(
            facts.walk_refusal,
            Some(format!("{} is a symbolic link", state.display()))
        );
        // The anchor itself is a link.
        let (root, _home) = private_tree();
        let linked_root = root.path().join("linked-root");
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::os::unix::fs::symlink(root.path().join("target"), &linked_root).unwrap();
        let facts = PrivateDirectoryFacts::observe(&linked_root, &linked_root.join("homes"));
        assert_eq!(
            facts.walk_refusal,
            Some(format!("{} is a symbolic link", linked_root.display()))
        );
        assert!(facts.chain.is_empty());
        // A path swapped after a private observation is judged as it now
        // stands: a link put where the directory was is refused even though
        // it leads to the very directory that was private.
        let (root, home) = private_tree();
        decide_private_directory(&PrivateDirectoryFacts::observe(root.path(), &home)).unwrap();
        let aside = root.path().join("state").join("aside");
        std::fs::rename(&home, &aside).unwrap();
        std::os::unix::fs::symlink(&aside, &home).unwrap();
        let swapped = PrivateDirectoryFacts::observe(root.path(), &home);
        assert!(
            decide_private_directory(&swapped)
                .unwrap_err()
                .ends_with("is a symbolic link")
        );
        // A regular file in its place is not a directory.
        std::fs::remove_file(&home).unwrap();
        std::fs::write(&home, b"").unwrap();
        let file = PrivateDirectoryFacts::observe(root.path(), &home);
        assert_eq!(
            file.walk_refusal,
            Some(format!("{} is not a directory", home.display()))
        );
    }

    #[test]
    fn a_view_keeps_bounded_printable_text_and_fits_a_guest_frame() {
        let mut view = HostCapabilities::default();
        view.provide(
            "time",
            ProofSource::Measured,
            "p\u{e9}r \"x\"",
            "a\\b\nc\u{2014}d",
        );
        assert_eq!(view.provided["time"].provider, "p?r 'x'");
        assert_eq!(view.provided["time"].basis, "a/b?c?d");
        view.refuse("other", "r".repeat(2 * MAX_CAPABILITY_TEXT_BYTES));
        let reason = &view.missing["other"];
        assert_eq!(reason.len(), MAX_CAPABILITY_TEXT_BYTES);
        assert!(reason.ends_with(" [cut]"));
        // The worst view: every name at the name bound, every entry
        // provided with the longest provider id, the longest known grade
        // and the longest basis, every character one JSON would escape.
        // A kept string encodes as its length plus two quotes, so a provided
        // entry encodes in at most
        //   (128 + 2) name + 1 colon + 10 `{"source":` + (17 + 2) grade
        //   + 12 `,"provider":` + (128 + 2) provider + 9 `,"basis":`
        //   + (512 + 2) basis + 1 `}` + 1 comma = 827 bytes,
        // and a missing entry, (128 + 2) + 1 + (512 + 2) + 1 = 646, in less.
        // 32 entries are at most 26,464 bytes, under half the 64 KiB guest
        // frame, which leaves the rest for the frame envelope.
        let mut worst = HostCapabilities::default();
        for index in 0..MAX_CAPABILITY_ENTRIES {
            let name = format!("{index:0>width$}", width = MAX_CAPABILITY_NAME_BYTES);
            worst.provide(
                &name,
                ProofSource::ProviderAttested,
                &"\"".repeat(2 * MAX_CAPABILITY_NAME_BYTES),
                "\\".repeat(2 * MAX_CAPABILITY_TEXT_BYTES),
            );
        }
        let encoded = serde_json::to_vec(&worst).unwrap();
        let entry_bound = (MAX_CAPABILITY_NAME_BYTES + 2)
            + 1
            + 10
            + ("provider_attested".len() + 2)
            + 12
            + (MAX_CAPABILITY_NAME_BYTES + 2)
            + 9
            + (MAX_CAPABILITY_TEXT_BYTES + 2)
            + 1
            + 1;
        assert_eq!(entry_bound, 827);
        assert!(
            encoded.len() <= MAX_CAPABILITY_ENTRIES * entry_bound + 32,
            "{}",
            encoded.len()
        );
        assert!(encoded.len() <= 32 * 1024, "{}", encoded.len());
        // Every name the host view decides is within the name bound, and the
        // host view alone is within the entry bound.
        let host = facts(Err("fixture".to_owned()), None).capabilities();
        let names: Vec<&String> = host.provided.keys().chain(host.missing.keys()).collect();
        assert!(
            names
                .iter()
                .all(|name| name.len() <= MAX_CAPABILITY_NAME_BYTES),
            "{names:?}"
        );
        assert!(names.len() <= MAX_CAPABILITY_ENTRIES, "{names:?}");
    }

    fn immutable_facts(
        mount_ro: bool,
        caps: u64,
        uid: u32,
        classes: &[(u32, u32)],
    ) -> ImmutableRootFacts {
        ImmutableRootFacts {
            path: "/opt/store".to_owned(),
            mount_id: Some(40),
            mount_point: Some("/opt".to_owned()),
            filesystem: Some("ext4".to_owned()),
            mount_read_only: Some(mount_ro),
            superblock_read_only: Some(false),
            credentials: Some(ProcessCredentialFacts {
                uid,
                effective_caps: caps,
                permitted_caps: caps,
            }),
            file_classes: classes
                .iter()
                .map(|(owner_uid, mode)| FileClassFacts {
                    owner_uid: *owner_uid,
                    owner_gid: *owner_uid,
                    mode: *mode,
                    files: 3,
                    example: format!("file-{owner_uid}-{mode:o}"),
                })
                .collect(),
            files_measured: 3 * classes.len() as u64,
            walk_refusal: None,
            kernel_kind: "native".to_owned(),
        }
    }

    #[test]
    fn immutable_root_read_only_mount_without_remount_capability_is_measured() {
        let proof =
            decide_immutable_root(&immutable_facts(true, 0, 1000, &[(1000, 0o644)])).unwrap();
        assert_eq!(proof.source, ProofSource::Measured);
        assert!(proof.basis.contains("read-only mount"), "{}", proof.basis);
        // The superblock alone also makes the mount read-only.
        let mut superblock = immutable_facts(false, 0, 1000, &[(1000, 0o644)]);
        superblock.superblock_read_only = Some(true);
        assert!(decide_immutable_root(&superblock).is_ok());
    }

    #[test]
    fn immutable_root_read_only_mount_with_sys_admin_falls_to_permissions() {
        // Root-owned read-only files: the permission route still proves.
        let proof =
            decide_immutable_root(&immutable_facts(true, CAP_SYS_ADMIN, 1000, &[(0, 0o444)]));
        assert!(proof.is_err(), "{proof:?}");
        let reason = proof.unwrap_err();
        assert!(reason.contains("CAP_SYS_ADMIN"), "{reason}");
    }

    #[test]
    fn immutable_root_writable_mount_is_decided_by_owner_mode_and_capabilities() {
        // Not the node's files, no write bit for group or other, no capability.
        let proof =
            decide_immutable_root(&immutable_facts(false, 0, 1000, &[(0, 0o755), (0, 0o444)]))
                .unwrap();
        assert!(proof.basis.contains("owned by uid 1000"), "{}", proof.basis);
        for (caps, name) in [
            (CAP_DAC_OVERRIDE, "CAP_DAC_OVERRIDE"),
            (CAP_FOWNER, "CAP_FOWNER"),
            (CAP_CHOWN, "CAP_CHOWN"),
            (CAP_SYS_ADMIN, "CAP_SYS_ADMIN"),
        ] {
            let reason = decide_immutable_root(&immutable_facts(false, caps, 1000, &[(0, 0o755)]))
                .unwrap_err();
            assert!(reason.contains(name), "{reason}");
        }
        // A file the node owns can have its write bit restored.
        let owned =
            decide_immutable_root(&immutable_facts(false, 0, 1000, &[(1000, 0o444)])).unwrap_err();
        assert!(owned.contains("owner can restore"), "{owned}");
        // Group or other write.
        for mode in [0o775, 0o757, 0o666] {
            let reason =
                decide_immutable_root(&immutable_facts(false, 0, 1000, &[(0, mode)])).unwrap_err();
            assert!(reason.contains("writable by group or other"), "{reason}");
        }
        // Root without capabilities still owns what it owns.
        assert!(decide_immutable_root(&immutable_facts(false, 0, 0, &[(0, 0o444)])).is_err());
    }

    #[test]
    fn immutable_root_unmeasured_facts_are_not_proven() {
        let mut unreadable = immutable_facts(false, 0, 1000, &[(0, 0o444)]);
        unreadable.credentials = None;
        assert!(
            decide_immutable_root(&unreadable)
                .unwrap_err()
                .contains("unreadable")
        );
        let mut walk = immutable_facts(false, 0, 1000, &[(0, 0o444)]);
        walk.walk_refusal = Some("a different mount".to_owned());
        assert!(
            decide_immutable_root(&walk)
                .unwrap_err()
                .contains("a different mount")
        );
        // A read-only mount proves without the walk.
        walk.mount_read_only = Some(true);
        assert!(decide_immutable_root(&walk).is_ok());
        let empty = immutable_facts(false, 0, 1000, &[]);
        assert!(
            decide_immutable_root(&empty)
                .unwrap_err()
                .contains("no regular file")
        );
        let mut gvisor = immutable_facts(true, 0, 1000, &[(0, 0o444)]);
        gvisor.kernel_kind = "gvisor".to_owned();
        assert_eq!(
            decide_immutable_root(&gvisor).unwrap().source,
            ProofSource::ProviderAttested
        );
    }

    #[test]
    fn an_in_place_root_is_admitted_only_at_the_grades_it_accepts() {
        let measured =
            decide_immutable_root(&immutable_facts(true, 0, 1000, &[(0, 0o444)])).unwrap();
        let mut attested = immutable_facts(true, 0, 1000, &[(0, 0o444)]);
        attested.kernel_kind = "gvisor".to_owned();
        let attested = decide_immutable_root(&attested).unwrap();
        let grades = |names: &[&str]| {
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        // Absent grades admit only a measured proof, like a floor entry.
        assert!(admit_immutable_root_proof(&[], &measured).is_ok());
        let refused = admit_immutable_root_proof(&[], &attested).unwrap_err();
        assert!(refused.contains("provider_attested"), "{refused}");
        assert!(
            admit_immutable_root_proof(&grades(&["measured", "provider_attested"]), &attested)
                .is_ok()
        );
        assert!(admit_immutable_root_proof(&grades(&["provider_attested"]), &measured).is_err());
        assert!(admit_immutable_root_proof(&grades(&["peer_attested"]), &measured).is_err());
        assert!(admit_immutable_root_proof(&grades(&["bogus"]), &measured).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn immutable_root_observes_a_real_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        let file = root.path().join("nested/blob");
        std::fs::write(&file, b"bytes").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
        let facts = ImmutableRootFacts::observe(root.path());
        assert_eq!(facts.walk_refusal, None, "{facts:?}");
        assert_eq!(facts.files_measured, 1);
        assert_eq!(facts.file_classes.len(), 1);
        assert_eq!(facts.file_classes[0].mode, 0o444);
        assert_eq!(facts.file_classes[0].example, "nested/blob");
        assert!(facts.credentials.is_some() && facts.mount_read_only.is_some());
        // Owned by the test's own uid, so never proven by permissions.
        let reason = decide_immutable_root(&facts);
        if facts.mount_read_only != Some(true) && facts.superblock_read_only != Some(true) {
            assert!(reason.unwrap_err().contains("owner can restore"));
        }
        let missing = ImmutableRootFacts::observe(&root.path().join("absent"));
        assert!(missing.walk_refusal.is_some());
        assert!(decide_immutable_root(&missing).is_err());
    }

    #[test]
    fn mountinfo_read_only_options_are_read_by_mount_id() {
        let mountinfo = "31 22 8:16 / /opt ro,relatime shared:1 - ext4 /dev/vdb rw\n\
                         32 22 8:17 / /srv rw,relatime - ext4 /dev/vdc ro,errors=remount-ro\n\
                         33 22 8:18 / /var rw,relatime - ext4 /dev/vdd rw,errors=remount-ro\n";
        assert_eq!(linux::mount_read_only(mountinfo, 31), Some((true, false)));
        assert_eq!(linux::mount_read_only(mountinfo, 32), Some((false, true)));
        assert_eq!(linux::mount_read_only(mountinfo, 33), Some((false, false)));
        assert_eq!(linux::mount_read_only(mountinfo, 9), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn root_storage_observes_a_real_directory() {
        let root = tempfile::tempdir().unwrap();
        let facts = RootStorageFacts::observe(root.path());
        assert_eq!(facts.path, root.path().display().to_string());
        assert!(facts.f_type.is_some(), "{facts:?}");
        assert!(
            facts.mount_id.is_some() && facts.root_mount_id.is_some(),
            "{facts:?}"
        );
        assert!(
            facts.filesystem.is_some() && facts.mount_point.is_some(),
            "{facts:?}"
        );
        assert!(facts.private_pid_namespace.is_some(), "{facts:?}");
        assert!(!facts.hypervisor_basis.is_empty(), "{facts:?}");
        // Whatever this host is, the decision is reached with a reason.
        match decide_durable_root(&facts) {
            Ok(evidence) => assert!(!evidence.basis.is_empty()),
            Err(reason) => assert!(reason.contains(&facts.path), "{reason}"),
        }
        let missing = RootStorageFacts::observe(&root.path().join("absent"));
        assert_eq!((missing.mount_id, missing.f_type), (None, None));
        assert!(decide_durable_root(&missing).is_err());
    }

    fn inits() -> Vec<String> {
        ["catatonit", "dumb-init", "tini"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    }

    #[test]
    fn enclosure_exit_is_trusted_only_under_a_named_init() {
        // A node that is pid 1 is refused by name, with the inits to run under.
        let pid_one =
            decide_enclosure_exit(Some(true), 1, 0, Some("ryeosd"), &inits()).unwrap_err();
        assert!(
            pid_one.contains("pid 1 of its private pid namespace"),
            "{pid_one}"
        );
        assert!(pid_one.contains("catatonit, dumb-init, tini"), "{pid_one}");
        let none_named = decide_enclosure_exit(Some(true), 1, 0, Some("ryeosd"), &[]).unwrap_err();
        assert!(
            none_named.contains("node policy names none"),
            "{none_named}"
        );
        let trusted = decide_enclosure_exit(Some(true), 7, 1, Some("tini"), &inits()).unwrap();
        assert_eq!(trusted.source, ProofSource::Trusted);
        assert!(trusted.basis.contains("tini"), "{}", trusted.basis);
        // A known init that policy does not name is not trusted.
        let unlisted = decide_enclosure_exit(Some(true), 7, 1, Some("tini"), &[]).unwrap_err();
        assert!(unlisted.contains("does not name"), "{unlisted}");
        let shell = decide_enclosure_exit(Some(true), 7, 1, Some("sh"), &inits()).unwrap_err();
        assert!(shell.contains("(sh)"), "{shell}");
        let unreadable = decide_enclosure_exit(Some(true), 7, 1, None, &inits()).unwrap_err();
        assert!(unreadable.contains("unreadable"), "{unreadable}");
        // Not a child of pid 1, for example a process entered from outside.
        let entered = decide_enclosure_exit(Some(true), 40, 0, Some("tini"), &inits()).unwrap_err();
        assert!(entered.contains("nothing proves"), "{entered}");
        let host = decide_enclosure_exit(Some(false), 1, 0, Some("init"), &inits()).unwrap_err();
        assert!(host.contains("initial pid namespace"), "{host}");
        assert!(decide_enclosure_exit(None, 1, 0, None, &inits()).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_memfd_capability_reports_the_memfd_exec_flag_value() {
        let mut host = facts(
            decide_enclosure_exit(Some(true), 7, 1, Some("tini"), &inits()),
            Some(42),
        );
        let errno = |errno: i32| ProbeOutcome::Refused {
            errno,
            reason: format!("errno {errno}"),
        };
        host.memfd_exec = ProbeOutcome::Allowed;
        let view = host.capabilities();
        assert!(
            view.provided[EXEC_SEALED_MEMFD]
                .basis
                .contains("memfd_exec_flag=supported")
        );
        host.memfd_exec = errno(libc::EINVAL);
        let view = host.capabilities();
        assert!(
            view.provided[EXEC_SEALED_MEMFD]
                .basis
                .contains("memfd_exec_flag=unsupported_executable_by_default")
        );
        host.kernel_kind = "gvisor".to_owned();
        assert_eq!(
            host.capabilities().provided[EXEC_SEALED_MEMFD].source,
            ProofSource::ProviderAttested
        );
        host.memfd_exec = errno(libc::EACCES);
        let view = host.capabilities();
        assert!(!view.provided.contains_key(EXEC_SEALED_MEMFD));
        assert_eq!(view.missing[EXEC_SEALED_MEMFD], "memfd_exec_forbidden");
        host.memfd_exec = errno(libc::ENOMEM);
        assert_eq!(
            host.capabilities().missing[EXEC_SEALED_MEMFD],
            "memfd_exec_probe_failed"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn group_signal_answers_map_to_probe_outcomes() {
        assert_eq!(classify_group_signal(Ok(())), ProbeOutcome::Allowed);
        assert_eq!(
            classify_group_signal(Err(libc::ESRCH)),
            ProbeOutcome::Allowed
        );
        assert!(matches!(
            classify_group_signal(Err(libc::EINVAL)),
            ProbeOutcome::Refused { errno, .. } if errno == libc::EINVAL
        ));
        assert!(!classify_group_signal(Err(libc::EPERM)).allowed());
    }

    #[test]
    fn pidfd_capabilities_follow_their_probes() {
        let mut host = facts(
            decide_enclosure_exit(Some(true), 7, 1, Some("tini"), &inits()),
            Some(42),
        );
        let refused = |reason: &str| ProbeOutcome::Refused {
            errno: 22,
            reason: reason.to_owned(),
        };
        host.pidfd = ProbeOutcome::Allowed;
        host.peer_pidfd = ProbeOutcome::Allowed;
        host.pidfd_signal_process_group = ProbeOutcome::Allowed;
        let view = host.capabilities();
        assert_eq!(
            view.provided[IDENTITY_PEER_PIDFD].source,
            ProofSource::Measured
        );
        assert_eq!(
            view.provided[SIGNAL_PIDFD_PROCESS_GROUP].source,
            ProofSource::Measured
        );
        assert!(view.provided.contains_key("termination:process_group"));

        host.pidfd_signal_process_group = refused("no group flag");
        let view = host.capabilities();
        assert!(!view.provided.contains_key(SIGNAL_PIDFD_PROCESS_GROUP));
        assert!(view.missing[SIGNAL_PIDFD_PROCESS_GROUP].contains("no group flag"));
        // pidfd_open alone no longer proves exact process-group termination.
        assert!(!view.provided.contains_key("termination:process_group"));
        assert!(view.missing["termination:process_group"].contains("PIDFD_SIGNAL_PROCESS_GROUP"));

        host.pidfd_signal_process_group = ProbeOutcome::Allowed;
        host.peer_pidfd = refused("no peer pidfd");
        let view = host.capabilities();
        assert!(!view.provided.contains_key(IDENTITY_PEER_PIDFD));
        assert!(view.missing[IDENTITY_PEER_PIDFD].contains("no peer pidfd"));
        assert!(view.provided.contains_key("termination:process_group"));

        host.kernel_kind = "gvisor".to_owned();
        host.peer_pidfd = ProbeOutcome::Allowed;
        let view = host.capabilities();
        assert_eq!(
            view.provided[IDENTITY_PEER_PIDFD].source,
            ProofSource::ProviderAttested
        );
    }

    #[test]
    fn trusted_grades_fall_to_provider_attested_under_gvisor() {
        let mut under_tini = facts(
            decide_enclosure_exit(Some(true), 7, 1, Some("tini"), &inits()),
            Some(42),
        );
        under_tini.kernel_kind = "gvisor".to_owned();
        under_tini.pidfd = ProbeOutcome::Allowed;
        under_tini.pidfd_signal_process_group = ProbeOutcome::Allowed;
        let view = under_tini.capabilities();
        for name in [
            TERMINATION_ENCLOSURE_TRUSTED_INIT,
            RECOVERY_ENCLOSURE_TRUSTED_INIT,
            "termination:process_group",
        ] {
            let evidence = &view.provided[name];
            assert_eq!(evidence.source, ProofSource::ProviderAttested, "{name}");
            assert!(
                evidence.basis.contains("gVisor"),
                "{name}: {}",
                evidence.basis
            );
            assert!(
                satisfies(&CapabilityRequirement::from(name), name, evidence).is_err(),
                "{name}"
            );
            assert!(
                satisfies(&accepting(name, &["trusted"]), name, evidence).is_err(),
                "{name}"
            );
            satisfies(&accepting(name, &["provider_attested"]), name, evidence).unwrap();
        }

        under_tini.kernel_kind = "native".to_owned();
        let view = under_tini.capabilities();
        let process_group = &view.provided["termination:process_group"];
        assert_eq!(process_group.source, ProofSource::Trusted);
        // The grade is accepted only where the floor says so; the name alone
        // carries no trust.
        let bare = CapabilityRequirement::from("termination:process_group");
        let refusal = satisfies(&bare, "termination:process_group", process_group).unwrap_err();
        assert_eq!(
            refusal,
            "termination:process_group is only trusted-proven here by host; the floor accepts measured"
        );
        satisfies(
            &accepting("termination:process_group", &["trusted"]),
            "termination:process_group",
            process_group,
        )
        .unwrap();
        satisfies(
            &accepting("termination:process_group", &["measured", "trusted"]),
            "termination:process_group",
            process_group,
        )
        .unwrap();
    }

    #[test]
    fn enclosure_capabilities_follow_the_exit_grade() {
        let pid_one = facts(
            decide_enclosure_exit(Some(true), 1, 0, Some("ryeosd"), &inits()),
            Some(42),
        );
        let view = pid_one.capabilities();
        for name in [
            TERMINATION_ENCLOSURE_TRUSTED_INIT,
            RECOVERY_ENCLOSURE_TRUSTED_INIT,
        ] {
            assert!(!view.provided.contains_key(name), "{name}");
        }
        assert!(
            view.missing[TERMINATION_ENCLOSURE_TRUSTED_INIT].contains("process identity refuses"),
            "{view:?}"
        );

        let under_tini = facts(
            decide_enclosure_exit(Some(true), 7, 1, Some("tini"), &inits()),
            Some(42),
        );
        let view = under_tini.capabilities();
        assert_eq!(
            view.provided[TERMINATION_ENCLOSURE_TRUSTED_INIT].source,
            ProofSource::Trusted
        );
        assert_eq!(
            view.provided[RECOVERY_ENCLOSURE_TRUSTED_INIT].source,
            ProofSource::Trusted
        );
        assert!(
            view.provided[RECOVERY_ENCLOSURE_TRUSTED_INIT]
                .basis
                .contains("inode 42")
        );
        // A trusted proof meets a trusted-init name only where the floor
        // accepts the trusted grade; a bare name still wants a measurement.
        for name in [
            TERMINATION_ENCLOSURE_TRUSTED_INIT,
            RECOVERY_ENCLOSURE_TRUSTED_INIT,
        ] {
            assert!(
                satisfies(
                    &CapabilityRequirement::from(name),
                    name,
                    &view.provided[name]
                )
                .is_err(),
                "{name}"
            );
            satisfies(&accepting(name, &["trusted"]), name, &view.provided[name]).unwrap();
        }
        // A provider attestation of a trusted-init name does not meet a
        // floor that accepts trust.
        let attested = CapabilityEvidence {
            source: ProofSource::ProviderAttested,
            provider: HOST_PROVIDER.to_owned(),
            basis: String::new(),
        };
        let refusal = satisfies(
            &accepting(TERMINATION_ENCLOSURE_TRUSTED_INIT, &["trusted"]),
            TERMINATION_ENCLOSURE_TRUSTED_INIT,
            &attested,
        )
        .unwrap_err();
        assert!(
            refusal
                .contains("only provider_attested-proven here by host; the floor accepts trusted"),
            "{refusal}"
        );

        let other = facts(
            decide_enclosure_exit(Some(true), 7, 1, Some("sh"), &inits()),
            Some(42),
        );
        let view = other.capabilities();
        for name in [
            TERMINATION_ENCLOSURE_TRUSTED_INIT,
            RECOVERY_ENCLOSURE_TRUSTED_INIT,
        ] {
            assert!(!view.provided.contains_key(name), "{name}");
        }
        assert!(view.missing[RECOVERY_ENCLOSURE_TRUSTED_INIT].contains("undecided"));

        let not_whole = facts(
            decide_enclosure_exit(Some(true), 7, 1, Some("tini"), &inits()),
            None,
        );
        let view = not_whole.capabilities();
        assert!(
            view.provided
                .contains_key(TERMINATION_ENCLOSURE_TRUSTED_INIT)
        );
        assert!(
            view.missing[RECOVERY_ENCLOSURE_TRUSTED_INIT].contains("not the whole of a container")
        );
    }

    #[test]
    fn residency_names_follow_the_proof_basis() {
        let mut host = facts(Err("fixture".to_owned()), None);
        let cases = [
            (
                ResidencyProof::Locked {
                    basis: "cap".to_owned(),
                },
                RESIDENCY_LOCKED,
                true,
            ),
            (
                ResidencyProof::SwapLimit {
                    basis: "cgroup /a".to_owned(),
                },
                RESIDENCY_SWAP_LIMIT,
                true,
            ),
            (
                ResidencyProof::SwapAbsent {
                    basis: "no swap now".to_owned(),
                },
                RESIDENCY_SWAP_ABSENT,
                false,
            ),
        ];
        for (proof, held, standing) in cases {
            host.residency = Some(proof.clone());
            host.residency_refusal = None;
            let view = host.capabilities();
            assert_eq!(proof.standing(), standing, "{held}");
            assert_eq!(proof.capability_name(), Some(held));
            assert_eq!(view.provided[held].source, ProofSource::Measured, "{held}");
            assert!(view.provided[held].basis.contains(proof.basis()), "{held}");
            for other in [
                RESIDENCY_LOCKED,
                RESIDENCY_SWAP_LIMIT,
                RESIDENCY_SWAP_ABSENT,
            ] {
                if other != held {
                    assert!(view.missing[other].contains(held), "{other}: {view:?}");
                }
            }
            if standing {
                // The strong name shows the basis name that proves it.
                assert!(
                    view.provided[ACCOUNTING].basis.starts_with(held),
                    "{view:?}"
                );
            } else {
                assert!(!view.provided.contains_key(ACCOUNTING));
                assert!(
                    view.missing[ACCOUNTING].contains(RESIDENCY_SWAP_ABSENT),
                    "{view:?}"
                );
            }
        }

        // An established proof re-decides every residency name.
        host.residency = Some(ResidencyProof::Locked {
            basis: "cap".to_owned(),
        });
        let mut view = host.capabilities();
        let weaker = ResidencyProof::SwapAbsent {
            basis: "no swap now".to_owned(),
        };
        host.apply_residency(&mut view, Ok(&weaker));
        assert!(!view.provided.contains_key(ACCOUNTING));
        assert!(!view.provided.contains_key(RESIDENCY_LOCKED));
        assert!(view.provided.contains_key(RESIDENCY_SWAP_ABSENT));

        host.residency = None;
        host.residency_refusal = Some("mlock refused and swap is active".to_owned());
        let view = host.capabilities();
        for name in RESIDENCY_CAPABILITIES {
            assert_eq!(
                view.missing[name], "mlock refused and swap is active",
                "{name}"
            );
        }
        host.residency = Some(ResidencyProof::Unknown(serde_json::Map::new()));
        let view = host.capabilities();
        for name in RESIDENCY_CAPABILITIES {
            assert!(view.missing[name].contains("does not know"), "{name}");
        }

        host.kernel_kind = "gvisor".to_owned();
        host.residency = Some(ResidencyProof::Locked {
            basis: "cap".to_owned(),
        });
        let view = host.capabilities();
        assert_eq!(
            view.provided[ACCOUNTING].source,
            ProofSource::ProviderAttested
        );
        assert!(
            satisfies(
                &CapabilityRequirement::from(ACCOUNTING),
                ACCOUNTING,
                &view.provided[ACCOUNTING]
            )
            .is_err()
        );
        for evidence in view.provided.values() {
            assert_eq!(evidence.provider, HOST_PROVIDER);
        }
    }

    #[test]
    fn floor_entries_parse_as_a_name_or_a_named_set() {
        let entries: Vec<CapabilityRequirement> = serde_json::from_value(serde_json::json!([
            {"any_of": [ACCOUNTING, RESIDENCY_SWAP_ABSENT]},
            "time"
        ]))
        .unwrap();
        assert_eq!(entries[1], CapabilityRequirement::from("time"));
        assert_eq!(entries[0].names(), [ACCOUNTING, RESIDENCY_SWAP_ABSENT]);
        assert_eq!(
            entries[0].label(),
            "any_of(accounting, residency:swap_absent)"
        );
        assert_eq!(entries[1].names(), ["time"]);
        assert_eq!(
            serde_json::to_value(&entries).unwrap(),
            serde_json::json!([{"any_of": ["accounting", "residency:swap_absent"]}, "time"])
        );
        assert!(
            serde_json::from_value::<CapabilityRequirement>(
                serde_json::json!({"any_of": ["accounting"], "prefer": "accounting"})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<CapabilityRequirement>(serde_json::json!(7)).is_err());

        // The object form carries accepted grades and providers and
        // round-trips without filling in what it did not say.
        let spec = serde_json::json!({
            "name": "termination:scope",
            "accept": ["measured", "provider_attested"],
            "providers": ["external_controller:oci"]
        });
        let entry: CapabilityRequirement = serde_json::from_value(spec.clone()).unwrap();
        assert!(!entry.is_set());
        assert_eq!(entry.names(), ["termination:scope"]);
        assert_eq!(entry.label(), "termination:scope");
        assert_eq!(entry.accepted_grades(), ["measured", "provider_attested"]);
        assert_eq!(entry.providers(), ["external_controller:oci"]);
        assert_eq!(entry.spec_refusal(), None);
        assert_eq!(serde_json::to_value(&entry).unwrap(), spec);
        let bare = CapabilityRequirement::from("time");
        assert!(bare.accepted_grades().is_empty() && bare.providers().is_empty());
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            serde_json::json!("time")
        );
    }

    fn spec(value: serde_json::Value) -> CapabilityRequirement {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_spec_names_one_thing_and_only_grades_this_node_can_judge() {
        for (entry, refusal) in [
            (
                spec(serde_json::json!({"name": "time", "any_of": ["accounting"]})),
                "both name and any_of",
            ),
            (
                spec(serde_json::json!({"accept": ["measured"]})),
                "neither name nor any_of",
            ),
            (spec(serde_json::json!({"name": ""})), "empty capability"),
            (
                spec(serde_json::json!({"any_of": ["accounting", "accounting"]})),
                "capability accounting twice",
            ),
            (
                spec(serde_json::json!({"name": "time", "accept": ["measured", "measured"]})),
                "grade measured twice",
            ),
            (
                spec(serde_json::json!({"name": "time", "accept": [""]})),
                "empty grade",
            ),
            (
                spec(serde_json::json!({"name": "time", "providers": ["host", "host"]})),
                "provider host twice",
            ),
            (
                spec(serde_json::json!({"name": "time", "accept": ["peer_attested"]})),
                "a peer's claim is not evidence",
            ),
            (
                spec(serde_json::json!({"name": "time", "accept": ["quorum_attested"]})),
                "grade quorum_attested, which this node cannot judge",
            ),
            (CapabilityRequirement::from(""), "empty capability"),
        ] {
            let reason = entry.spec_refusal().unwrap_or_default();
            assert!(reason.contains(refusal), "{entry:?}: {reason}");
            // The placement check reports the shape refusal first.
            assert_eq!(floor_entry_refusal(&entry), entry.spec_refusal());
        }
        for entry in [
            spec(serde_json::json!({"name": "time"})),
            spec(serde_json::json!({"name": "time", "accept": ["trusted"]})),
            spec(
                serde_json::json!({"any_of": [ACCOUNTING, RESIDENCY_SWAP_ABSENT], "providers": ["host"]}),
            ),
        ] {
            assert_eq!(entry.spec_refusal(), None, "{entry:?}");
        }
    }

    fn accepting(name: &str, grades: &[&str]) -> CapabilityRequirement {
        CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: Some(name.to_owned()),
            any_of: Vec::new(),
            accept: grades.iter().map(|grade| (*grade).to_owned()).collect(),
            providers: Vec::new(),
        })
    }

    fn evidence(source: ProofSource, provider: &str) -> CapabilityEvidence {
        CapabilityEvidence {
            source,
            provider: provider.to_owned(),
            basis: "fixture".to_owned(),
        }
    }

    #[test]
    fn an_entry_is_met_by_the_grades_and_providers_it_accepts() {
        let measured_by_service = evidence(ProofSource::Measured, "host_service");
        let bare = CapabilityRequirement::from("termination:scope");
        satisfies(&bare, "termination:scope", &measured_by_service).unwrap();
        // Any provider meets an entry that names none; a listed provider set
        // refuses every other, with the mismatch spelled out.
        let oci_only = CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: Some("termination:scope".to_owned()),
            any_of: Vec::new(),
            accept: Vec::new(),
            providers: vec!["external_controller:oci".to_owned()],
        });
        assert_eq!(
            satisfies(&oci_only, "termination:scope", &measured_by_service).unwrap_err(),
            "termination:scope is proven here by host_service (measured); the floor accepts only external_controller:oci"
        );
        satisfies(
            &oci_only,
            "termination:scope",
            &evidence(ProofSource::Measured, "external_controller:oci"),
        )
        .unwrap();
        // The grade is judged before the provider.
        let attested_by_oci = evidence(ProofSource::ProviderAttested, "external_controller:oci");
        assert_eq!(
            satisfies(&oci_only, "termination:scope", &attested_by_oci).unwrap_err(),
            "termination:scope is only provider_attested-proven here by external_controller:oci; the floor accepts measured"
        );
        // Accepted grades are matched as text, so listing one widens the
        // entry to exactly that grade.
        let widened = accepting("termination:scope", &["measured", "provider_attested"]);
        satisfies(&widened, "termination:scope", &attested_by_oci).unwrap();
        satisfies(&widened, "termination:scope", &measured_by_service).unwrap();
        let refusal = satisfies(
            &widened,
            "termination:scope",
            &evidence(ProofSource::Trusted, "host"),
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            "termination:scope is only trusted-proven here by host; the floor accepts measured, provider_attested"
        );
        // A grade this build does not know meets nothing, even when listed
        // in evidence under the same text as an accepted grade's neighbour.
        let unknown = evidence(ProofSource::Unknown("quorum_attested".to_owned()), "host");
        assert!(
            satisfies(&widened, "termination:scope", &unknown)
                .unwrap_err()
                .contains("quorum_attested-proven")
        );
        assert!(
            satisfies(
                &bare,
                "termination:scope",
                &evidence(ProofSource::PeerAttested, "peer")
            )
            .is_err()
        );
    }

    #[test]
    fn weaker_residency_names_appear_only_beside_accounting() {
        let any_of = |names: &[&str]| {
            CapabilityRequirement::Spec(CapabilityRequirementSpec {
                name: None,
                any_of: names.iter().map(|name| (*name).to_owned()).collect(),
                accept: Vec::new(),
                providers: Vec::new(),
            })
        };
        for entry in [
            CapabilityRequirement::from(ACCOUNTING),
            CapabilityRequirement::from(RESIDENCY_LOCKED),
            CapabilityRequirement::from("time"),
            CapabilityRequirement::from(STORAGE_DURABLE_ROOT),
            any_of(&[ACCOUNTING, RESIDENCY_SWAP_ABSENT]),
            any_of(&[ACCOUNTING, RESIDENCY_SWAP_LIMIT]),
            CapabilityRequirement::from(TERMINATION_ENCLOSURE_TRUSTED_INIT),
        ] {
            assert_eq!(floor_entry_refusal(&entry), None, "{}", entry.label());
        }
        for (entry, refusal) in [
            (
                CapabilityRequirement::from(RESIDENCY_SWAP_ABSENT),
                "does not meet",
            ),
            (
                CapabilityRequirement::from(RESIDENCY_SWAP_LIMIT),
                "does not meet",
            ),
            (
                any_of(&[RESIDENCY_LOCKED, RESIDENCY_SWAP_ABSENT]),
                "does not meet",
            ),
            (any_of(&[ACCOUNTING, "termination:scope"]), "per-scope"),
            (any_of(&[ACCOUNTING, SCOPE_LIMITS]), "per-scope"),
            (any_of(&[ACCOUNTING, STORAGE_DURABLE_ROOT]), "on its own"),
            (
                any_of(&["isolation:namespaces", STORAGE_DURABLE_ROOT]),
                "on its own",
            ),
            (
                any_of(&[ISOLATION_NAMESPACES, "time"]),
                "isolation:namespaces, which must be required on its own",
            ),
        ] {
            let reason = floor_entry_refusal(&entry).unwrap_or_default();
            assert!(reason.contains(refusal), "{}: {reason}", entry.label());
        }
        assert!(is_scope_capability(SCOPE_LIMITS));
        assert!(is_scope_capability("recovery:scope"));
        assert!(!is_scope_capability(ACCOUNTING));

        let residency = any_of(&[ACCOUNTING, RESIDENCY_SWAP_ABSENT]);
        assert!(met_by_strongest(&residency, ACCOUNTING));
        assert!(!met_by_strongest(&residency, RESIDENCY_SWAP_ABSENT));
    }

    #[test]
    fn trusted_enclosure_inits_are_file_names() {
        assert!(admit_enclosure_inits(&["with/slash".to_owned()]).is_err());
        admit_enclosure_inits(&inits()).unwrap();
        assert_eq!(enclosure_inits(), inits());
        admit_enclosure_inits(&[]).unwrap();
        assert!(enclosure_inits().is_empty());
    }

    #[test]
    fn unknown_report_enums_round_trip_exactly() {
        let source: ProofSource = serde_json::from_str("\"quorum_attested\"").unwrap();
        assert_eq!(source, ProofSource::Unknown("quorum_attested".to_owned()));
        let foreign = CapabilityEvidence {
            source: source.clone(),
            provider: HOST_PROVIDER.to_owned(),
            basis: String::new(),
        };
        assert!(satisfies(&CapabilityRequirement::from("time"), "time", &foreign).is_err());
        let round_trip: CapabilityEvidence =
            serde_json::from_value(serde_json::to_value(&foreign).unwrap()).unwrap();
        assert_eq!(round_trip, foreign);
        assert!(
            serde_json::from_value::<CapabilityEvidence>(
                serde_json::json!({"source": "measured", "basis": "no provider"})
            )
            .is_err()
        );
        assert_eq!(
            serde_json::to_string(&source).unwrap(),
            "\"quorum_attested\""
        );
        assert_eq!(
            serde_json::to_string(&ProofSource::Trusted).unwrap(),
            "\"trusted\""
        );

        let original = serde_json::json!({"method": "pinned_hugepages", "basis": "b", "pages": 4});
        let proof: ResidencyProof = serde_json::from_value(original.clone()).unwrap();
        assert!(matches!(proof, ResidencyProof::Unknown(_)));
        assert_eq!(serde_json::to_value(&proof).unwrap(), original);
        assert!(proof.confirm_swap_unreachable().is_err());

        for (method, expected) in [
            (
                "locked",
                ResidencyProof::Locked {
                    basis: "cap".to_owned(),
                },
            ),
            (
                "swap_limit",
                ResidencyProof::SwapLimit {
                    basis: "cap".to_owned(),
                },
            ),
            (
                "swap_absent",
                ResidencyProof::SwapAbsent {
                    basis: "cap".to_owned(),
                },
            ),
        ] {
            let known = serde_json::json!({"method": method, "basis": "cap"});
            let proof: ResidencyProof = serde_json::from_value(known.clone()).unwrap();
            assert_eq!(proof, expected);
            assert_eq!(proof.method(), method);
            assert_eq!(serde_json::to_value(&proof).unwrap(), known);
        }

        // A known method keeps its strict shape.
        assert!(
            serde_json::from_value::<ResidencyProof>(
                serde_json::json!({"method": "locked", "basis": "cap", "extra": 1})
            )
            .is_err()
        );
    }
}
