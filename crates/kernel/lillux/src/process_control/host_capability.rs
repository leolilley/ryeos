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

pub const HOST_FACTS_VERSION: u32 = 2;

/// How the node's resident native region is kept in physical memory.
///
/// A lock holds for the mapping's life. A swap-unreachable proof holds only
/// while its basis holds: `memory.swap.max` of 0 on an enclosing cgroup the
/// node cannot write is a standing kernel limit and is preferred; an empty
/// swap list is a point-in-time observation that a later `swapon` can undo,
/// so its holder re-checks it before admitting new native work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResidencyProof {
    /// The pages are locked with mlock.
    Locked { basis: String },
    /// The pages are touched once while no swap is reachable for this
    /// process, so the kernel has nowhere to page anonymous memory out to.
    SwapUnreachable { basis: String },
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
    SwapUnreachable { basis: String },
}

impl Serialize for ResidencyProof {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Locked { basis } => {
                KnownResidencyProof::Locked { basis: basis.clone() }.serialize(serializer)
            }
            Self::SwapUnreachable { basis } => {
                KnownResidencyProof::SwapUnreachable { basis: basis.clone() }.serialize(serializer)
            }
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
        if !matches!(method, "locked" | "swap_unreachable") {
            return Ok(Self::Unknown(original));
        }
        match serde_json::from_value(serde_json::Value::Object(original)).map_err(D::Error::custom)? {
            KnownResidencyProof::Locked { basis } => Ok(Self::Locked { basis }),
            KnownResidencyProof::SwapUnreachable { basis } => Ok(Self::SwapUnreachable { basis }),
        }
    }
}

impl ResidencyProof {
    /// Choose the proof this process can establish for `bytes` of resident
    /// memory, or explain in plain words why neither is available. Locking is
    /// chosen whenever the effective capability or the remaining
    /// RLIMIT_MEMLOCK allows it.
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

    /// The swap-unreachable proof alone, for a holder whose lock attempt the
    /// kernel refused although the limits looked sufficient.
    pub fn plan_without_lock(bytes: u64) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            let _ = bytes;
            linux::swap_unreachable_basis().map(|basis| Self::SwapUnreachable { basis })
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
            Self::SwapUnreachable { basis: _ } => "swap_unreachable",
            Self::Unknown(_) => "unknown",
        }
    }

    pub fn basis(&self) -> &str {
        match self {
            Self::Locked { basis } | Self::SwapUnreachable { basis } => basis,
            Self::Unknown(_) => "",
        }
    }

    /// Re-check that a swap-unreachable proof still holds: immediately
    /// before the pages are touched, and again before new native work is
    /// admitted against them. A locked proof is held by the lock itself.
    pub fn confirm_swap_unreachable(&self) -> Result<(), String> {
        match self {
            Self::Locked { basis: _ } => Ok(()),
            Self::Unknown(_) => Err("an unknown residency method cannot be confirmed".to_owned()),
            #[cfg(target_os = "linux")]
            Self::SwapUnreachable { basis: _ } => linux::swap_unreachable_basis().map(|_| ()),
            #[cfg(not(target_os = "linux"))]
            Self::SwapUnreachable { basis: _ } => {
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

/// Capability names whose only proof is policy trust: the grade is part of
/// the name, so a requirement that names one explicitly accepts trusted
/// evidence for it. Every other requirement needs a local measurement.
pub const TRUSTED_GRADE_CAPABILITIES: &[&str] = &["termination:process_group"];

/// Whether `evidence` satisfies a local requirement for `name`. Provider
/// and peer attestations never satisfy a local requirement.
pub fn satisfies_local_requirement(name: &str, evidence: &CapabilityEvidence) -> bool {
    match evidence.source {
        ProofSource::Measured => true,
        ProofSource::Trusted => TRUSTED_GRADE_CAPABILITIES.contains(&name),
        ProofSource::ProviderAttested | ProofSource::PeerAttested | ProofSource::Unknown(_) => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityEvidence {
    pub source: ProofSource,
    pub basis: String,
}

/// Jobs die with the node because the node is pid 1 of its pid namespace.
pub const TERMINATION_ENCLOSURE: &str = "termination:enclosure";
/// Jobs die with the node because the node is pid 1, or because its parent
/// is a pid 1 that node policy trusts to exit when its only child exits.
pub const TERMINATION_ENCLOSURE_TRUSTED_INIT: &str = "termination:enclosure_trusted_init";
/// A new container enclosure on the same boot proves the old one's jobs
/// dead, with the termination grade of `TERMINATION_ENCLOSURE`.
pub const RECOVERY_ENCLOSURE: &str = "recovery:enclosure";
/// As `RECOVERY_ENCLOSURE`, with the grade of
/// `TERMINATION_ENCLOSURE_TRUSTED_INIT`.
pub const RECOVERY_ENCLOSURE_TRUSTED_INIT: &str = "recovery:enclosure_trusted_init";

static ENCLOSURE_INITS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Admit the executable names of inits that the node's signed policy trusts
/// to exit when their only child exits, replacing any earlier list in this
/// process. Until this is called no init is trusted, so only a node that is
/// itself pid 1 can prove its exit ends its enclosure.
pub fn admit_enclosure_inits(names: &[String]) -> Result<(), String> {
    for name in names {
        if name.is_empty() || name.len() > 255 || name.contains('/') || name.contains('\0') {
            return Err(format!("trusted enclosure init {name:?} is not an executable file name"));
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
    ENCLOSURE_INITS.read().map(|admitted| admitted.clone()).unwrap_or_default()
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
        return Ok(CapabilityEvidence {
            source: ProofSource::Measured,
            basis: "the node is pid 1 of its private pid namespace, so the namespace ends when \
                    the node exits"
                .to_owned(),
        });
    }
    if parent_pid != 1 {
        return Err(format!(
            "the node is pid {node_pid} with parent pid {parent_pid} in its pid namespace; \
             nothing proves the namespace ends when the node exits"
        ));
    }
    match pid1 {
        Some(init) if trusted_inits.iter().any(|trusted| trusted == init) => Ok(CapabilityEvidence {
            source: ProofSource::Trusted,
            basis: format!(
                "the node's parent is pid 1 ({init}), which node policy trusts to exit when its \
                 only child exits"
            ),
        }),
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
    /// Whether the host permits this node to become a child subreaper,
    /// probed in a disposable child. The node does not set it: no reaper of
    /// adopted orphans exists, so this records only what the host allows.
    pub subreaper: ProbeOutcome,
    pub resident_bytes: u64,
    pub residency: Option<ResidencyProof>,
    pub residency_refusal: Option<String>,
}

/// Host-level capability view derived from facts. Scope capabilities come
/// from an opened process-scope provider and are added by its owner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilities {
    pub provided: BTreeMap<String, CapabilityEvidence>,
    pub missing: BTreeMap<String, String>,
}

impl HostCapabilities {
    pub fn provide(&mut self, name: &str, source: ProofSource, basis: impl Into<String>) {
        self.missing.remove(name);
        self.provided.insert(name.to_owned(), CapabilityEvidence { source, basis: basis.into() });
    }

    pub fn refuse(&mut self, name: &str, reason: impl Into<String>) {
        if !self.provided.contains_key(name) {
            self.missing.insert(name.to_owned(), reason.into());
        }
    }
}

impl HostFacts {
    /// Observe this host for a node whose resident region is
    /// `resident_bytes`. Never fails: an unreadable fact is recorded as such.
    pub fn observe(resident_bytes: u64) -> Self {
        #[cfg(target_os = "linux")]
        {
            linux::observe(resident_bytes)
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
                enclosure_exit_refusal: Some("host observation is unavailable on this OS".to_owned()),
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
                memlock: MemlockFacts { soft: None, hard: None, locked_bytes: None, cap_ipc_lock: false },
                unshare_user: unobservable(),
                unshare_user_pid_mount: unobservable(),
                pidfd: unobservable(),
                subreaper: unobservable(),
                resident_bytes,
                residency: None,
                residency_refusal: Some("resident memory proof is unavailable on this OS".to_owned()),
            }
        }
    }

    pub fn gvisor(&self) -> bool {
        self.kernel_kind == "gvisor"
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
        let observed = if self.gvisor() { ProofSource::ProviderAttested } else { ProofSource::Measured };
        let emulated = |basis: String| {
            if self.gvisor() { format!("{basis} (observed under gVisor emulation)") } else { basis }
        };
        let mut view = HostCapabilities::default();
        view.provide("time", ProofSource::Measured, "the node enforces each deadline and then kills");
        match (&self.residency, &self.residency_refusal) {
            (Some(proof), _) if !matches!(proof, ResidencyProof::Unknown(_)) => view.provide(
                "accounting",
                observed.clone(),
                emulated(format!("resident region {}: {}", proof.method(), proof.basis())),
            ),
            (_, refusal) => view.refuse(
                "accounting",
                refusal.clone().unwrap_or_else(|| "resident memory is not proven".to_owned()),
            ),
        }
        if self.pidfd.allowed() {
            view.provide(
                "termination:process_group",
                ProofSource::Trusted,
                "process group plus pidfd; a job that calls setsid or double-forks escapes it",
            );
        } else {
            view.refuse("termination:process_group", format!("pidfd_open {}", self.pidfd.describe()));
        }
        // The grade each enclosure name may carry, or why it cannot.
        let unproven = || {
            self.enclosure_exit_refusal
                .clone()
                .unwrap_or_else(|| "nothing proves the node's exit ends its pid namespace".to_owned())
        };
        let (measured_exit, trusted_exit): (Result<(ProofSource, String), String>, Result<(ProofSource, String), String>) =
            match &self.enclosure_exit {
                Some(evidence) if evidence.source == ProofSource::Measured => (
                    Ok((observed.clone(), emulated(evidence.basis.clone()))),
                    Ok((observed.clone(), emulated(evidence.basis.clone()))),
                ),
                Some(evidence) if evidence.source == ProofSource::Trusted => (
                    Err(format!(
                        "the node is not pid 1 of its pid namespace; its enclosure ends with it only \
                         by trust in pid 1 ({})",
                        evidence.basis
                    )),
                    Ok((ProofSource::Trusted, evidence.basis.clone())),
                ),
                _ => (Err(unproven()), Err(unproven())),
            };
        for (name, exit) in [
            (TERMINATION_ENCLOSURE, &measured_exit),
            (TERMINATION_ENCLOSURE_TRUSTED_INIT, &trusted_exit),
        ] {
            match exit {
                Ok((source, basis)) => view.provide(
                    name,
                    source.clone(),
                    format!("{basis}; when the pid namespace ends the kernel kills every job"),
                ),
                Err(reason) => view.refuse(name, reason.clone()),
            }
        }
        match &self.cgroup.memory_max {
            Some(limit) if !limit.writable_by_node => view.provide(
                "limits:enclosure",
                observed.clone(),
                emulated(format!("memory.max {} on cgroup {}, not writable by the node", limit.value, limit.cgroup)),
            ),
            Some(limit) => view.refuse(
                "limits:enclosure",
                format!("memory.max {} on cgroup {} is writable by the node itself", limit.value, limit.cgroup),
            ),
            None => view.refuse(
                "limits:enclosure",
                format!(
                    "no enclosing cgroup sets a finite memory.max (cgroup filesystem: {})",
                    self.cgroup.filesystem
                ),
            ),
        }
        for (name, exit) in [
            (RECOVERY_ENCLOSURE, &measured_exit),
            (RECOVERY_ENCLOSURE_TRUSTED_INIT, &trusted_exit),
        ] {
            match (self.container_enclosure, exit) {
                (Some(inode), Ok((source, basis))) => view.provide(
                    name,
                    source.clone(),
                    format!(
                        "boot {} and container enclosure cgroup inode {inode}; {basis}, so a \
                         different inode on this boot is a new generation whose jobs all died \
                         with the old one",
                        self.boot_id
                    ),
                ),
                (_, Err(reason)) => view.refuse(
                    name,
                    format!(
                        "jobs are not proven to die with the node's enclosure ({reason}), so the \
                         generation is undecided beyond the boot"
                    ),
                ),
                (None, Ok(_)) => view.refuse(
                    name,
                    format!(
                        "the node is not the whole of a container enclosure (cgroup namespace {}, \
                         membership {}, /sys/fs/cgroup is {}), so a restart cannot be told apart \
                         from the same generation",
                        if self.initial_cgroup_namespace { "initial" } else { "private" },
                        self.cgroup.membership.as_deref().unwrap_or("unreadable"),
                        self.cgroup.filesystem
                    ),
                ),
            }
        }
        if self.unshare_user_pid_mount.allowed() {
            view.provide(
                "isolation:namespaces",
                observed,
                emulated("unshare of user, pid and mount namespaces is allowed".to_owned()),
            );
        } else {
            view.refuse(
                "isolation:namespaces",
                format!("unshare of user, pid and mount namespaces {}", self.unshare_user_pid_mount.describe()),
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
        if boot_id.len() != 36 || !boot_id.bytes().all(|byte| byte == b'-' || byte.is_ascii_hexdigit()) {
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
/// only when the node's exit is proven to end it (the node is pid 1, or its
/// parent is a trusted init). Otherwise `None`, so the witness decides by
/// boot alone and a same-boot restart is never taken as proof of death.
#[cfg(target_os = "linux")]
pub(super) fn read_witnessed_enclosure() -> Result<Option<u64>, String> {
    let (node_pid, parent_pid, pid1) = linux::pid_facts();
    let private = linux::private_pid_namespace();
    match decide_enclosure_exit(private, node_pid, parent_pid, pid1.as_deref(), &enclosure_inits()) {
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
        status
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':').map(|value| value.trim().to_owned()))
    }

    fn cap_ipc_lock(status: &str) -> bool {
        status_field(status, "CapEff")
            .and_then(|value| u64::from_str_radix(&value, 16).ok())
            .is_some_and(|mask| mask & (1 << CAP_IPC_LOCK) != 0)
    }

    fn locked_bytes(status: &str) -> Option<u64> {
        let value = status_field(status, "VmLck")?;
        value.strip_suffix("kB")?.trim().parse::<u64>().ok()?.checked_mul(1024)
    }

    fn memlock_limit() -> Option<(u64, u64)> {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) } != 0 {
            return None;
        }
        Some((limit.rlim_cur as u64, limit.rlim_max as u64))
    }

    fn display_limit(value: u64) -> String {
        if value == libc::RLIM_INFINITY as u64 { "unlimited".to_owned() } else { format!("{value} bytes") }
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
                if soft == libc::RLIM_INFINITY as u64 || soft.saturating_sub(locked) >= bytes {
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
            _ => lock_notes.push("RLIMIT_MEMLOCK or the locked-memory count is unreadable".to_owned()),
        }
        swap_unreachable_basis()
            .map(|basis| ResidencyProof::SwapUnreachable { basis })
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

    /// Plain-language basis for "no swap is reachable for this process", or
    /// the reason it cannot be proven.
    /// The standing cgroup limit is preferred; an empty swap list is only
    /// what is true now.
    pub(super) fn swap_unreachable_basis() -> Result<String, String> {
        if let Some(cgroup) = zero_swap_cgroup() {
            return Ok(format!(
                "memory.swap.max is 0 on enclosing cgroup {cgroup}, which the node can neither \
                 write nor move itself out of"
            ));
        }
        let devices = swap_devices();
        if let Ok(devices) = &devices
            && devices.is_empty()
        {
            return Ok(
                "no swap device is active now (/proc/swaps lists none); a point-in-time \
                 observation, re-checked before new native work is admitted"
                    .to_owned(),
            );
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
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(root.as_raw_fd(), &mut stat) } != 0 {
            return Err(format!("statfs {CGROUP_ROOT}: {}", std::io::Error::last_os_error()));
        }
        Ok((stat.f_type == super::super::cgroup::CGROUP2_SUPER_MAGIC).then_some(root))
    }

    fn filesystem_name() -> String {
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        let Ok(path) = CString::new(CGROUP_ROOT) else { return "absent".to_owned() };
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
        std::fs::metadata(format!("/proc/self/ns/{name}")).ok().map(|metadata| metadata.ino())
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
            std::fs::metadata("/proc/1/ns/pid").ok().map(|metadata| metadata.ino()),
            namespace_inode("pid"),
        ) {
            (Some(pid1_namespace), Some(own)) => pid1_namespace != own,
            _ => false,
        };
        let pid1 = if foreign_proc { None } else { pid1_executable().or_else(pid1_comm) };
        (node_pid, parent_pid, pid1)
    }

    fn pid1_executable() -> Option<String> {
        let target = std::fs::read_link("/proc/1/exe").ok()?;
        target.file_name()?.to_str().map(str::to_owned)
    }

    fn pid1_comm() -> Option<String> {
        read_small("/proc/1/comm").ok().map(|comm| comm.trim().to_owned())
    }

    /// The enclosure inode only when the node is the whole of a container:
    /// a private cgroup namespace whose root is this process's own cgroup,
    /// and a private pid namespace whose init exit kills every member.
    pub(super) fn container_enclosure() -> Result<Option<u64>, String> {
        let private_cgroup = namespace_inode("cgroup")
            .is_some_and(|inode| inode != INITIAL_CGROUP_NAMESPACE_INODE);
        let private_pid = namespace_inode("pid")
            .is_some_and(|inode| inode != INITIAL_PID_NAMESPACE_INODE);
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
                root.metadata().map_err(|error| format!("stat {CGROUP_ROOT}: {error}"))?.ino(),
            )),
            None => Ok(None),
        }
    }

    fn membership() -> Result<PathBuf, String> {
        let file = File::open("/proc/self/cgroup")
            .map_err(|error| format!("open current process cgroup membership: {error}"))?;
        super::super::cgroup::unified_cgroup_from_file(&file, super::super::cgroup::CURRENT_PROCESS_SUBJECT)
    }

    /// This process's cgroup and each ancestor up to the visible root, as
    /// (relative name, directory) pairs, nearest first.
    fn enclosing_cgroups() -> Vec<(String, PathBuf)> {
        let Ok(Some(_)) = cgroup2_root() else { return Vec::new() };
        let Ok(relative) = membership() else { return Vec::new() };
        relative
            .ancestors()
            .filter_map(|ancestor| {
                let inside = ancestor.strip_prefix("/").ok()?;
                Some((ancestor.display().to_string(), Path::new(CGROUP_ROOT).join(inside)))
            })
            .collect()
    }

    /// Whether this process may write `path`, judged with its effective
    /// identity, which is the one the kernel checks when it writes.
    fn writable(path: &Path) -> bool {
        let Ok(path) = CString::new(path.as_os_str().as_encoded_bytes()) else { return false };
        unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::W_OK, libc::AT_EACCESS) == 0 }
    }

    fn nearest_finite(name: &str) -> Option<CgroupLimit> {
        enclosing_cgroups().into_iter().find_map(|(cgroup, directory)| {
            let file = directory.join(name);
            let value = std::fs::read_to_string(&file).ok()?.trim().to_owned();
            (value != "max").then(|| CgroupLimit { value, cgroup, writable_by_node: writable(&file) })
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
        let highest_movable =
            enclosing.iter().rposition(|(_, directory)| writable(&directory.join("cgroup.procs")));
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
        enclosing.iter().enumerate().find_map(|(index, (cgroup, directory))| {
            if highest_movable.is_some_and(|highest| index < highest) {
                return None;
            }
            proves(directory)?.then(|| cgroup.clone())
        })
    }

    fn cgroup_mount() -> (Option<bool>, Option<bool>) {
        let Ok(raw) = read_small("/proc/self/mountinfo") else { return (None, None) };
        for line in raw.lines() {
            let Some((left, right)) = line.split_once(" - ") else { continue };
            let fields: Vec<&str> = left.split_whitespace().collect();
            if fields.get(4) != Some(&CGROUP_ROOT) {
                continue;
            }
            let read_only = fields.get(5).map(|options| options.split(',').any(|option| option == "ro"));
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
            return unobservable(format!("fork for probe: {}", std::io::Error::last_os_error()));
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
                std::io::Error::last_os_error().raw_os_error().unwrap_or(255).clamp(1, 255)
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
            return ProbeOutcome::Unobservable { reason: "probe child did not exit normally".to_owned() };
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
            return ProbeOutcome::Refused { errno: error.raw_os_error().unwrap_or(0), reason: error.to_string() };
        }
        unsafe { libc::close(fd as libc::c_int) };
        ProbeOutcome::Allowed
    }

    pub(super) fn observe(resident_bytes: u64) -> HostFacts {
        let status = read_small("/proc/self/status").unwrap_or_default();
        let version = read_small("/proc/version").unwrap_or_default();
        let kernel_kind = if version.to_ascii_lowercase().contains("gvisor") { "gvisor" } else { "native" };
        let pid_namespace = std::fs::metadata("/proc/self/ns/pid").ok().map(|metadata| metadata.ino());
        let (soft, hard) = memlock_limit().map_or((None, None), |(soft, hard)| (Some(soft), Some(hard)));
        let membership_path = membership().ok();
        let membership_writable = membership_path
            .as_ref()
            .and_then(|relative| relative.strip_prefix("/").ok().map(|inside| Path::new(CGROUP_ROOT).join(inside)))
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
            kernel_release: version.split_whitespace().nth(2).unwrap_or_default().to_owned(),
            kernel_kind: kernel_kind.to_owned(),
            boot_id: super::super::linux::read_boot_id().unwrap_or_default(),
            pid_namespace,
            initial_pid_namespace: pid_namespace == Some(INITIAL_PID_NAMESPACE_INODE),
            cgroup_namespace: namespace_inode("cgroup"),
            initial_cgroup_namespace: namespace_inode("cgroup") == Some(INITIAL_CGROUP_NAMESPACE_INODE),
            container_enclosure: container_enclosure().ok().flatten(),
            node_pid,
            parent_pid,
            pid1: pid1_comm(),
            pid1_executable: pid1_executable(),
            enclosure_exit_refusal: enclosure_exit.as_ref().err().cloned(),
            enclosure_exit: enclosure_exit.ok(),
            uid_map: read_small("/proc/self/uid_map").ok().map(|map| {
                map.lines().map(|line| line.split_whitespace().collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("; ")
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
            subreaper: probe_in_child(ChildProbe::Subreaper),
            resident_bytes,
            residency_refusal: residency.as_ref().err().cloned(),
            residency: residency.ok(),
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
                    assert!(reason.contains("retains fork-sensitive descriptor authority"), "{reason}");
                }
                other => panic!("probe forked while this thread held a descriptor lease: {other:?}"),
            }
            drop(lease);
            let hold = std::time::Duration::from_millis(300);
            let holder = lease_held_elsewhere_for(hold);
            let started = std::time::Instant::now();
            let outcome = super::probe_in_child(super::ChildProbe::Subreaper);
            let waited = started.elapsed();
            holder.join().unwrap();
            assert_eq!(outcome, super::ProbeOutcome::Allowed);
            assert!(waited >= hold - std::time::Duration::from_millis(50), "{waited:?}");
            // The window reopened: a descriptor lease is granted at once.
            drop(crate::exec::try_retain_fork_sensitive_descriptors().unwrap());
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
                move |directory: &std::path::Path| Some(zero.iter().any(|name| std::path::Path::new(name) == directory))
            };
            // Nothing movable: the nearest zero limit proves.
            assert_eq!(super::zero_swap_at_or_above(&enclosing, None, zero_at(&["/a/b"])).as_deref(), Some("/a/b"));
            // The node can write /a's cgroup.procs, so a limit on /a/b can be left.
            assert_eq!(super::zero_swap_at_or_above(&enclosing, Some(2), zero_at(&["/a/b"])), None);
            assert_eq!(super::zero_swap_at_or_above(&enclosing, Some(2), zero_at(&["/a/b", "/a"])).as_deref(), Some("/a"));
            assert_eq!(super::zero_swap_at_or_above(&enclosing, Some(2), zero_at(&["/"])).as_deref(), Some("/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(enclosure_exit: Result<CapabilityEvidence, String>, container_enclosure: Option<u64>) -> HostFacts {
        let unobservable = || ProbeOutcome::Unobservable { reason: "fixture".to_owned() };
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
            memlock: MemlockFacts { soft: None, hard: None, locked_bytes: None, cap_ipc_lock: false },
            unshare_user: unobservable(),
            unshare_user_pid_mount: unobservable(),
            pidfd: unobservable(),
            subreaper: unobservable(),
            resident_bytes: 0,
            residency: None,
            residency_refusal: Some("fixture".to_owned()),
        }
    }

    fn inits() -> Vec<String> {
        ["catatonit", "dumb-init", "tini"].iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn enclosure_exit_is_measured_only_for_pid_one() {
        let measured = decide_enclosure_exit(Some(true), 1, 0, Some("ryeosd"), &[]).unwrap();
        assert_eq!(measured.source, ProofSource::Measured);
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

    #[test]
    fn enclosure_capabilities_follow_the_exit_grade() {
        let pid_one = facts(decide_enclosure_exit(Some(true), 1, 0, Some("ryeosd"), &[]), Some(42));
        let view = pid_one.capabilities();
        for name in [
            TERMINATION_ENCLOSURE,
            TERMINATION_ENCLOSURE_TRUSTED_INIT,
            RECOVERY_ENCLOSURE,
            RECOVERY_ENCLOSURE_TRUSTED_INIT,
        ] {
            assert_eq!(view.provided[name].source, ProofSource::Measured, "{name}");
            assert!(satisfies_local_requirement(name, &view.provided[name]), "{name}");
        }

        let under_tini = facts(decide_enclosure_exit(Some(true), 7, 1, Some("tini"), &inits()), Some(42));
        let view = under_tini.capabilities();
        assert!(view.missing[TERMINATION_ENCLOSURE].contains("not pid 1"), "{view:?}");
        assert!(view.missing[RECOVERY_ENCLOSURE].contains("undecided"), "{view:?}");
        assert_eq!(view.provided[TERMINATION_ENCLOSURE_TRUSTED_INIT].source, ProofSource::Trusted);
        assert_eq!(view.provided[RECOVERY_ENCLOSURE_TRUSTED_INIT].source, ProofSource::Trusted);
        assert!(view.provided[RECOVERY_ENCLOSURE_TRUSTED_INIT].basis.contains("inode 42"));

        let other = facts(decide_enclosure_exit(Some(true), 7, 1, Some("sh"), &inits()), Some(42));
        let view = other.capabilities();
        for name in [
            TERMINATION_ENCLOSURE,
            TERMINATION_ENCLOSURE_TRUSTED_INIT,
            RECOVERY_ENCLOSURE,
            RECOVERY_ENCLOSURE_TRUSTED_INIT,
        ] {
            assert!(!view.provided.contains_key(name), "{name}");
        }
        assert!(view.missing[RECOVERY_ENCLOSURE_TRUSTED_INIT].contains("undecided"));

        let not_whole = facts(decide_enclosure_exit(Some(true), 1, 0, Some("ryeosd"), &[]), None);
        let view = not_whole.capabilities();
        assert!(view.provided.contains_key(TERMINATION_ENCLOSURE));
        assert!(view.missing[RECOVERY_ENCLOSURE].contains("not the whole of a container"));
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
        assert!(!satisfies_local_requirement("time", &CapabilityEvidence { source: source.clone(), basis: String::new() }));
        assert_eq!(serde_json::to_string(&source).unwrap(), "\"quorum_attested\"");
        assert_eq!(serde_json::to_string(&ProofSource::Trusted).unwrap(), "\"trusted\"");

        let original = serde_json::json!({"method": "pinned_hugepages", "basis": "b", "pages": 4});
        let proof: ResidencyProof = serde_json::from_value(original.clone()).unwrap();
        assert!(matches!(proof, ResidencyProof::Unknown(_)));
        assert_eq!(serde_json::to_value(&proof).unwrap(), original);
        assert!(proof.confirm_swap_unreachable().is_err());

        let locked = serde_json::json!({"method": "locked", "basis": "cap"});
        let proof: ResidencyProof = serde_json::from_value(locked.clone()).unwrap();
        assert_eq!(proof, ResidencyProof::Locked { basis: "cap".to_owned() });
        assert_eq!(serde_json::to_value(&proof).unwrap(), locked);
        // A known method keeps its strict shape.
        assert!(serde_json::from_value::<ResidencyProof>(
            serde_json::json!({"method": "locked", "basis": "cap", "extra": 1})
        )
        .is_err());
    }
}
