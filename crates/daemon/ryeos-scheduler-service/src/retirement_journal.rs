use std::{collections::BTreeMap, ffi::OsStr};

use anyhow::{ensure, Context, Result};
use ryeos_app::execution_history_reset::{SchedulerResetPreview, SchedulerResetRetirement};
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_scheduler_contract::{
    decode_scheduler_retirement_record, encode_scheduler_retirement_record,
    SchedulerRetirementEffect, SchedulerRetirementFailureDisposition,
    SchedulerRetirementFailureFact, SchedulerRetirementFailureStage, SchedulerRetirementIntent,
    SchedulerRetirementNonRegularKind, SchedulerRetirementObservedCoordinate,
    SchedulerRetirementObservedQuarantine, SchedulerRetirementObservedStoppedCoordinate,
    SchedulerRetirementRecord, SchedulerRetirementRecoveryBinding,
    SchedulerRetirementRecoveryResolutionRef, SchedulerRetirementResolutionAction,
    SchedulerRetirementResolutionAttemptFact, SchedulerRetirementResolutionIntent,
    SchedulerRetirementResolutionObservation, SchedulerRetirementResolutionOutcome,
    SchedulerRetirementResolutionState, SchedulerRetirementRole, SchedulerRetirementRoleIdentity,
    SchedulerRetirementStoppedPlan, SchedulerRetirementStoppedRolePlan,
    SchedulerRetirementStoppedSource,
};

const ORIGINAL_OPERATION_PREFIX: &str = "execution-history-reset.operation.";
const INTENT_PREFIX: &str = "scheduler-retirement.intent.";
const PHASE_PREFIX: &str = "scheduler-retirement.phase.";
const RESOLUTION_PREFIX: &str = "scheduler-retirement.resolution.";
const RESOLUTION_QUARANTINE_PREFIX: &str = "scheduler-retirement.resolve.";
const RESERVED_PREFIX: &str = "scheduler-retirement.";
/// Names one acknowledged operation whose completed generations' journals
/// are being removed; see [`RetiredOperationEvidence`].
const RETIRED_PREFIX: &str = "scheduler-retirement.retired.";
const RETIRED_PURPOSE: &str = "scheduler-retirement-evidence-retired";
/// Names the fire-history retirement one reset operation began; see
/// [`FireHistoryRetirementIntent`].
const FIRES_PREFIX: &str = "scheduler-retirement.fires.";
const FIRES_PURPOSE: &str = "scheduler-fire-history-retirement";
/// The runtime child holding one directory of fire journals per schedule.
const FIRES_ROOT: &str = "schedules";
/// The only entries a schedule's fire directory may hold.
const FIRE_JOURNAL: &str = "fires.jsonl";
const FIRE_JOURNAL_LOCK: &str = ".fires.jsonl.lock";
/// The phase sequence of a generation's terminal `Complete` record.
const COMPLETE_SEQUENCE: u8 = 27;
const MAX_GENERATIONS: usize = ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_GENERATIONS;
const MAX_RECORDS_PER_GENERATION: usize =
    ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PHASE_RECORDS_PER_GENERATION;
const MAX_DIRECTORY_ENTRIES: usize =
    ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_DIRECTORY_ENTRIES;

/// Test-only process-kill boundaries at the armed stage transitions of one
/// role. The physical move or unlink itself is a single native call; these
/// cuts sit at the signed record immediately on either side of it.
#[cfg(feature = "crash-qualification-test-support")]
#[derive(Clone, Copy)]
enum ResetCrashCut {
    /// `QuarantineAuthorized` is durable; the role is still at its canonical
    /// coordinate (armed P to Q, not moved).
    QuarantineArmed,
    /// The native quarantine move succeeded; its `Quarantined` acknowledgement
    /// is not yet durable (the role survives only at Q).
    QuarantinedBeforeAck,
    /// `PurgeAuthorized` is durable; the role is still at its quarantine
    /// coordinate (armed Q to U, not moved).
    PurgeArmed,
    /// The native purge unlinked the role; its `Purged` acknowledgement is
    /// not yet durable.
    UnlinkedBeforeAck,
}

/// Native-owner cuts inside the planned removal, between its move and its
/// unlink. They are selected by the same environment value as [`ResetCrashCut`]
/// and run inside the native call, so they are dispatched through the Lillux
/// planned-removal hook.
#[cfg(feature = "crash-qualification-test-support")]
const MOVED_TO_PURGE_BEFORE_UNLINK: &str = "moved_to_purge_before_unlink";
/// Selecting this cut makes the ordinary purge's unlink fail after its move so
/// the resolution path runs, and parks that resolution after its own move from
/// the purge coordinate into the resolution quarantine, before its unlink.
#[cfg(feature = "crash-qualification-test-support")]
const MOVED_TO_RESOLUTION_BEFORE_UNLINK: &str = "moved_to_resolution_before_unlink";

/// Register the native planned-removal hook once per process.
#[cfg(feature = "crash-qualification-test-support")]
pub(crate) fn install_native_removal_cut() {
    lillux::secure_fs::install_planned_removal_hook(native_removal_cut);
}

#[cfg(feature = "crash-qualification-test-support")]
fn native_removal_cut(
    _source: &OsStr,
    sink: &OsStr,
) -> lillux::secure_fs::PlannedRemovalCut {
    use lillux::secure_fs::PlannedRemovalCut;
    let Some(sink) = sink.to_str() else {
        return PlannedRemovalCut::Proceed;
    };
    let parts: Vec<&str> = sink.split('.').collect();
    let (stage, role) = match parts.as_slice() {
        ["scheduler-retirement", "role", _, role, "purge"] => ("purge", *role),
        ["scheduler-retirement", "resolve", _, role, _, "quarantine"] => ("resolution", *role),
        _ => return PlannedRemovalCut::Proceed,
    };
    let Some(selected) = std::env::var_os(ryeos_app::env_contract::SCHEDULER_RESET_TEST_CUT_ENV)
    else {
        return PlannedRemovalCut::Proceed;
    };
    let selected = selected.to_string_lossy();
    let purge = format!("{MOVED_TO_PURGE_BEFORE_UNLINK}:{role}");
    let resolution = format!("{MOVED_TO_RESOLUTION_BEFORE_UNLINK}:{role}");
    if stage == "resolution" && selected == resolution {
        park_at(&resolution);
    }
    if stage == "purge" {
        if selected == purge {
            park_at(&purge);
        }
        if selected == resolution {
            return PlannedRemovalCut::FailUnlink;
        }
    }
    PlannedRemovalCut::Proceed
}

#[cfg(feature = "crash-qualification-test-support")]
impl ResetCrashCut {
    fn before_publication(phase: ryeos_scheduler_contract::SchedulerRetirementPhase) -> Option<Self> {
        use ryeos_scheduler_contract::SchedulerRetirementPhase as Phase;
        match phase {
            Phase::Quarantined => Some(Self::QuarantinedBeforeAck),
            Phase::Purged => Some(Self::UnlinkedBeforeAck),
            _ => None,
        }
    }

    fn after_publication(phase: ryeos_scheduler_contract::SchedulerRetirementPhase) -> Option<Self> {
        use ryeos_scheduler_contract::SchedulerRetirementPhase as Phase;
        match phase {
            Phase::QuarantineAuthorized => Some(Self::QuarantineArmed),
            Phase::PurgeAuthorized => Some(Self::PurgeArmed),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::QuarantineArmed => "quarantine_armed",
            Self::QuarantinedBeforeAck => "quarantined_before_ack",
            Self::PurgeArmed => "purge_armed",
            Self::UnlinkedBeforeAck => "unlinked_before_ack",
        }
    }
}

/// Park the resetting thread forever at the `<boundary>:<role>` selected by
/// the qualification environment, after announcing it on stderr. The harness
/// SIGKILLs the process there, so no retry, release or unwinding can run
/// after the boundary's durable effects.
#[cfg(feature = "crash-qualification-test-support")]
fn reach_reset_crash_cut(cut: Option<ResetCrashCut>, role: SchedulerRetirementRole) {
    let Some(cut) = cut else {
        return;
    };
    let role = match role {
        SchedulerRetirementRole::Main => "main",
        SchedulerRetirementRole::Wal => "wal",
        SchedulerRetirementRole::Shm => "shm",
    };
    let boundary = format!("{}:{role}", cut.name());
    let selected = std::env::var_os(ryeos_app::env_contract::SCHEDULER_RESET_TEST_CUT_ENV);
    if selected.as_deref() != Some(OsStr::new(&boundary)) {
        return;
    }
    park_at(&boundary);
}

/// Announce `boundary` on stderr and park the thread forever; the harness
/// SIGKILLs the process there.
#[cfg(feature = "crash-qualification-test-support")]
fn park_at(boundary: &str) -> ! {
    eprintln!(
        "{}scheduler_reset:{boundary}",
        ryeos_app::env_contract::TEST_CRASH_CUT_REACHED_PREFIX
    );
    loop {
        std::thread::park();
    }
}

/// Signed by the node once State has acknowledged the reset operation that
/// one lineage of scheduler generations served, and only after that
/// lineage's terminal `Complete` record is durable. While this record
/// exists, the listed generations' journal records are being removed:
/// scanners skip them (each must still be absent from every role
/// coordinate), no new generation may join the operation, and the next
/// acknowledged reset finishes removing them before it removes this record.
/// A completed scheduler reset therefore leaves no scheduler state, and a
/// process that dies partway through leaves a state the next reset settles.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredOperationEvidence {
    schema: u32,
    purpose: String,
    operation_id: String,
    owner_fingerprint: String,
    runtime_identity: String,
    /// The one generation of the lineage that published `Complete`.
    terminal_generation: String,
    /// SHA-256 of that generation's signed `Complete` record bytes.
    terminal_sha256: String,
    /// Every generation of the operation's lineage, sorted and unique.
    generations: Vec<String>,
}

impl RetiredOperationEvidence {
    fn validate(&self, name: &str, identity: &NodeIdentity, runtime_identity: &str) -> Result<()> {
        ensure!(
            self.schema == 1 && self.purpose == RETIRED_PURPOSE,
            "retired scheduler evidence record has an unsupported schema or purpose"
        );
        validate_generation(&self.operation_id)?;
        validate_generation(&self.terminal_generation)?;
        ensure!(
            name.strip_prefix(RETIRED_PREFIX) == Some(self.operation_id.as_str()),
            "retired scheduler evidence record is not at its operation's name"
        );
        ensure!(
            self.owner_fingerprint == identity.fingerprint()
                && self.runtime_identity == runtime_identity,
            "retired scheduler evidence record differs from current signer/runtime pins"
        );
        ensure!(
            self.terminal_sha256.len() == 64
                && self
                    .terminal_sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "retired scheduler evidence record has a malformed terminal digest"
        );
        ensure!(
            !self.generations.is_empty()
                && self.generations.len() <= MAX_GENERATIONS
                && self.generations.windows(2).all(|pair| pair[0] < pair[1])
                && self.generations.contains(&self.terminal_generation),
            "retired scheduler evidence record lists an invalid generation set"
        );
        for generation in &self.generations {
            validate_generation(generation)?;
        }
        Ok(())
    }
}

/// Signed by the node before a reset removes any fire journal. While it
/// exists, the fires root is being emptied: every well-formed schedule
/// directory (only `fires.jsonl` and its `.fires.jsonl.lock`, both regular
/// files) goes, and the next retirement completion finishes what a process
/// that died partway left. Any partial state of that removal is itself a
/// well-formed fires tree, so resumption needs no per-file list; the counts
/// record what the retirement found when it began. The intent is removed
/// once the tree is empty, before the generation's `Complete` record, so a
/// State acknowledgement never precedes the fire history's retirement.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FireHistoryRetirementIntent {
    schema: u32,
    purpose: String,
    operation_id: String,
    operation_sha256: String,
    owner_fingerprint: String,
    runtime_identity: String,
    /// Schedule directories the fires root held when retirement began.
    schedules: u32,
    /// Regular fire-journal and lock files those directories held.
    files: u32,
}

impl FireHistoryRetirementIntent {
    fn validate(&self, name: &str, identity: &NodeIdentity, runtime_identity: &str) -> Result<()> {
        ensure!(
            self.schema == 1 && self.purpose == FIRES_PURPOSE,
            "scheduler fire-history retirement intent has an unsupported schema or purpose"
        );
        validate_generation(&self.operation_id)?;
        ensure!(
            name.strip_prefix(FIRES_PREFIX) == Some(self.operation_id.as_str()),
            "scheduler fire-history retirement intent is not at its operation's name"
        );
        ensure!(
            self.operation_sha256.len() == 64
                && self
                    .operation_sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "scheduler fire-history retirement intent has a malformed operation digest"
        );
        ensure!(
            self.owner_fingerprint == identity.fingerprint()
                && self.runtime_identity == runtime_identity,
            "scheduler fire-history retirement intent differs from current signer/runtime pins"
        );
        Ok(())
    }
}

/// One schedule's fire directory, classified without following links and
/// without reading any fire content.
struct FireDirectory {
    name: std::ffi::OsString,
    directory: lillux::PinnedDirectory,
    files: Vec<std::ffi::OsString>,
}

/// Classify the whole fires root before anything is removed. Only schedule
/// directories named by a valid schedule id, holding nothing but a regular
/// `fires.jsonl` and a regular `.fires.jsonl.lock`, are fire history; any
/// other entry refuses, is named, and keeps everything in place. Returns
/// `None` when the runtime holds no fires root.
fn classify_fires_root(
    runtime: &lillux::PinnedDirectory,
) -> Result<Option<(lillux::PinnedDirectory, Vec<FireDirectory>)>> {
    // A shape refusal is permanent: no retry changes it until the operator
    // removes the named entry, so the reset surfaces it at once.
    let refuse = |message: String| {
        anyhow::Error::new(ryeos_app::execution_history_reset::SchedulerPermanentRefusal(message))
    };
    let root_name = OsStr::new(FIRES_ROOT);
    let Some(entry) = runtime.entry_no_follow(root_name)? else {
        return Ok(None);
    };
    let root_path = runtime.path().join(root_name);
    if !matches!(entry.entry_type, lillux::PinnedEntryType::Directory) {
        return Err(refuse(format!(
            "scheduler fires root {} is not a directory; it is kept and the reset cannot retire \
             fire history until it is removed by hand",
            root_path.display()
        )));
    }
    let root = runtime
        .open_child_directory(root_name)?
        .with_context(|| format!("scheduler fires root {} changed while pinned", root_path.display()))?;
    let names = root.entry_names_bounded(MAX_DIRECTORY_ENTRIES + 1)?;
    if names.len() > MAX_DIRECTORY_ENTRIES {
        return Err(refuse(format!(
            "scheduler fires root {} exceeds its {MAX_DIRECTORY_ENTRIES}-entry bound; every entry \
             is kept",
            root_path.display()
        )));
    }
    let mut directories = Vec::new();
    for name in names {
        let path = root.path().join(&name);
        let foreign = || {
            refuse(format!(
                "scheduler fires root holds a foreign entry {}; it is kept and the reset cannot \
                 retire fire history until it is removed by hand",
                path.display()
            ))
        };
        let schedule_id = name.to_str().ok_or_else(foreign)?;
        ryeos_scheduler_contract::validate_schedule_id(schedule_id).map_err(|_| foreign())?;
        let entry = root.entry_no_follow(&name)?.ok_or_else(foreign)?;
        if !matches!(entry.entry_type, lillux::PinnedEntryType::Directory) {
            return Err(foreign());
        }
        let directory = root.open_child_directory(&name)?.ok_or_else(foreign)?;
        let entries = directory.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        // Only the two fixed names are allowed, so any third entry is named
        // as foreign below.
        let mut files = Vec::new();
        for file in entries {
            let file_path = directory.path().join(&file);
            let allowed = matches!(file.to_str(), Some(FIRE_JOURNAL | FIRE_JOURNAL_LOCK));
            let regular = directory
                .entry_no_follow(&file)?
                .is_some_and(|entry| matches!(entry.entry_type, lillux::PinnedEntryType::Regular));
            if !(allowed && regular) {
                return Err(refuse(format!(
                    "scheduler fire directory holds a foreign entry {}; it is kept and the reset \
                     cannot retire fire history until it is removed by hand",
                    file_path.display()
                )));
            }
            files.push(file);
        }
        directories.push(FireDirectory {
            name,
            directory,
            files,
        });
    }
    Ok(Some((root, directories)))
}

/// Retire every schedule's fire history for one reset operation, returning
/// how many fire files and schedule directories were removed. The empty
/// fires root itself stays until the operation is acknowledged.
///
/// The caller holds the reset's exclusive offline authority for the whole
/// call: the State lock (which every daemon and standalone scheduler
/// holds), the runtime namespace lock and an Active discard operation. A
/// signed [`FireHistoryRetirementIntent`] is durable before the first
/// removal; an intent an earlier attempt left (of this or any earlier
/// operation of this node) is finished the same way. Each file is pinned
/// `O_PATH` and unlinked only while its name still holds that inode; each
/// emptied schedule directory is removed only as the pinned directory.
pub(crate) fn retire_fire_history(
    runtime: &lillux::PinnedDirectory,
    identity: &NodeIdentity,
    operation_id: &str,
    operation_sha256: &str,
) -> Result<usize> {
    let runtime_identity = serde_json::to_string(&runtime.identity()?)?;
    let mut begun = Vec::new();
    for name in runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)? {
        let display = name.to_string_lossy();
        if display.starts_with(FIRES_PREFIX) {
            let intent: FireHistoryRetirementIntent = read_signed(runtime, &display, identity)?;
            intent.validate(&display, identity, &runtime_identity)?;
            if intent.operation_id == operation_id {
                ensure!(
                    intent.operation_sha256 == operation_sha256,
                    "scheduler fire-history retirement intent names another operation digest"
                );
            }
            begun.push((display.into_owned(), intent));
        }
    }
    // The whole tree is classified before any effect, so a foreign entry
    // keeps every fire file in place.
    let classified = classify_fires_root(runtime)?;
    let own_name = format!("{FIRES_PREFIX}{operation_id}");
    if begun.is_empty() {
        let Some((_, directories)) = classified.as_ref() else {
            return Ok(0);
        };
        if directories.is_empty() {
            return Ok(0);
        }
        let files = directories
            .iter()
            .map(|directory| directory.files.len())
            .sum::<usize>();
        let intent = FireHistoryRetirementIntent {
            schema: 1,
            purpose: FIRES_PURPOSE.to_owned(),
            operation_id: operation_id.to_owned(),
            operation_sha256: operation_sha256.to_owned(),
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: runtime_identity.clone(),
            schedules: u32::try_from(directories.len())?,
            files: u32::try_from(files)?,
        };
        intent.validate(&own_name, identity, &runtime_identity)?;
        publish_signed(runtime, identity, &own_name, &intent)?;
        runtime
            .sync()
            .context("sync the fire-history retirement intent before any removal")?;
        begun.push((own_name, intent));
    }
    let mut removed = 0usize;
    if let Some((root, directories)) = classified {
        for fire_directory in directories {
            for file in &fire_directory.files {
                let pin = fire_directory
                    .directory
                    .open_mount_entry(file)?
                    .with_context(|| {
                        format!(
                            "scheduler fire file {} disappeared before removal",
                            fire_directory.directory.path().join(file).display()
                        )
                    })?;
                ensure!(
                    pin.metadata()?.is_file(),
                    "scheduler fire file {} is no longer a regular file",
                    fire_directory.directory.path().join(file).display()
                );
                ensure!(
                    fire_directory.directory.unlink_regular_if_same(file, &pin)?,
                    "scheduler fire file {} disappeared during removal",
                    fire_directory.directory.path().join(file).display()
                );
                removed += 1;
            }
            ensure!(
                root.remove_empty_child_if_same(&fire_directory.name, &fire_directory.directory)?
                    .is_some(),
                "scheduler fire directory {} gained an entry during retirement; it is kept",
                fire_directory.directory.path().display()
            );
            removed += 1;
        }
        root.sync()?;
    }
    for (name, intent) in begun {
        remove_signed_record(runtime, &name, |bytes| {
            let current: FireHistoryRetirementIntent = verify_signed(bytes, identity)?;
            ensure!(
                current == intent,
                "scheduler fire-history retirement intent changed during retirement"
            );
            Ok(())
        })?;
    }
    runtime.sync()?;
    Ok(removed)
}

/// The generation a signed journal record name belongs to: an intent, a
/// phase record, or a resolution intent/outcome. Other names are `None`.
fn journal_record_generation(name: &str) -> Result<Option<String>> {
    if let Some(generation) = name.strip_prefix(INTENT_PREFIX) {
        validate_generation(generation)?;
        return Ok(Some(generation.to_owned()));
    }
    if let Some(rest) = name.strip_prefix(PHASE_PREFIX) {
        return Ok(Some(parse_phase_name(rest)?.0));
    }
    if let Some(rest) = name.strip_prefix(RESOLUTION_PREFIX) {
        return Ok(Some(parse_resolution_name(rest)?.0));
    }
    Ok(None)
}

/// Authenticate every retired-evidence record in `names` and return each
/// listed generation with its operation. A surviving `Complete` record of a
/// listed terminal generation must still hash to the bound digest, and a
/// surviving intent of a listed generation must name the same operation.
fn retired_generations_in(
    factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    runtime: &lillux::PinnedDirectory,
    identity: &NodeIdentity,
    names: &[std::ffi::OsString],
) -> Result<BTreeMap<String, String>> {
    let runtime_identity = serde_json::to_string(&runtime.identity()?)?;
    let mut retired = BTreeMap::<String, String>::new();
    for name in names {
        let display = name.to_string_lossy();
        if !display.starts_with(RETIRED_PREFIX) {
            continue;
        }
        let record: RetiredOperationEvidence = read_signed_in(factory, runtime, &display, identity)?;
        record.validate(&display, identity, &runtime_identity)?;
        let complete = phase_name(&record.terminal_generation, COMPLETE_SEQUENCE);
        if let Some(bytes) = read_optional_signed_bytes_in(factory, runtime, &complete)? {
            ensure!(
                lillux::sha256_hex(&bytes) == record.terminal_sha256,
                "retired scheduler evidence record does not bind its surviving terminal record"
            );
        }
        for generation in &record.generations {
            let intent_name = format!("{INTENT_PREFIX}{generation}");
            if let Some(bytes) = read_optional_signed_bytes_in(factory, runtime, &intent_name)? {
                let intent: SchedulerRetirementIntent = verify_signed(&bytes, identity)?;
                ensure!(
                    intent.generation == *generation && intent.operation_id == record.operation_id,
                    "retired scheduler generation intent names another operation"
                );
            }
            ensure!(
                retired
                    .insert(generation.clone(), record.operation_id.clone())
                    .is_none(),
                "scheduler generation is retired by more than one operation record"
            );
        }
    }
    Ok(retired)
}

#[derive(Default)]
struct ResolutionFiles {
    intent: Option<String>,
    outcome: Option<String>,
    quarantine: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct RetainedResolutionOutcome {
    value: SchedulerRetirementResolutionOutcome,
    signed: Vec<u8>,
}

/// Authenticated observations for a stopped interrupted generation. It carries
/// only signed records and fixed coordinate names; the current State/CAS permit
/// and a newly acquired native owner must still authorize every effect.
#[derive(Clone)]
pub(crate) struct AuthenticatedInterruptedGeneration {
    pub(crate) generation_intent: SchedulerRetirementIntent,
    pub(crate) generation_intent_sha256: String,
    pub(crate) phase_records: Vec<SchedulerRetirementRecord>,
    pub(crate) phase_sha256: Vec<String>,
    pub(crate) resolution_slots: Vec<AuthenticatedResolutionSlot>,
    pub(crate) candidate_coordinates: Vec<String>,
    /// Every signed generation id in the bounded namespace, including
    /// already-terminal generations, for the stopped plan's full name set.
    pub(crate) known_generations: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct AuthenticatedResolutionSlot {
    pub(crate) intent: SchedulerRetirementResolutionIntent,
    pub(crate) intent_sha256: String,
    pub(crate) outcome: Option<SchedulerRetirementResolutionOutcome>,
    pub(crate) outcome_sha256: Option<String>,
}

pub(crate) struct RetirementJournal<'a> {
    runtime: &'a lillux::PinnedDirectory,
    identity: &'a NodeIdentity,
    generation: String,
    runtime_identity: String,
    generation_intent: SchedulerRetirementIntent,
    previous_hash: Option<String>,
    next_sequence: u8,
    next_resolution_ordinal: [u8; 3],
    pending_resolution: [Option<ResolutionReservation>; 3],
    previous_resolution_outcome: [Option<String>; 3],
    pending_resolution_outcome: [Option<RetainedResolutionOutcome>; 3],
    resolution_record_count: usize,
    resolution_quarantine_count: usize,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ResolutionReservation {
    pub(crate) intent: SchedulerRetirementResolutionIntent,
    pub(crate) signed_intent_sha256: String,
    pub(crate) signed_intent: Vec<u8>,
    pub(crate) host_expected: lillux_sqlite::SqliteRoleResolutionObservation,
    intent_is_durable: bool,
}

fn original_stopped_plan(
    runtime: &lillux::PinnedDirectory,
    _identity: &NodeIdentity,
    generation: String,
    sources: [SchedulerRetirementStoppedRolePlan; 3],
) -> Result<SchedulerRetirementStoppedPlan> {
    // Copied original-operation reference only, not a published intake grant.
    // The actual normal producer supplies its full authenticated inventory.
    let generations = vec![generation.clone()];
    let plan = SchedulerRetirementStoppedPlan {
        generation,
        generations,
        runtime_identity: serde_json::to_string(&runtime.identity()?)?,
        roles: sources,
    };
    plan.validate()?;
    Ok(plan)
}

fn intent_for_permit(
    permit: &SchedulerResetRetirement<'_, '_>,
    generation: String,
    recovery: Option<SchedulerRetirementRecoveryBinding>,
    stopped_plan: Option<SchedulerRetirementStoppedPlan>,
) -> Result<SchedulerRetirementIntent> {
    let reference = permit.operation_reference()?;
    reference.validate()?;
    ensure!(
        lillux::sha256_hex(permit.original_operation_bytes()?) == reference.sha256,
        "scheduler permit operation differs from its original signed bytes"
    );
    ensure!(
        parse_generation(&reference.operation_id)? == permit.initial_scheduler_generation()?,
        "scheduler initial generation differs from the original permit operation"
    );
    ensure!(
        recovery.is_none() || stopped_plan.is_none(),
        "recovery plan must be carried once by its signed binding"
    );
    let intent = SchedulerRetirementIntent {
        schema: 3,
        purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
        generation,
        operation_id: reference.operation_id.clone(),
        operation_sha256: reference.sha256.clone(),
        scheduler_accounting: permit.original_scheduler_accounting()?.clone(),
        scheduler_sources: permit.original_scheduler_sources()?.clone(),
        owner_fingerprint: permit.node_identity().fingerprint().to_owned(),
        runtime_identity: permit.runtime_identity()?,
        roles: [None, None, None],
        intake_mode: if recovery.is_some() {
            "stopped-recovery"
        } else {
            "stopped-original"
        }
        .to_owned(),
        stopped_plan: if recovery.is_some() {
            None
        } else {
            Some(match stopped_plan {
                Some(plan) => plan,
                None => original_stopped_plan(
                    permit.runtime_directory()?,
                    permit.node_identity(),
                    reference.operation_id.clone(),
                    permit.original_scheduler_sources()?.clone(),
                )?,
            })
        },
        recovery,
    };
    intent.validate()?;
    Ok(intent)
}

fn intent_for_preview(
    permit: &SchedulerResetPreview<'_>,
) -> Result<Option<SchedulerRetirementIntent>> {
    let Some(reference) = permit.original_operation_reference()? else {
        return Ok(None);
    };
    reference.validate()?;
    let identity = permit
        .node_identity()
        .context("original scheduler operation lacks selected node identity")?;
    let bytes = permit
        .original_operation_bytes()?
        .context("original scheduler operation bytes are absent")?;
    ensure!(
        lillux::sha256_hex(bytes) == reference.sha256,
        "scheduler preview differs from its original signed operation bytes"
    );
    let intent = SchedulerRetirementIntent {
        schema: 3,
        purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
        generation: reference.operation_id.clone(),
        operation_id: reference.operation_id.clone(),
        operation_sha256: reference.sha256.clone(),
        scheduler_accounting: permit
            .original_scheduler_accounting()?
            .context("original scheduler accounting absent")?
            .clone(),
        scheduler_sources: permit
            .original_scheduler_sources()?
            .context("original scheduler sources absent")?
            .clone(),
        owner_fingerprint: identity.fingerprint().to_owned(),
        runtime_identity: serde_json::to_string(&permit.runtime_directory()?.identity()?)?,
        roles: [None, None, None],
        intake_mode: "stopped-original".to_owned(),
        stopped_plan: Some(original_stopped_plan(
            permit.runtime_directory()?,
            identity,
            reference.operation_id.clone(),
            permit
                .original_scheduler_sources()?
                .context("original scheduler sources absent")?
                .clone(),
        )?),
        recovery: None,
    };
    intent.validate()?;
    Ok(Some(intent))
}

fn is_campaign_signed_name(name: &str) -> bool {
    name.starts_with(INTENT_PREFIX)
        || name.starts_with(PHASE_PREFIX)
        || name.starts_with(RESOLUTION_PREFIX)
        || name.starts_with(ORIGINAL_OPERATION_PREFIX)
        || name.starts_with(RETIRED_PREFIX)
        || name.starts_with(FIRES_PREFIX)
}

impl<'a> RetirementJournal<'a> {
    /// Admission only: reserve the full bounded remainder of every known
    /// generation plus a possible original-operation publication and compact
    /// cleanup evidence. This does not issue mutation or resource authority.
    ///
    /// A generation an authenticated retired-evidence record lists needs no
    /// further records, so it holds no generation slot; its surviving records
    /// still count toward the signed totals. Recognising one requires the
    /// node identity, so without it any retired-evidence record refuses.
    pub(crate) fn reserve_pre_effect_capacity(
        runtime: &lillux::PinnedDirectory,
        identity: Option<&NodeIdentity>,
        new_generation: bool,
    ) -> Result<()> {
        let names = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        let retired = match identity {
            Some(identity) => retired_generations_in(None, runtime, identity, &names)?,
            None => {
                ensure!(
                    !names
                        .iter()
                        .any(|name| name.to_string_lossy().starts_with(RETIRED_PREFIX)),
                    "retired scheduler evidence requires the node identity to reserve capacity"
                );
                BTreeMap::new()
            }
        };
        let mut generations = BTreeMap::<String, usize>::new();
        let mut signed_count = 0usize;
        let mut signed_bytes = 0usize;
        for name in &names {
            let display = name.to_string_lossy();
            let generation = if let Some(id) = display.strip_prefix(INTENT_PREFIX) {
                validate_generation(id)?;
                Some(id.to_owned())
            } else if let Some(rest) = display.strip_prefix(PHASE_PREFIX) {
                Some(parse_phase_name(rest)?.0)
            } else if let Some(rest) = display.strip_prefix(RESOLUTION_PREFIX) {
                Some(parse_resolution_name(rest)?.0)
            } else {
                None
            };
            if let Some(generation) = generation.filter(|generation| !retired.contains_key(generation)) {
                *generations.entry(generation).or_default() += 1;
            }
            if let Some(id) = display.strip_prefix(ORIGINAL_OPERATION_PREFIX) {
                validate_generation(id)?;
            }
            if let Some(id) = display
                .strip_prefix(RETIRED_PREFIX)
                .or_else(|| display.strip_prefix(FIRES_PREFIX))
            {
                validate_generation(id)?;
            }
            if is_campaign_signed_name(&display) {
                signed_count = signed_count
                    .checked_add(1)
                    .context("pre-effect signed count overflow")?;
                signed_bytes = signed_bytes
                    .checked_add(read_signed_bytes(runtime, &display)?.len())
                    .context("pre-effect signed bytes overflow")?;
            }
        }
        ensure!(
            generations
                .len()
                .checked_add(usize::from(new_generation))
                .is_some_and(|count| count <= MAX_GENERATIONS),
            "scheduler reset lacks pre-effect generation capacity"
        );
        let per_generation =
            ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS_PER_GENERATION;
        let mut remaining = usize::from(new_generation) * per_generation;
        for count in generations.values() {
            remaining = remaining
                .checked_add(
                    per_generation
                        .checked_sub(*count)
                        .context("scheduler generation exceeds its signed-record capacity")?,
                )
                .context("scheduler remaining reservation overflow")?;
        }
        // One operation may not yet be staged at the first preflight. Reserve
        // it even at post-stage checks, plus one bounded inventory part and
        // terminal anchor per maximum generation. These are capacity margins,
        // not fabricated records, cleanup authorization or allocation grants.
        remaining = remaining
            .checked_add(1 + 2 * MAX_GENERATIONS)
            .context("scheduler operation/cleanup reservation overflow")?;
        let reserved_count = signed_count
            .checked_add(remaining)
            .context("scheduler pre-effect record reservation overflow")?;
        let reserved_bytes = signed_bytes
            .checked_add(
                remaining
                    .checked_mul(ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES)
                    .context("scheduler pre-effect byte reservation overflow")?,
            )
            .context("scheduler pre-effect byte reservation overflow")?;
        let namespace_growth = remaining
            .checked_mul(2)
            .and_then(|count| {
                count.checked_add(
                    ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_RECOVERY_CANDIDATES,
                )
            })
            .and_then(|count| {
                count.checked_add(
                    ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_PUBLICATION_RESIDUE,
                )
            })
            .context("scheduler pre-effect namespace reservation overflow")?;
        ensure!(
            reserved_count <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS
                && reserved_count
                    <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_CLEANUP_COORDINATES
                && reserved_bytes
                    <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES_TOTAL
                && names
                    .len()
                    .checked_add(namespace_growth)
                    .is_some_and(|count| count <= MAX_DIRECTORY_ENTRIES),
            "scheduler reset lacks aggregate pre-effect signed/cleanup/byte/namespace capacity"
        );
        Ok(())
    }

    pub(crate) fn authenticated_generation_ids(
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
    ) -> Result<Vec<String>> {
        let runtime_identity = serde_json::to_string(&runtime.identity()?)?;
        let mut generations = Vec::new();
        let names = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        // A retired generation is no longer part of the inventory any plan
        // binds, matching the evidence scan's known generations.
        let retired = retired_generations_in(None, runtime, identity, &names)?;
        for name in names {
            let display = name.to_string_lossy();
            let Some(generation) = display.strip_prefix(INTENT_PREFIX) else {
                continue;
            };
            if retired.contains_key(generation) {
                continue;
            }
            let intent: SchedulerRetirementIntent = read_signed(runtime, &display, identity)?;
            intent.validate()?;
            ensure!(
                intent.generation == generation
                    && intent.owner_fingerprint == identity.fingerprint()
                    && intent.runtime_identity == runtime_identity,
                "scheduler fixed inventory differs from signed generation owner/root"
            );
            generations.push(generation.to_owned());
        }
        generations.sort();
        ensure!(
            generations.len() <= MAX_GENERATIONS,
            "scheduler fixed inventory exceeds generation bound"
        );
        Ok(generations)
    }

    pub(crate) fn validate_original_preview(permit: &SchedulerResetPreview<'_>) -> Result<()> {
        intent_for_preview(permit)?;
        Ok(())
    }

    pub(crate) fn validate_for_preview(
        intent: &SchedulerRetirementIntent,
        permit: &SchedulerResetPreview<'_>,
    ) -> Result<()> {
        let original = intent_for_preview(permit)?
            .context("interrupted scheduler lacks its original staged operation")?;
        intent.validate_same_operation(&original)
    }

    pub(crate) fn completed_preview_generation(
        permit: &'a SchedulerResetPreview<'_>,
    ) -> Result<Option<[u8; 16]>> {
        let original = intent_for_preview(permit)?
            .context("scheduler completion lacks its original staged operation")?;
        Self::completed_generation_for_evidence(
            permit.runtime_directory()?,
            permit
                .node_identity()
                .context("scheduler completion lacks its original node identity")?,
            &original,
        )
    }

    pub(crate) fn validate_for_permit(
        intent: &SchedulerRetirementIntent,
        permit: &SchedulerResetRetirement<'_, '_>,
    ) -> Result<()> {
        let expected = intent_for_permit(
            permit,
            intent.generation.clone(),
            intent.recovery.clone(),
            intent.stopped_plan.clone(),
        )?;
        ensure!(
            *intent == expected,
            "scheduler generation differs from the actual original operation permit"
        );
        Ok(())
    }

    /// Validate that prior retirement work reached a durable terminal record.
    /// This admission check deliberately does not infer authority from old
    /// role descriptors or reopen them: interrupted generations require a
    /// separately authenticated resolution path.
    pub(crate) fn validate_admission(
        runtime: &'a lillux::PinnedDirectory,
        identity: &'a NodeIdentity,
    ) -> Result<usize> {
        Self::validate_admission_in(None, runtime, identity)
    }

    /// Admission with its guarded reads inside `factory`'s native scope.
    pub(crate) fn validate_admission_in(
        factory: Option<&lillux_sqlite::OriginalNativeFactory>,
        runtime: &'a lillux::PinnedDirectory,
        identity: &'a NodeIdentity,
    ) -> Result<usize> {
        let artifacts = Self::preflight_in(factory, runtime, identity)?;
        let names = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        // Generations whose acknowledged evidence is being removed are
        // settled; `preflight` has authenticated their retirement record.
        let retired = retired_generations_in(factory, runtime, identity, &names)?;
        let mut generations = std::collections::BTreeSet::new();
        let mut terminal = std::collections::BTreeSet::new();
        for name in names {
            let display = name.to_string_lossy();
            if journal_record_generation(&display)?
                .is_some_and(|generation| retired.contains_key(&generation))
            {
                continue;
            }
            if let Some(generation) = display.strip_prefix(INTENT_PREFIX) {
                validate_generation(generation)?;
                generations.insert(generation.to_owned());
            } else if let Some(rest) = display.strip_prefix(PHASE_PREFIX) {
                let (generation, sequence) = parse_phase_name(rest)?;
                generations.insert(generation.clone());
                if sequence == COMPLETE_SEQUENCE {
                    let record: SchedulerRetirementRecord =
                        read_signed_in(factory, runtime, &phase_name(&generation, sequence), identity)?;
                    ensure!(
                        record.phase
                            == ryeos_scheduler_contract::SchedulerRetirementPhase::Complete,
                        "scheduler retirement has released roles but lacks terminal completion"
                    );
                    terminal.insert(generation);
                }
            }
        }
        let mut settled_by_recovery_child = terminal.clone();
        loop {
            let before = settled_by_recovery_child.len();
            let settled = settled_by_recovery_child
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            for generation in settled {
                let intent_name = format!("{INTENT_PREFIX}{generation}");
                let intent: SchedulerRetirementIntent =
                    read_signed_in(factory, runtime, &intent_name, identity)?;
                if let Some(binding) = intent.recovery {
                    // `preflight` has already authenticated this exact parent
                    // intent and dense phase chain. Follow the bounded lineage
                    // so a terminal descendant settles every earlier prefix.
                    settled_by_recovery_child.insert(binding.parent_generation);
                }
            }
            if settled_by_recovery_child.len() == before {
                break;
            }
        }
        ensure!(
            generations.iter().all(|generation| {
                terminal.contains(generation) || settled_by_recovery_child.contains(generation)
            }),
            "interrupted scheduler retirement requires authenticated resolution under current pins"
        );
        Ok(artifacts)
    }

    pub(crate) fn preflight(
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
    ) -> Result<usize> {
        Self::preflight_in(None, runtime, identity)
    }

    fn preflight_in(
        factory: Option<&lillux_sqlite::OriginalNativeFactory>,
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
    ) -> Result<usize> {
        // One authenticated reader owns prefix, resolution and ancestry rules.
        // Startup admission below still refuses any unresolved leaf.
        Self::scan_interrupted_evidence_in(factory, runtime, identity)?;
        Ok(runtime
            .entry_names_bounded(MAX_DIRECTORY_ENTRIES)?
            .iter()
            .filter(|name| name.to_string_lossy().starts_with(RESERVED_PREFIX))
            .count())
    }

    /// Read-only restart evidence scan. Unlike startup admission, this accepts
    /// a legal incomplete phase prefix and one last intent without an outcome;
    /// it never turns an old pathname observation into mutation authority.
    pub(crate) fn scan_interrupted_evidence(
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
    ) -> Result<Vec<AuthenticatedInterruptedGeneration>> {
        Self::scan_interrupted_evidence_in(None, runtime, identity)
    }

    /// The same scan with its guarded reads inside `factory`'s native scope,
    /// so projection leases that factory holds answer for their owner.
    pub(crate) fn scan_interrupted_evidence_in(
        factory: Option<&lillux_sqlite::OriginalNativeFactory>,
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
    ) -> Result<Vec<AuthenticatedInterruptedGeneration>> {
        let runtime_identity = serde_json::to_string(&runtime.identity()?)?;
        let names = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES + 1)?;
        ensure!(
            names.len() <= MAX_DIRECTORY_ENTRIES,
            "scheduler directory census exceeds its bound"
        );
        let retired = retired_generations_in(factory, runtime, identity, &names)?;
        let retired_operations = retired
            .values()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut generation_intents = BTreeMap::<String, String>::new();
        let mut phases = BTreeMap::<String, Vec<u8>>::new();
        let mut resolution_files = BTreeMap::<(String, usize, u8), ResolutionFiles>::new();
        let mut role_artifact_generations = std::collections::BTreeSet::new();
        let mut signed_names = Vec::<String>::new();
        for name in &names {
            let display = name.to_string_lossy();
            if let Some(operation_id) = display.strip_prefix(ORIGINAL_OPERATION_PREFIX) {
                validate_generation(operation_id)?;
                signed_names.push(display.to_string());
                continue;
            }
            match lillux::classify_secure_temp_name(name, crate::SCHEDULER_TEMP_PURPOSE) {
                lillux::SecureTempOwnership::NotTemp => {}
                lillux::SecureTempOwnership::Own => anyhow::bail!(
                    "unfinished scheduler journal publication requires held recovery"
                ),
                // The scheduler directory is shared with other Runtime owners;
                // their interrupted publications are theirs to settle.
                lillux::SecureTempOwnership::Foreign => continue,
                lillux::SecureTempOwnership::Malformed => anyhow::bail!(
                    "scheduler directory holds a secure temporary without an exact owner token: {display}; \
                     stop the node and remove that entry before scheduler retirement can continue"
                ),
            }
            // An acknowledged operation's retirement record and the surviving
            // records of its retired generations stay inside the signed
            // bounds until removal finishes; they are no longer evidence any
            // reset resumes. Their role and resolution coordinates are not
            // skipped: those names find no generation intent below and refuse.
            if display.starts_with(RETIRED_PREFIX) {
                // Authenticated above by `retired_generations_in`.
                signed_names.push(display.to_string());
                continue;
            }
            if let Some(generation) = journal_record_generation(&display)? {
                if let Some(operation_id) = retired.get(&generation) {
                    // Skipped, but only as this node's signed record of the
                    // listed generation: a forged file refuses here, at
                    // admission, never later.
                    let bytes = read_signed_bytes_in(factory, runtime, &display)?;
                    authenticate_journal_record(&display, &bytes, identity, &generation, operation_id)?;
                    signed_names.push(display.to_string());
                    continue;
                }
            }
            // A begun fire-history retirement is finished by the next
            // retirement completion; it must be this node's signed intent.
            if display.starts_with(FIRES_PREFIX) {
                let intent: FireHistoryRetirementIntent =
                    read_signed_in(factory, runtime, &display, identity)?;
                intent.validate(&display, identity, &runtime_identity)?;
                signed_names.push(display.to_string());
                continue;
            }
            if let Some(generation) = display.strip_prefix(INTENT_PREFIX) {
                validate_generation(generation)?;
                generation_intents.insert(generation.to_owned(), display.to_string());
                signed_names.push(display.to_string());
            } else if let Some(rest) = display.strip_prefix(PHASE_PREFIX) {
                let (generation, sequence) = parse_phase_name(rest)?;
                phases.entry(generation).or_default().push(sequence);
                signed_names.push(display.to_string());
            } else if let Some(rest) = display.strip_prefix(RESOLUTION_PREFIX) {
                let (generation, role, ordinal, outcome) = parse_resolution_name(rest)?;
                let files = resolution_files
                    .entry((generation, role, ordinal))
                    .or_default();
                let slot = if outcome {
                    &mut files.outcome
                } else {
                    &mut files.intent
                };
                ensure!(
                    slot.replace(display.to_string()).is_none(),
                    "duplicate scheduler resolution record"
                );
                signed_names.push(display.to_string());
            } else if let Some(rest) = display.strip_prefix(RESOLUTION_QUARANTINE_PREFIX) {
                let (generation, role, ordinal) = parse_resolution_quarantine_name(rest)?;
                let files = resolution_files
                    .entry((generation, role, ordinal))
                    .or_default();
                ensure!(
                    !files.quarantine,
                    "duplicate scheduler resolution coordinate"
                );
                files.quarantine = true;
            } else if display.starts_with("scheduler-retirement.role.") {
                let generation = parse_role_artifact_name(&display)?;
                role_artifact_generations.insert(generation);
            } else if display.starts_with(RESERVED_PREFIX) {
                anyhow::bail!(
                    "unknown scheduler retirement artifact requires held recovery: {display}"
                );
            }
        }
        ensure!(
            signed_names.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS,
            "scheduler campaign signed-record limit exceeded"
        );
        let mut signed_bytes = 0usize;
        for name in &signed_names {
            signed_bytes = signed_bytes
                .checked_add(read_signed_bytes_in(factory, runtime, name)?.len())
                .context("scheduler campaign signed-byte total overflow")?;
        }
        ensure!(
            signed_bytes <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES_TOTAL,
            "scheduler campaign signed-byte limit exceeded"
        );
        ensure!(
            phases
                .keys()
                .all(|generation| generation_intents.contains_key(generation)),
            "scheduler phase evidence lacks a generation intent"
        );
        ensure!(
            generation_intents.len() <= MAX_GENERATIONS,
            "scheduler generation inventory exceeds its bound"
        );
        let terminal = phases
            .iter()
            .filter(|(_, values)| values.contains(&COMPLETE_SEQUENCE))
            .map(|(generation, _)| generation.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for ((generation, _, _), _) in &resolution_files {
            ensure!(
                generation_intents.contains_key(generation),
                "scheduler resolution artifact lacks a generation intent"
            );
        }
        for generation in &role_artifact_generations {
            ensure!(
                generation_intents.contains_key(generation) && !terminal.contains(generation),
                "scheduler role artifact lacks an interrupted owner with authorized effects"
            );
        }
        let mut child_parents = std::collections::BTreeSet::new();
        for intent_name in generation_intents.values() {
            let intent: SchedulerRetirementIntent = read_signed_in(factory, runtime, intent_name, identity)?;
            intent.validate()?;
            ensure!(
                !retired_operations.contains(&intent.operation_id),
                "scheduler generation joins an operation whose evidence is retired"
            );
            if let Some(binding) = intent.recovery {
                child_parents.insert(binding.parent_generation);
            }
        }
        let mut result = Vec::new();
        for (generation, intent_name) in &generation_intents {
            let is_terminal = terminal.contains(generation);
            let generation_intent: SchedulerRetirementIntent =
                read_signed_in(factory, runtime, intent_name, identity)?;
            generation_intent.validate()?;
            let generation_intent_sha256 =
                lillux::sha256_hex(&read_signed_bytes_in(factory, runtime, intent_name)?);
            ensure!(
                generation_intent.generation == *generation
                    && generation_intent.owner_fingerprint == identity.fingerprint()
                    && generation_intent.runtime_identity == runtime_identity,
                "interrupted scheduler generation intent differs from current signer/runtime pins"
            );
            let mut sequences = phases.get(generation).cloned().unwrap_or_default();
            sequences.sort_unstable();
            ensure!(
                sequences
                    .iter()
                    .copied()
                    .eq(0..u8::try_from(sequences.len())?)
                    && sequences.len() <= MAX_RECORDS_PER_GENERATION,
                "interrupted scheduler phase prefix is not dense and bounded"
            );
            let mut phase_records = Vec::new();
            let mut phase_sha256 = Vec::new();
            let mut previous_hash = None;
            for sequence in &sequences {
                let filename = phase_name(generation, *sequence);
                let record: SchedulerRetirementRecord = read_signed_in(factory, runtime, &filename, identity)?;
                record.validate()?;
                let bytes = read_signed_bytes_in(factory, runtime, &filename)?;
                ensure!(
                    record.generation == *generation
                        && record.sequence == *sequence
                        && record.owner_fingerprint == generation_intent.owner_fingerprint
                        && record.runtime_identity == generation_intent.runtime_identity
                        && record.previous_sha256 == previous_hash,
                    "interrupted scheduler phase lineage or signer changed"
                );
                previous_hash = Some(lillux::sha256_hex(&bytes));
                phase_sha256.push(previous_hash.clone().unwrap());
                phase_records.push(record);
            }
            if is_terminal {
                validate_completed_flow(&phase_records)?;
            } else {
                validate_phase_prefix(&phase_records)?;
            }
            validate_stopped_intake_prefix(&generation_intent, &phase_records)?;
            let mut resolution_slots = Vec::new();
            let mut prior_outcome = [None::<String>, None, None];
            let mut candidate_coordinates = std::collections::BTreeSet::new();
            for role in SchedulerRetirementRole::ALL {
                let role_i = role_index(role);
                let slots = resolution_files
                    .iter()
                    .filter(|((candidate, index, _), _)| {
                        candidate == generation && *index == role_i
                    })
                    .collect::<Vec<_>>();
                let mut expected_ordinal = 0u8;
                for ((_, _, ordinal), files) in slots {
                    ensure!(
                        *ordinal == expected_ordinal && files.intent.is_some(),
                        "interrupted scheduler resolution ordinal has a gap/orphan"
                    );
                    ensure!(
                        expected_ordinal
                            < ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE,
                        "interrupted scheduler resolution budget exceeded"
                    );
                    let intent_name = files.intent.as_ref().unwrap();
                    let intent: SchedulerRetirementResolutionIntent =
                        read_signed_in(factory, runtime, intent_name, identity)?;
                    intent.validate_for_generation(&generation_intent)?;
                    ensure!(
                        intent.generation == *generation
                            && intent.role == role
                            && intent.ordinal == *ordinal
                            && intent.owner_fingerprint == generation_intent.owner_fingerprint
                            && intent.runtime_identity == generation_intent.runtime_identity
                            && intent.intent_filename()? == *intent_name
                            && intent.previous_outcome_sha256 == prior_outcome[role_i],
                        "interrupted scheduler resolution intent is not bound to its generation/chain"
                    );
                    let parent_sequence = match intent.action {
                        SchedulerRetirementResolutionAction::SyncQuarantine
                        | SchedulerRetirementResolutionAction::RearmOriginal => match role {
                            SchedulerRetirementRole::Main => 9,
                            SchedulerRetirementRole::Wal => 13,
                            SchedulerRetirementRole::Shm => 17,
                        },
                        _ => match role {
                            SchedulerRetirementRole::Main => 11,
                            SchedulerRetirementRole::Wal => 15,
                            SchedulerRetirementRole::Shm => 19,
                        },
                    };
                    // A completed child may have advanced beyond the authorizing
                    // phase, but must still authenticate that exact signed ancestor.
                    // An interrupted owner remains bound to its latest phase.
                    let parent_matches =
                        phase_records
                            .iter()
                            .zip(&phase_sha256)
                            .any(|(record, hash)| {
                                (files.outcome.is_some()
                                    || Some(record) == phase_records.last()
                                    || child_parents.contains(generation))
                                    && record.sequence == parent_sequence
                                    && record.role == role
                                    && hash == &intent.parent_journal_sha256
                                    && resolution_identity_matches_phase(
                                        record.role_identity.as_ref(),
                                        intent.expected.original.as_ref(),
                                    )
                            });
                    ensure!(
                        parent_matches,
                        "interrupted resolution intent lacks its exact latest signed parent phase"
                    );
                    let intent_bytes = read_signed_bytes_in(factory, runtime, intent_name)?;
                    let intent_sha256 = lillux::sha256_hex(&intent_bytes);
                    ensure!(
                        !is_terminal || files.outcome.is_some(),
                        "terminal scheduler resolution intent lacks its signed outcome"
                    );
                    let (outcome, outcome_sha256) =
                        if let Some(outcome_name) = files.outcome.as_ref() {
                            let outcome: SchedulerRetirementResolutionOutcome =
                                read_signed_in(factory, runtime, outcome_name, identity)?;
                            ensure!(
                                intent.outcome_filename()? == *outcome_name,
                                "interrupted scheduler outcome filename mismatch"
                            );
                            outcome.validate_for_intent(&intent, &intent_sha256)?;
                            ensure!(
                                outcome
                                    .observed
                                    .resolution_quarantines
                                    .last()
                                    .is_some_and(|coordinate| coordinate.ordinal == *ordinal),
                                "scheduler resolution outcome omitted its allocated R coordinate"
                            );
                            let bytes = read_signed_bytes_in(factory, runtime, outcome_name)?;
                            let hash = lillux::sha256_hex(&bytes);
                            prior_outcome[role_i] = Some(hash.clone());
                            (Some(outcome), Some(hash))
                        } else {
                            ensure!(
                                !resolution_files.keys().any(|(candidate, index, next)| {
                                    candidate == generation && *index == role_i && *next > *ordinal
                                }),
                                "pending scheduler resolution intent is not the final role ordinal"
                            );
                            (None, None)
                        };
                    candidate_coordinates.insert(intent.quarantine_filename()?);
                    resolution_slots.push(AuthenticatedResolutionSlot {
                        intent,
                        intent_sha256,
                        outcome,
                        outcome_sha256,
                    });
                    expected_ordinal += 1;
                    if files.outcome.is_none() {
                        break;
                    }
                }
            }
            for role in SchedulerRetirementRole::ALL {
                let public = match role {
                    SchedulerRetirementRole::Main => "scheduler.sqlite3",
                    SchedulerRetirementRole::Wal => "scheduler.sqlite3-wal",
                    SchedulerRetirementRole::Shm => "scheduler.sqlite3-shm",
                };
                candidate_coordinates.insert(public.to_owned());
                for suffix in ["quarantine", "purge"] {
                    let coordinate = format!(
                        "scheduler-retirement.role.{generation}.{}.{suffix}",
                        role.name()
                    );
                    candidate_coordinates.insert(coordinate.clone());
                    if role == SchedulerRetirementRole::Main {
                        candidate_coordinates.insert(format!("{coordinate}-journal"));
                    }
                }
                for ordinal in
                    0..ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE
                {
                    let coordinate = format!(
                        "scheduler-retirement.resolve.{generation}.{}.{ordinal:02}.quarantine",
                        role.name()
                    );
                    candidate_coordinates.insert(coordinate.clone());
                    if role == SchedulerRetirementRole::Main {
                        candidate_coordinates.insert(format!("{coordinate}-journal"));
                    }
                }
            }
            candidate_coordinates.insert("scheduler.sqlite3-journal".to_owned());
            ensure!(
                candidate_coordinates.len()
                    <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_RECOVERY_CANDIDATES,
                "interrupted scheduler candidate-coordinate budget exceeded"
            );
            ensure!(
                phase_records.len() + 1 + resolution_slots.len() * 2
                    <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS_PER_GENERATION,
                "interrupted scheduler generation signed-record budget exceeded"
            );
            result.push(AuthenticatedInterruptedGeneration {
                generation_intent,
                generation_intent_sha256,
                phase_records,
                phase_sha256,
                resolution_slots,
                candidate_coordinates: candidate_coordinates.into_iter().collect(),
                known_generations: Vec::new(),
            });
        }
        let known_generations = generation_intents.keys().cloned().collect::<Vec<_>>();
        for generation in &mut result {
            generation.known_generations = known_generations.clone();
        }
        validate_authenticated_recovery_graph(&result)?;
        let mut settled = terminal.clone();
        loop {
            let before = settled.len();
            for generation in &result {
                if settled.contains(&generation.generation_intent.generation) {
                    // Historical terminal evidence may coexist with a later
                    // operation's canonical source. Authenticate this whole old
                    // chain and require its noncanonical aliases absent; the
                    // actual completing permit separately checks ALL names.
                    let operation_generations = result
                        .iter()
                        .filter(|candidate| {
                            candidate.generation_intent.operation_id
                                == generation.generation_intent.operation_id
                        })
                        .map(|candidate| candidate.generation_intent.generation.clone())
                        .collect::<Vec<_>>();
                    ensure_fixed_inventory_absent(
                        runtime,
                        &generation.generation_intent,
                        &operation_generations,
                        false,
                    )?;
                    if let Some(binding) = &generation.generation_intent.recovery {
                        settled.insert(binding.parent_generation.clone());
                    }
                }
            }
            if settled.len() == before {
                break;
            }
        }
        result.retain(|generation| !settled.contains(&generation.generation_intent.generation));
        let pending_outcomes = resolution_files
            .values()
            .filter(|files| files.intent.is_some() && files.outcome.is_none())
            .count();
        ensure!(
            signed_names.len() + pending_outcomes
                <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS
                && signed_names.len() + pending_outcomes
                    <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_CLEANUP_COORDINATES,
            "interrupted scheduler recovery outcome/cleanup capacity is exhausted"
        );
        ensure!(
            signed_bytes
                .checked_add(
                    pending_outcomes
                        * ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES,
                )
                .is_some_and(|bytes| {
                    bytes <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES_TOTAL
                }),
            "interrupted scheduler recovery signed-byte capacity is exhausted"
        );
        let candidate_count = result
            .iter()
            .flat_map(|generation| generation.candidate_coordinates.iter())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        ensure!(
            candidate_count
                <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_RECOVERY_CANDIDATES,
            "interrupted scheduler campaign candidate-coordinate budget exceeded"
        );
        Ok(result)
    }

    pub(crate) fn completed_operation_generation(
        permit: &'a SchedulerResetRetirement<'_, '_>,
    ) -> Result<Option<[u8; 16]>> {
        let original = intent_for_permit(
            permit,
            permit.operation_reference()?.operation_id.clone(),
            None,
            None,
        )?;
        Self::completed_generation_for_evidence(
            permit.runtime_directory()?,
            permit.node_identity(),
            &original,
        )
    }

    fn completed_generation_for_evidence(
        runtime: &'a lillux::PinnedDirectory,
        identity: &'a NodeIdentity,
        original: &SchedulerRetirementIntent,
    ) -> Result<Option<[u8; 16]>> {
        Self::validate_admission(runtime, identity)?;
        let operation_id = &original.operation_id;
        let mut completed = None;
        let names = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        // A retired generation served an operation State has acknowledged;
        // it is never completion evidence for an active operation.
        let retired = retired_generations_in(None, runtime, identity, &names)?;
        for name in names {
            let display = name.to_string_lossy();
            let Some(generation) = display.strip_prefix(INTENT_PREFIX) else {
                continue;
            };
            if retired.contains_key(generation) {
                continue;
            }
            let intent: SchedulerRetirementIntent = read_signed(runtime, &display, identity)?;
            if &intent.operation_id != operation_id {
                continue;
            }
            intent.validate_same_operation(original)?;
            let records = load_record_chain(runtime, identity, generation)?;
            if records.len() != 28 {
                continue;
            }
            ensure!(
                completed.is_none(),
                "original scheduler operation has multiple terminal generations"
            );
            completed = Some(parse_generation(generation)?);
        }
        Ok(completed)
    }

    pub(crate) fn begin(
        permit: &'a SchedulerResetRetirement<'_, '_>,
        plan: SchedulerRetirementStoppedPlan,
    ) -> Result<Self> {
        let runtime = permit.runtime_directory()?;
        let intent = intent_for_permit(
            permit,
            permit.operation_reference()?.operation_id.clone(),
            None,
            Some(plan),
        )?;
        Self::begin_with_intent(runtime, permit.node_identity(), intent)
    }

    pub(crate) fn resume_original_stopped(
        permit: &'a SchedulerResetRetirement<'_, '_>,
        evidence: &AuthenticatedInterruptedGeneration,
    ) -> Result<Self> {
        Self::validate_for_permit(&evidence.generation_intent, permit)?;
        ensure!(
            evidence.generation_intent.intake_mode == "stopped-original"
                && evidence.generation_intent.recovery.is_none()
                && evidence.phase_records.len() <= 9
                && evidence.resolution_slots.is_empty(),
            "original stopped resume is outside its authenticated intake-only prefix"
        );
        let intent = evidence.generation_intent.clone();
        ensure!(
            intent
                .stopped_plan
                .as_ref()
                .context("original stopped plan absent")?
                .generations
                == evidence.known_generations,
            "original stopped retry fixed generation inventory changed"
        );
        validate_stopped_intake_prefix(&intent, &evidence.phase_records)?;
        let runtime = permit.runtime_directory()?;
        Self::reserve_pre_effect_capacity(runtime, Some(permit.node_identity()), false)?;
        let identity = permit.node_identity();
        let current_records = load_record_chain(runtime, identity, &intent.generation)?;
        ensure!(
            current_records == evidence.phase_records,
            "original stopped retry phase prefix changed"
        );
        let hashes = current_records
            .iter()
            .map(|record| {
                read_signed_bytes(runtime, &phase_name(&intent.generation, record.sequence))
                    .map(|bytes| lillux::sha256_hex(&bytes))
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            hashes == evidence.phase_sha256,
            "original stopped retry signed phase hashes changed"
        );
        let retained: SchedulerRetirementIntent = read_signed(
            runtime,
            &format!("{INTENT_PREFIX}{}", intent.generation),
            identity,
        )?;
        ensure!(
            retained == intent,
            "original stopped intent changed before intake retry"
        );
        Ok(Self {
            runtime,
            identity,
            generation: intent.generation.clone(),
            runtime_identity: intent.runtime_identity.clone(),
            generation_intent: intent,
            previous_hash: evidence.phase_sha256.last().cloned(),
            next_sequence: u8::try_from(evidence.phase_records.len())?,
            next_resolution_ordinal: [0; 3],
            pending_resolution: std::array::from_fn(|_| None),
            previous_resolution_outcome: std::array::from_fn(|_| None),
            pending_resolution_outcome: std::array::from_fn(|_| None),
            resolution_record_count: 0,
            resolution_quarantine_count: 0,
        })
    }

    fn begin_with_intent(
        runtime: &'a lillux::PinnedDirectory,
        identity: &'a NodeIdentity,
        intent: SchedulerRetirementIntent,
    ) -> Result<Self> {
        // Capacity is checked before publishing an intent or opening the
        // scheduler owner. A full journal must never fail only after role
        // effects have started.
        Self::preflight(runtime, identity)
            .context("validate scheduler retirement journal before reserving capacity")?;
        Self::reserve_pre_effect_capacity(runtime, Some(identity), true)?;
        // The inventory a new plan binds excludes generations whose
        // acknowledged evidence is being removed, exactly as
        // `authenticated_generation_ids` does when the plan is built.
        let inventory = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        let retired = retired_generations_in(None, runtime, identity, &inventory)?;
        let mut generations = std::collections::BTreeSet::new();
        for name in inventory {
            let display = name.to_string_lossy();
            if let Some(generation) = display.strip_prefix(INTENT_PREFIX) {
                validate_generation(generation)?;
                generations.insert(generation.to_owned());
            } else if let Some(rest) = display.strip_prefix(PHASE_PREFIX) {
                let (generation, _) = parse_phase_name(rest)?;
                generations.insert(generation);
            }
        }
        generations.retain(|generation| !retired.contains_key(generation));
        ensure!(
            generations.len() < MAX_GENERATIONS,
            "scheduler retirement journal has no reserved generation capacity; terminal recovery is required before a new reset"
        );
        let generation = intent.generation.clone();
        let runtime_identity = intent.runtime_identity.clone();
        intent.validate()?;
        let mut expected_generations = generations.into_iter().collect::<Vec<_>>();
        expected_generations.push(generation.clone());
        expected_generations.sort();
        expected_generations.dedup();
        ensure!(
            intent.intake_mode == "stopped-original"
                && intent.recovery.is_none()
                && intent.stopped_intake_plan()?.generations == expected_generations,
            "normal stopped publication lacks complete authenticated generation inventory"
        );
        let names = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        let mut signed_count = 0usize;
        let mut signed_bytes = 0usize;
        for name in &names {
            let display = name.to_string_lossy();
            if is_campaign_signed_name(&display) {
                signed_count += 1;
                signed_bytes = signed_bytes
                    .checked_add(read_signed_bytes(runtime, &display)?.len())
                    .context("scheduler initial signed-byte total overflow")?;
            }
        }
        let reserved =
            ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS_PER_GENERATION;
        ensure!(signed_count.checked_add(reserved).is_some_and(|count|
            count <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS
                && count <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_CLEANUP_COORDINATES)
            && signed_bytes.checked_add(reserved * ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES)
                .is_some_and(|bytes| bytes <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES_TOTAL)
            && names.len().checked_add(reserved * 2)
                .and_then(|count| count.checked_add(ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_COORDINATES_PER_GENERATION + 2))
                .is_some_and(|count| count <= MAX_DIRECTORY_ENTRIES),
            "scheduler initial operation lacks aggregate reserved capacity before native effects");
        publish_signed(
            runtime,
            identity,
            &format!("{INTENT_PREFIX}{generation}"),
            &intent,
        )?;
        Ok(Self {
            runtime,
            identity,
            generation,
            runtime_identity,
            generation_intent: intent,
            previous_hash: None,
            next_sequence: 0,
            next_resolution_ordinal: [0; 3],
            pending_resolution: std::array::from_fn(|_| None),
            previous_resolution_outcome: std::array::from_fn(|_| None),
            pending_resolution_outcome: std::array::from_fn(|_| None),
            resolution_record_count: 0,
            resolution_quarantine_count: 0,
        })
    }

    /// Publish a new-generation intent that authenticates the entire bounded
    /// interrupted-parent lineage and exact stopped source plan before native
    /// current-object acquisition begins.
    pub(crate) fn begin_recovery(
        permit: &'a SchedulerResetRetirement<'_, '_>,
        parent: &AuthenticatedInterruptedGeneration,
        stopped_plan: SchedulerRetirementStoppedPlan,
    ) -> Result<Self> {
        let runtime = permit.runtime_directory()?;
        let identity = permit.node_identity();
        Self::validate_for_permit(&parent.generation_intent, permit)?;
        stopped_plan.validate()?;
        let runtime_identity = serde_json::to_string(&runtime.identity()?)?;
        ensure!(
            stopped_plan.runtime_identity == runtime_identity
                && parent.generation_intent.owner_fingerprint == identity.fingerprint()
                && parent.generation_intent.runtime_identity == runtime_identity
                && parent.known_generations.len() <= MAX_GENERATIONS,
            "stopped recovery plan differs from current runtime/signer or exceeds generation capacity"
        );
        let generation = stopped_plan.generation.clone();
        Self::reserve_pre_effect_capacity(
            runtime,
            Some(identity),
            runtime
                .entry_no_follow(OsStr::new(&format!("{INTENT_PREFIX}{generation}")))?
                .is_none(),
        )?;
        let recovery = recovery_binding(parent, stopped_plan)?;
        let intent = intent_for_permit(permit, generation.clone(), Some(recovery), None)?;
        intent.validate_same_operation(&parent.generation_intent)?;
        intent.validate()?;
        let intent_name = format!("{INTENT_PREFIX}{generation}");
        if runtime.entry_no_follow(OsStr::new(&intent_name))?.is_some() {
            let existing: SchedulerRetirementIntent = read_signed(runtime, &intent_name, identity)?;
            ensure!(
                existing == intent,
                "existing scheduler recovery child intent differs from the authenticated retry plan"
            );
            let names = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
            let mut sequences = names
                .iter()
                .filter_map(|name| {
                    let display = name.to_string_lossy();
                    display
                        .strip_prefix(PHASE_PREFIX)
                        .and_then(|rest| parse_phase_name(rest).ok())
                        .filter(|(candidate, _)| candidate == &generation)
                        .map(|(_, sequence)| sequence)
                })
                .collect::<Vec<_>>();
            sequences.sort_unstable();
            ensure!(
                sequences
                    .iter()
                    .copied()
                    .eq(0..u8::try_from(sequences.len())?)
                    && sequences.len() <= 9,
                "existing scheduler recovery child phase prefix is not bounded"
            );
            let mut previous_hash = None;
            for sequence in &sequences {
                let name = phase_name(&generation, *sequence);
                let record: SchedulerRetirementRecord = read_signed(runtime, &name, identity)?;
                record.validate()?;
                let mut expected_identity = intent.stopped_intake_plan()?.roles
                    [role_index(record.role)]
                .current
                .clone();
                if *sequence < 6 {
                    if let Some(identity) = &mut expected_identity {
                        identity.content_sha256 = None;
                    }
                }
                let bytes = read_signed_bytes(runtime, &name)?;
                ensure!(
                    record.generation == generation
                        && record.sequence == *sequence
                        && record.owner_fingerprint == identity.fingerprint()
                        && record.runtime_identity == runtime_identity
                        && record.previous_sha256 == previous_hash
                        && record.role_identity == expected_identity,
                    "existing scheduler recovery child phase differs from its authenticated parent prefix"
                );
                previous_hash = Some(lillux::sha256_hex(&bytes));
            }
            let decoded = load_record_chain(runtime, identity, &generation)?;
            validate_phase_prefix(&decoded)?;
            validate_stopped_intake_prefix(&intent, &decoded)?;
            let names = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
            let mut signed_count = 0usize;
            let mut signed_bytes = 0usize;
            for name in &names {
                let display = name.to_string_lossy();
                if is_campaign_signed_name(&display) {
                    signed_count += 1;
                    signed_bytes = signed_bytes
                        .checked_add(read_signed_bytes(runtime, &display)?.len())
                        .context("scheduler recovery signed-byte total overflow")?;
                }
            }
            let remaining =
                ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS_PER_GENERATION
                    .checked_sub(1 + sequences.len())
                    .context("scheduler recovery child record reservation underflow")?;
            ensure!(
                signed_count
                    .checked_add(remaining)
                    .is_some_and(|count| count
                        <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS)
                    && signed_count + remaining
                        <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_CLEANUP_COORDINATES,
                "scheduler recovery retry lacks aggregate signed-record capacity"
            );
            ensure!(
                signed_bytes
                    .checked_add(
                        remaining * ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES
                    )
                    .is_some_and(|bytes| bytes
                        <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES_TOTAL),
                "scheduler recovery retry lacks aggregate signed-byte capacity"
            );
            ensure!(
                names.len()
                    .checked_add(remaining * 2)
                    .and_then(|count| count.checked_add(ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_COORDINATES_PER_GENERATION))
                    .is_some_and(|count| count <= MAX_DIRECTORY_ENTRIES),
                "scheduler recovery retry lacks namespace capacity"
            );
            return Ok(Self {
                runtime,
                identity,
                generation,
                runtime_identity,
                generation_intent: intent,
                previous_hash,
                next_sequence: u8::try_from(sequences.len())?,
                next_resolution_ordinal: [0; 3],
                pending_resolution: std::array::from_fn(|_| None),
                previous_resolution_outcome: std::array::from_fn(|_| None),
                pending_resolution_outcome: std::array::from_fn(|_| None),
                resolution_record_count: 0,
                resolution_quarantine_count: 0,
            });
        }
        let names = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        let mut current_signed_count = 0usize;
        let mut current_signed_bytes = 0usize;
        for name in &names {
            let display = name.to_string_lossy();
            if is_campaign_signed_name(&display) {
                current_signed_count += 1;
                current_signed_bytes = current_signed_bytes
                    .checked_add(read_signed_bytes(runtime, &display)?.len())
                    .context("scheduler recovery signed-byte total overflow")?;
            }
        }
        let pending_outcomes = parent
            .resolution_slots
            .iter()
            .filter(|slot| slot.outcome.is_none())
            .count();
        let new_records =
            ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS_PER_GENERATION;
        let reserved_records = current_signed_count
            .checked_add(pending_outcomes)
            .and_then(|count| count.checked_add(new_records))
            .context("scheduler recovery record reservation overflow")?;
        ensure!(
            reserved_records <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS
                && reserved_records
                    <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_CLEANUP_COORDINATES,
            "scheduler recovery lacks aggregate signed-record/cleanup capacity before intake"
        );
        let reserved_bytes = current_signed_bytes
            .checked_add(
                pending_outcomes * ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES,
            )
            .and_then(|bytes| {
                bytes.checked_add(
                    new_records * ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES,
                )
            })
            .context("scheduler recovery signed-byte reservation overflow")?;
        ensure!(
            reserved_bytes <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES_TOTAL,
            "scheduler recovery lacks aggregate signed-byte capacity before intake"
        );
        let reserved_entries = names
            .len()
            .checked_add(new_records * 2)
            .and_then(|entries| {
                entries.checked_add(
                    ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_COORDINATES_PER_GENERATION,
                )
            })
            .and_then(|entries| entries.checked_add(2))
            .context("scheduler recovery namespace reservation overflow")?;
        ensure!(
            reserved_entries <= MAX_DIRECTORY_ENTRIES,
            "scheduler recovery lacks reserved namespace cleanup/publication capacity before intake"
        );
        publish_signed(
            runtime,
            identity,
            &format!("{INTENT_PREFIX}{generation}"),
            &intent,
        )?;
        Ok(Self {
            runtime,
            identity,
            generation,
            runtime_identity,
            generation_intent: intent,
            previous_hash: None,
            next_sequence: 0,
            next_resolution_ordinal: [0; 3],
            pending_resolution: std::array::from_fn(|_| None),
            previous_resolution_outcome: std::array::from_fn(|_| None),
            pending_resolution_outcome: std::array::from_fn(|_| None),
            resolution_record_count: 0,
            resolution_quarantine_count: 0,
        })
    }

    pub(crate) fn complete_generation(
        permit: &'a SchedulerResetRetirement<'_, '_>,
        generation_bytes: [u8; 16],
    ) -> Result<usize> {
        let runtime = permit.runtime_directory()?;
        let identity = permit.node_identity();
        let generation = generation_bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let intent: SchedulerRetirementIntent =
            read_signed(runtime, &format!("{INTENT_PREFIX}{generation}"), identity)?;
        Self::validate_for_permit(&intent, permit)?;
        let mut artifact_count = Self::preflight(runtime, identity)?;
        let records = load_record_chain(runtime, identity, &generation)?;
        if records.len() == 28 {
            let generations = Self::authenticated_generation_ids(runtime, identity)?;
            ensure_fixed_inventory_absent(runtime, &intent, &generations, true)?;
            runtime.sync()?;
            return Ok(artifact_count);
        }
        ensure!(
            records.len() == 27
                && records.last().is_some_and(|record| {
                    record.phase == ryeos_scheduler_contract::SchedulerRetirementPhase::Released
                }),
            "scheduler retirement cannot complete before all role releases are acknowledged"
        );
        let generations = Self::authenticated_generation_ids(runtime, identity)?;
        ensure_fixed_inventory_absent(runtime, &intent, &generations, true)?;
        runtime.sync()?;
        let previous_name = phase_name(&generation, 26);
        let previous_bytes = read_signed_bytes(runtime, &previous_name)?;
        let runtime_identity = records[26].runtime_identity.clone();
        let mut journal = Self {
            runtime,
            identity,
            generation,
            runtime_identity,
            generation_intent: intent,
            previous_hash: Some(lillux::sha256_hex(&previous_bytes)),
            next_sequence: 27,
            next_resolution_ordinal: [0; 3],
            pending_resolution: std::array::from_fn(|_| None),
            previous_resolution_outcome: std::array::from_fn(|_| None),
            pending_resolution_outcome: std::array::from_fn(|_| None),
            resolution_record_count: 0,
            resolution_quarantine_count: 0,
        };
        journal.append(
            SchedulerRetirementRole::Main,
            ryeos_scheduler_contract::SchedulerRetirementPhase::Complete,
            records[24].role_identity.clone(),
        )?;
        artifact_count = Self::preflight(runtime, identity)?;
        Ok(artifact_count)
    }

    /// Finish, under the active reset's exclusive authority and before any
    /// new generation begins, the cleanup an earlier acknowledged operation
    /// left: every authenticated retired lineage, and any fire-history
    /// retirement an earlier operation began. Returns how many entries were
    /// removed.
    ///
    /// Both are sound to finish early. A retired-evidence record exists only
    /// after State acknowledged its operation, so nothing still needs those
    /// records; and fire history is retired by every completed reset, this
    /// one included, so finishing an earlier begun retirement only does now
    /// what this operation's completion would. Afterwards the namespace holds
    /// no record the native stopped intake would refuse as unknown. When only
    /// this operation's own fire-retirement intent exists it is left to its
    /// own completion; when an earlier operation's intent exists too, the
    /// single retirement here finishes both, which is the same removal this
    /// operation's completion would make.
    pub(crate) fn finish_acknowledged_leftovers(
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
        operation_id: &str,
        operation_sha256: &str,
    ) -> Result<usize> {
        let names = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        // Authenticates every retired record and the records it lists.
        Self::scan_interrupted_evidence(runtime, identity)?;
        // Only an acknowledged operation's evidence is retired, so no record
        // may name the operation now active; checked before any removal.
        for name in &names {
            let display = name.to_string_lossy();
            ensure!(
                display.strip_prefix(RETIRED_PREFIX) != Some(operation_id),
                "retired scheduler evidence record {display} names the active operation; \
                 every scheduler record is kept"
            );
        }
        let mut removed = 0usize;
        for name in &names {
            let display = name.to_string_lossy();
            if display.starts_with(RETIRED_PREFIX) {
                removed += finish_retired_operation(runtime, identity, &display)?;
            }
        }
        let earlier_fire_retirement = names.iter().any(|name| {
            name.to_string_lossy()
                .strip_prefix(FIRES_PREFIX)
                .is_some_and(|id| id != operation_id)
        });
        if earlier_fire_retirement {
            removed += retire_fire_history(runtime, identity, operation_id, operation_sha256)?;
        }
        Ok(removed)
    }

    /// Remove this node's completed scheduler retirement journals, returning
    /// how many signed records were removed.
    ///
    /// The caller must prove that State has acknowledged the reset operation
    /// and that no operation is active, and must hold the offline State lock
    /// and the runtime namespace lock exclusively for the whole call. Until
    /// that acknowledgement the journals are the only evidence a resumed
    /// operation has that its scheduler roles were retired, so they are never
    /// removed earlier. With no operation active every generation must be
    /// settled; an interrupted one refuses and keeps all evidence.
    ///
    /// Each operation's lineage is retired in two durable steps: a signed
    /// [`RetiredOperationEvidence`] record binding its terminal `Complete`
    /// record, then removal of every listed generation's records followed by
    /// that retirement record. A process that dies between or during those
    /// steps leaves the retirement record, which every scanner honours and
    /// the next acknowledged reset finishes first.
    pub(crate) fn retire_acknowledged_evidence(
        runtime: &'a lillux::PinnedDirectory,
        identity: &'a NodeIdentity,
    ) -> Result<usize> {
        ensure!(
            Self::scan_interrupted_evidence(runtime, identity)?.is_empty(),
            "an acknowledged reset found interrupted scheduler retirement evidence; it keeps \
             every scheduler journal record"
        );
        Self::validate_admission(runtime, identity)?;
        let runtime_identity = serde_json::to_string(&runtime.identity()?)?;
        let mut removed = 0usize;
        for name in runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)? {
            let display = name.to_string_lossy();
            if display.starts_with(RETIRED_PREFIX) {
                removed += finish_retired_operation(runtime, identity, &display)?;
            }
        }
        let mut lineages = BTreeMap::<String, Vec<String>>::new();
        for generation in Self::authenticated_generation_ids(runtime, identity)? {
            let intent: SchedulerRetirementIntent =
                read_signed(runtime, &format!("{INTENT_PREFIX}{generation}"), identity)?;
            lineages
                .entry(intent.operation_id)
                .or_default()
                .push(generation);
        }
        for (operation_id, mut generations) in lineages {
            generations.sort();
            let mut terminal = None;
            for generation in &generations {
                let records = load_record_chain(runtime, identity, generation)?;
                if records.len() == usize::from(COMPLETE_SEQUENCE) + 1 {
                    validate_completed_flow(&records)?;
                    ensure!(
                        terminal.replace(generation.clone()).is_none(),
                        "acknowledged scheduler operation has multiple terminal generations"
                    );
                }
            }
            let terminal_generation = terminal.context(
                "acknowledged scheduler operation has no terminal generation to retire",
            )?;
            let complete = read_signed_bytes(
                runtime,
                &phase_name(&terminal_generation, COMPLETE_SEQUENCE),
            )?;
            let record = RetiredOperationEvidence {
                schema: 1,
                purpose: RETIRED_PURPOSE.to_owned(),
                operation_id: operation_id.clone(),
                owner_fingerprint: identity.fingerprint().to_owned(),
                runtime_identity: runtime_identity.clone(),
                terminal_generation,
                terminal_sha256: lillux::sha256_hex(&complete),
                generations,
            };
            let name = format!("{RETIRED_PREFIX}{operation_id}");
            record.validate(&name, identity, &runtime_identity)?;
            publish_signed(runtime, identity, &name, &record)?;
            runtime
                .sync()
                .context("sync the retired scheduler evidence record before removal")?;
            removed += finish_retired_operation(runtime, identity, &name)?;
        }
        Ok(removed)
    }

    fn validate_resolution_phase(
        &self,
        intent: &SchedulerRetirementResolutionIntent,
    ) -> Result<()> {
        let records = load_record_chain(self.runtime, self.identity, &self.generation)?;
        let record = records
            .last()
            .context("resolution lacks its authenticated phase")?;
        ensure!(
            record.sequence == resolution_authorizing_sequence(intent.role, intent.action)
                && record.role == intent.role
                && lillux::sha256_hex(&read_signed_bytes(
                    self.runtime,
                    &phase_name(&self.generation, record.sequence)
                )?) == intent.parent_journal_sha256
                && resolution_identity_matches_phase(
                    record.role_identity.as_ref(),
                    intent.expected.original.as_ref()
                ),
            "live resolution differs from authenticated SQL-drained phase/native identity"
        );
        Ok(())
    }

    fn validate_retained_generation(&self) -> Result<SchedulerRetirementIntent> {
        let intent: SchedulerRetirementIntent = read_signed(
            self.runtime,
            &format!("{INTENT_PREFIX}{}", self.generation),
            self.identity,
        )?;
        ensure!(
            intent == self.generation_intent,
            "live scheduler generation changed its retained original operation evidence"
        );
        intent.validate()?;
        Ok(intent)
    }

    pub(crate) fn append(
        &mut self,
        role: SchedulerRetirementRole,
        phase: ryeos_scheduler_contract::SchedulerRetirementPhase,
        role_identity: Option<SchedulerRetirementRoleIdentity>,
    ) -> Result<()> {
        self.validate_retained_generation()?;
        if self.next_sequence < 9 {
            let index = usize::from(self.next_sequence % 3);
            let expected_phase = match self.next_sequence / 3 {
                0 => ryeos_scheduler_contract::SchedulerRetirementPhase::Captured,
                1 => ryeos_scheduler_contract::SchedulerRetirementPhase::SqlDrainAuthorized,
                _ => ryeos_scheduler_contract::SchedulerRetirementPhase::SqlDrained,
            };
            let mut expected = self.generation_intent.stopped_intake_plan()?.roles[index]
                .current
                .clone();
            if self.next_sequence < 6 {
                if let Some(identity) = &mut expected {
                    identity.content_sha256 = None;
                }
            }
            ensure!(
                role == SchedulerRetirementRole::ALL[index]
                    && phase == expected_phase
                    && role_identity == expected,
                "stopped intake acknowledgement differs from signed copied/current plan"
            );
        }
        ensure!(
            usize::from(self.next_sequence) < MAX_RECORDS_PER_GENERATION,
            "scheduler retirement record limit exceeded"
        );
        let record = SchedulerRetirementRecord {
            schema: 1,
            purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
            generation: self.generation.clone(),
            sequence: self.next_sequence,
            previous_sha256: self.previous_hash.clone(),
            owner_fingerprint: self.identity.fingerprint().to_owned(),
            runtime_identity: self.runtime_identity.clone(),
            role,
            phase,
            role_identity,
        };
        record.validate()?;
        #[cfg(feature = "crash-qualification-test-support")]
        reach_reset_crash_cut(ResetCrashCut::before_publication(phase), role);
        let name = phase_name(&self.generation, self.next_sequence);
        let signed = publish_signed(self.runtime, self.identity, &name, &record)?;
        self.previous_hash = Some(lillux::sha256_hex(&signed));
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .context("scheduler retirement record sequence overflow")?;
        #[cfg(feature = "crash-qualification-test-support")]
        reach_reset_crash_cut(ResetCrashCut::after_publication(phase), role);
        Ok(())
    }

    /// Durably reserve one exact-observation action before asking the native
    /// owner to settle it. The intent and its outcome slot are both budgeted
    /// before publication; the outcome is a retryable acknowledgement only.
    pub(crate) fn authorize_resolution(
        &mut self,
        role: SchedulerRetirementRole,
        host_expected: lillux_sqlite::SqliteRoleResolutionObservation,
        action: SchedulerRetirementResolutionAction,
    ) -> Result<ResolutionReservation> {
        // The signed policy view is derived exactly once from the copied
        // observation that will be passed back to the retained native owner.
        let expected = native_resolution_projection(&host_expected)?;
        let generation_intent: SchedulerRetirementIntent = read_signed(
            self.runtime,
            &format!("{INTENT_PREFIX}{}", self.generation),
            self.identity,
        )?;
        ensure!(
            generation_intent.generation == self.generation
                && generation_intent.runtime_identity == self.runtime_identity
                && generation_intent.owner_fingerprint == self.identity.fingerprint(),
            "live scheduler resolution generation differs from retained journal authority"
        );
        ensure!(
            generation_intent == self.generation_intent,
            "live scheduler generation changed its retained original operation evidence"
        );
        let index = role_index(role);
        if let Some(pending) = self.pending_resolution[index].as_ref() {
            pending.intent.validate_for_generation(&generation_intent)?;
            self.validate_resolution_phase(&pending.intent)?;
            ensure!(
                pending.intent.expected == expected
                    && pending.intent.action == action
                    && pending.host_expected == host_expected,
                "retry changed a previously reserved resolution observation/action"
            );
            if pending.intent_is_durable {
                return Ok(pending.clone());
            }
            let intent_name = pending.intent.intent_filename()?;
            publish_signed_bytes(
                self.runtime,
                self.identity,
                &intent_name,
                &pending.intent,
                &pending.signed_intent,
            )?;
            let mut durable = pending.clone();
            durable.intent_is_durable = true;
            self.pending_resolution[index] = Some(durable.clone());
            self.resolution_record_count += 1;
            return Ok(durable);
        }
        let ordinal = self.next_resolution_ordinal[index];
        ensure!(
            ordinal < ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE,
            "scheduler role resolution budget exhausted before host effect"
        );
        ensure!(
            self.resolution_record_count + 2
                <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_RECORDS_PER_GENERATION,
            "scheduler resolution record budget exhausted before host effect"
        );
        ensure!(
            self.signed_record_count() + 2
                <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS_PER_GENERATION,
            "scheduler generation signed-record budget exhausted before host effect"
        );
        let campaign_names = self.runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        let current_signed_names = campaign_names
            .iter()
            .filter(|name| {
                let name = name.to_string_lossy();
                is_campaign_signed_name(&name)
            })
            .collect::<Vec<_>>();
        let current_signed_count = current_signed_names.len();
        let mut current_signed_bytes = 0usize;
        for name in current_signed_names {
            let display = name.to_string_lossy();
            current_signed_bytes = current_signed_bytes
                .checked_add(read_signed_bytes(self.runtime, display.as_ref())?.len())
                .context("scheduler campaign signed-byte total overflow")?;
        }
        ensure!(
            current_signed_count + 2
                <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS,
            "scheduler campaign signed-record capacity exhausted before host effect"
        );
        ensure!(
            current_signed_count + 2
                <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_CLEANUP_COORDINATES,
            "scheduler campaign cleanup-coordinate capacity exhausted before host effect"
        );
        ensure!(
            current_signed_bytes
                .checked_add(2 * ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES)
                .is_some_and(|bytes| bytes
                    <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES_TOTAL),
            "scheduler campaign signed-byte capacity exhausted before host effect"
        );
        ensure!(
            expected.generation == self.generation
                && expected.role == role
                && expected.runtime_identity == self.runtime_identity
                && expected.resolution_quarantines.len() == usize::from(ordinal)
                && expected.resolutions.len() == usize::from(ordinal),
            "scheduler resolution observation differs from the reserved generation/ordinal"
        );
        let expected_generation = parse_generation(&self.generation)?;
        ensure!(
            host_expected.generation == expected_generation
                && host_expected.role == host_role(role),
            "native resolution observation differs from the reserved generation/role"
        );
        let parent_journal_sha256 = self
            .previous_hash
            .clone()
            .context("scheduler resolution has no durable parent phase")?;
        let intent = SchedulerRetirementResolutionIntent {
            schema: 2,
            operation_id: generation_intent.operation_id.clone(),
            operation_sha256: generation_intent.operation_sha256.clone(),
            purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE.to_owned(),
            generation: self.generation.clone(),
            owner_fingerprint: self.identity.fingerprint().to_owned(),
            runtime_identity: self.runtime_identity.clone(),
            parent_journal_sha256,
            role,
            ordinal,
            previous_outcome_sha256: self.previous_resolution_outcome[index].clone(),
            expected,
            action,
        };
        intent.validate_for_generation(&generation_intent)?;
        self.validate_resolution_phase(&intent)?;
        let intent_name = intent.intent_filename()?;
        let outcome_name = intent.outcome_filename()?;
        let quarantine_name = intent.quarantine_filename()?;
        let intent_cleanup = cleanup_name(&intent_name);
        let outcome_cleanup = cleanup_name(&outcome_name);
        let names = self.runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        // Keep capacity for the intent, outcome, each planned-cleanup name,
        // the fixed R coordinate, and one in-flight atomic publication file.
        ensure!(
            names.len() + 6 <= MAX_DIRECTORY_ENTRIES,
            "scheduler retirement namespace has no reserved resolution capacity"
        );
        for reserved in [
            intent_name.as_str(),
            outcome_name.as_str(),
            quarantine_name.as_str(),
            intent_cleanup.as_str(),
            outcome_cleanup.as_str(),
        ] {
            ensure!(
                self.runtime
                    .entry_no_follow(OsStr::new(reserved))?
                    .is_none(),
                "scheduler resolution reserved coordinate is already occupied"
            );
        }
        let signed_intent = sign_record_bytes(self.identity, &intent)?;
        let reservation = ResolutionReservation {
            intent,
            signed_intent_sha256: lillux::sha256_hex(&signed_intent),
            signed_intent,
            host_expected,
            intent_is_durable: false,
        };
        self.next_resolution_ordinal[index] = ordinal
            .checked_add(1)
            .context("scheduler resolution ordinal overflow")?;
        self.pending_resolution[index] = Some(reservation.clone());
        let name = reservation.intent.intent_filename()?;
        publish_signed_bytes(
            self.runtime,
            self.identity,
            &name,
            &reservation.intent,
            &reservation.signed_intent,
        )?;
        let mut durable = reservation;
        durable.intent_is_durable = true;
        self.pending_resolution[index] = Some(durable.clone());
        self.resolution_record_count += 1;
        Ok(durable)
    }

    /// Publish an immutable outcome for a previously acknowledged intent.
    /// Repeating this call after an uncertain journal write reuses the same
    /// bytes and never calls the native effect again.
    pub(crate) fn acknowledge_resolution(
        &mut self,
        reservation: &ResolutionReservation,
        outcome: &SchedulerRetirementResolutionOutcome,
    ) -> Result<()> {
        reservation
            .intent
            .validate_for_generation(&self.validate_retained_generation()?)?;
        self.validate_resolution_phase(&reservation.intent)?;
        let index = role_index(reservation.intent.role);
        ensure!(
            self.pending_resolution[index].as_ref() == Some(reservation),
            "scheduler resolution outcome has no matching pending intent"
        );
        ensure!(
            reservation.intent_is_durable,
            "scheduler resolution outcome cannot precede durable intent publication"
        );
        if self.pending_resolution_outcome[index].is_none() {
            outcome.validate_for_intent(&reservation.intent, &reservation.signed_intent_sha256)?;
            self.pending_resolution_outcome[index] = Some(RetainedResolutionOutcome {
                value: outcome.clone(),
                signed: sign_record_bytes(self.identity, outcome)?,
            });
        }
        self.publish_retained_resolution_outcome(reservation)
    }

    fn publish_retained_resolution_outcome(
        &mut self,
        reservation: &ResolutionReservation,
    ) -> Result<()> {
        let index = role_index(reservation.intent.role);
        let retained = self.pending_resolution_outcome[index]
            .as_ref()
            .context("scheduler resolution outcome bytes were not retained")?;
        ensure!(
            self.resolution_record_count
                < ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_RECORDS_PER_GENERATION,
            "scheduler resolution outcome record budget exhausted"
        );
        let filename = reservation.intent.outcome_filename()?;
        let cleanup = cleanup_name(&filename);
        ensure!(
            self.runtime
                .entry_no_follow(OsStr::new(&cleanup))?
                .is_none(),
            "scheduler resolution outcome cleanup coordinate is already occupied"
        );
        let signed = publish_signed_bytes(
            self.runtime,
            self.identity,
            &filename,
            &retained.value,
            &retained.signed,
        )?;
        self.previous_resolution_outcome[index] = Some(lillux::sha256_hex(&signed));
        if retained
            .value
            .observed
            .resolution_quarantines
            .last()
            .is_some_and(|item| {
                !matches!(
                    &item.coordinate,
                    ryeos_scheduler_contract::SchedulerRetirementObservedCoordinate::Absent
                )
            })
        {
            self.resolution_quarantine_count += 1;
        }
        self.pending_resolution[index] = None;
        self.pending_resolution_outcome[index] = None;
        self.resolution_record_count += 1;
        Ok(())
    }

    pub(crate) fn artifact_count(&self) -> usize {
        self.signed_record_count() + self.resolution_quarantine_count
    }

    pub(crate) fn next_sequence(&self) -> u8 {
        self.next_sequence
    }

    fn signed_record_count(&self) -> usize {
        usize::from(self.next_sequence) + 1 + self.resolution_record_count
    }
}

fn role_index(role: SchedulerRetirementRole) -> usize {
    match role {
        SchedulerRetirementRole::Main => 0,
        SchedulerRetirementRole::Wal => 1,
        SchedulerRetirementRole::Shm => 2,
    }
}

fn host_role(role: SchedulerRetirementRole) -> lillux_sqlite::SqliteRole {
    match role {
        SchedulerRetirementRole::Main => lillux_sqlite::SqliteRole::Main,
        SchedulerRetirementRole::Wal => lillux_sqlite::SqliteRole::Wal,
        SchedulerRetirementRole::Shm => lillux_sqlite::SqliteRole::Shm,
    }
}

fn parse_generation(value: &str) -> Result<[u8; 16]> {
    validate_generation(value)?;
    let mut generation = [0; 16];
    for (index, byte) in generation.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(generation)
}

pub(crate) fn native_resolution_projection(
    native: &lillux_sqlite::SqliteRoleResolutionObservation,
) -> Result<SchedulerRetirementResolutionObservation> {
    use lillux::secure_fs::{
        PinnedEntryType, PlannedQuarantineDisposition as Disposition,
        PlannedQuarantineStage as Stage,
    };
    use lillux_sqlite::{
        SqliteRoleCoordinateObservation as NativeCoordinate,
        SqliteRoleMutationOperation as Operation, SqliteRoleResolutionAction as Action,
        SqliteRoleResolutionOutcome as Outcome,
    };
    use SchedulerRetirementObservedCoordinate as Coordinate;

    let coordinate = |value: &NativeCoordinate| -> Result<Coordinate> {
        Ok(match value {
            NativeCoordinate::Absent => Coordinate::Absent,
            NativeCoordinate::SameOriginal { device, inode } => Coordinate::SameOriginal {
                device: *device,
                inode: *inode,
            },
            NativeCoordinate::ForeignRegular { device, inode } => Coordinate::ForeignRegular {
                device: *device,
                inode: *inode,
            },
            NativeCoordinate::Unreadable(error) => Coordinate::Unreadable {
                error_sha256: lillux::sha256_hex(error.as_bytes()),
            },
            NativeCoordinate::NonRegular(kind) => Coordinate::NonRegular {
                kind: match kind {
                    PinnedEntryType::Directory => SchedulerRetirementNonRegularKind::Directory,
                    PinnedEntryType::Regular => {
                        anyhow::bail!("regular entry returned as non-regular")
                    }
                    PinnedEntryType::Symlink => SchedulerRetirementNonRegularKind::Symlink,
                    PinnedEntryType::CharacterDevice => {
                        SchedulerRetirementNonRegularKind::CharacterDevice
                    }
                    PinnedEntryType::BlockDevice => SchedulerRetirementNonRegularKind::BlockDevice,
                    PinnedEntryType::Fifo => SchedulerRetirementNonRegularKind::Fifo,
                    PinnedEntryType::Socket => SchedulerRetirementNonRegularKind::Socket,
                    PinnedEntryType::Other => SchedulerRetirementNonRegularKind::Other,
                },
            },
        })
    };
    let effect = |operation| match operation {
        Operation::Quarantine => SchedulerRetirementEffect::Quarantine,
        Operation::Purge => SchedulerRetirementEffect::Purge,
    };
    let failure = |value: &lillux_sqlite::SqliteRoleMutationFailureObservation| {
        SchedulerRetirementFailureFact {
            operation: effect(value.operation),
            disposition: match value.error.disposition {
                Disposition::BeforeCommit => SchedulerRetirementFailureDisposition::BeforeCommit,
                Disposition::DurabilityUncertain => {
                    SchedulerRetirementFailureDisposition::DurabilityUncertain
                }
                Disposition::NamespaceChanged => {
                    SchedulerRetirementFailureDisposition::NamespaceChanged
                }
            },
            stage: match value.error.stage {
                Stage::Unchanged => SchedulerRetirementFailureStage::Unchanged,
                Stage::RestoredUnexpected => SchedulerRetirementFailureStage::RestoredUnexpected,
                Stage::PreservedUnexpected => SchedulerRetirementFailureStage::PreservedUnexpected,
                Stage::QuarantinedOriginal => SchedulerRetirementFailureStage::QuarantinedOriginal,
                Stage::UnlinkedOriginal => SchedulerRetirementFailureStage::UnlinkedOriginal,
            },
            substep_observed: value.substep_observed,
            error_sha256: lillux::sha256_hex(value.error.message.as_bytes()),
        }
    };
    let action = |value: Action| match value {
        Action::SyncQuarantine => SchedulerRetirementResolutionAction::SyncQuarantine,
        Action::SyncRemoved => SchedulerRetirementResolutionAction::SyncRemoved,
        Action::RearmOriginal => SchedulerRetirementResolutionAction::RearmOriginal,
        Action::RearmQuarantined => SchedulerRetirementResolutionAction::RearmQuarantined,
        Action::FinishPurgeAtU => SchedulerRetirementResolutionAction::FinishPurgeAtU,
        Action::FinishPurgeAtResolution { source_ordinal } => {
            SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine {
                source_ordinal,
            }
        }
    };
    for native_failure in [
        native.original_failure.as_ref(),
        native.latest_failure.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        ensure!(
            native_failure.error.role == native.role,
            "native scheduler failure testimony names another role"
        );
    }
    for attempt in &native.resolutions {
        if let Some(error) = &attempt.effect_failure {
            ensure!(
                error.role == native.role,
                "native scheduler resolution failure names another role"
            );
        }
    }
    let original_failure = native.original_failure.as_ref().map(failure);
    let latest_failure = native.latest_failure.as_ref().map(failure);
    let attempted = latest_failure
        .as_ref()
        .context("native resolution observation lacks failure testimony")?
        .operation;
    let generation = native
        .generation
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let original = native
        .original
        .as_ref()
        .map(|value| SchedulerRetirementRoleIdentity {
            device: value.device,
            inode: value.inode,
            byte_length: value.byte_length,
            content_sha256: value.content_sha256.clone(),
        });
    let resolutions = native
        .resolutions
        .iter()
        .map(|attempt| SchedulerRetirementResolutionAttemptFact {
            ordinal: attempt.ordinal,
            action: action(attempt.action),
            substep_observed: attempt.substep.is_some(),
            original_unlink_observed: attempt.original_unlink_observed,
            outcome: attempt.outcome.map(|outcome| match outcome {
                Outcome::QuarantinedDurable => {
                    SchedulerRetirementResolutionState::QuarantinedDurable
                }
                Outcome::RemovedDurable => SchedulerRetirementResolutionState::RemovedDurable,
                Outcome::RearmedOriginal => SchedulerRetirementResolutionState::RearmedOriginal,
                Outcome::RearmedQuarantined => {
                    SchedulerRetirementResolutionState::RearmedQuarantined
                }
            }),
            effect_failure: attempt.effect_failure.as_ref().map(|error| {
                SchedulerRetirementFailureFact {
                    operation: match attempt.action {
                        Action::SyncQuarantine | Action::RearmOriginal => {
                            SchedulerRetirementEffect::Quarantine
                        }
                        Action::SyncRemoved
                        | Action::RearmQuarantined
                        | Action::FinishPurgeAtU
                        | Action::FinishPurgeAtResolution { .. } => {
                            SchedulerRetirementEffect::Purge
                        }
                    },
                    disposition: match error.disposition {
                        Disposition::BeforeCommit => {
                            SchedulerRetirementFailureDisposition::BeforeCommit
                        }
                        Disposition::DurabilityUncertain => {
                            SchedulerRetirementFailureDisposition::DurabilityUncertain
                        }
                        Disposition::NamespaceChanged => {
                            SchedulerRetirementFailureDisposition::NamespaceChanged
                        }
                    },
                    stage: match error.stage {
                        Stage::Unchanged => SchedulerRetirementFailureStage::Unchanged,
                        Stage::RestoredUnexpected => {
                            SchedulerRetirementFailureStage::RestoredUnexpected
                        }
                        Stage::PreservedUnexpected => {
                            SchedulerRetirementFailureStage::PreservedUnexpected
                        }
                        Stage::QuarantinedOriginal => {
                            SchedulerRetirementFailureStage::QuarantinedOriginal
                        }
                        Stage::UnlinkedOriginal => {
                            SchedulerRetirementFailureStage::UnlinkedOriginal
                        }
                    },
                    substep_observed: attempt.substep == Some(error.stage),
                    error_sha256: lillux::sha256_hex(error.message.as_bytes()),
                }
            }),
            error_sha256: attempt
                .error
                .as_ref()
                .map(|error| lillux::sha256_hex(error.as_bytes())),
        })
        .collect();
    let resolution_quarantines = native
        .resolution_coordinates
        .iter()
        .map(|item| {
            ensure!(
                item.name
                    == format!(
                        "scheduler-retirement.resolve.{}.{}.{:02}.quarantine",
                        generation,
                        host_role_contract(native.role).name(),
                        item.ordinal
                    ),
                "native resolution coordinate differs from the fixed R name"
            );
            Ok(SchedulerRetirementObservedQuarantine {
                ordinal: item.ordinal,
                coordinate: coordinate(&item.observation)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let stopped_plan = native
        .stopped_plan
        .as_ref()
        .map(|plan| {
            Ok::<_, anyhow::Error>(SchedulerRetirementStoppedPlan {
                generation: plan
                    .generation
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                generations: plan
                    .generations
                    .iter()
                    .map(|generation| {
                        generation
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect()
                    })
                    .collect(),
                runtime_identity: serde_json::to_string(&plan.runtime_identity)?,
                roles: std::array::from_fn(|index| {
                    let role = &plan.roles[index];
                    SchedulerRetirementStoppedRolePlan {
                        source: stopped_source_contract(&role.source),
                        current: role.current.as_ref().map(|current| {
                            SchedulerRetirementRoleIdentity {
                                device: current.device,
                                inode: current.inode,
                                byte_length: current.byte_length,
                                content_sha256: current.content_sha256.clone(),
                            }
                        }),
                    }
                }),
            })
        })
        .transpose()?;
    ensure!(
        native.stopped_coordinates.len()
            <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_RECOVERY_CANDIDATES,
        "native stopped observation exceeds the fixed inventory bound"
    );
    // Keep every original name/role fact. The contract validates the exact
    // plan-derived mapping before its serializer emits compact coordinates.
    let mut stopped_coordinates = native
        .stopped_coordinates
        .iter()
        .map(|item| {
            Ok::<_, anyhow::Error>(SchedulerRetirementObservedStoppedCoordinate {
                role: item.role.map(host_role_contract),
                name: item.name.clone(),
                coordinate: coordinate(&item.observation)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    stopped_coordinates.sort_by(|left, right| left.name.cmp(&right.name));
    let result = SchedulerRetirementResolutionObservation {
        generation,
        role: host_role_contract(native.role),
        runtime_identity: serde_json::to_string(&native.runtime_identity)?,
        epoch: native.epoch,
        barriers_held: native.barriers_held,
        recovery_required: native.recovery_required,
        attempted,
        original,
        original_link_count: native.original_link_count,
        source: coordinate(&native.source)?,
        selected_source: native.selected_source.as_ref().map(stopped_source_contract),
        stopped_plan,
        stopped_coordinates,
        quarantine_name: native.quarantine_name.clone(),
        purge_name: native.purge_name.clone(),
        original_failure,
        latest_failure,
        resolutions,
        public: coordinate(&native.public)?,
        quarantine: coordinate(&native.quarantine)?,
        purge: coordinate(&native.purge)?,
        resolution_quarantines,
    };
    result.validate()?;
    Ok(result)
}

fn resolution_authorizing_sequence(
    role: SchedulerRetirementRole,
    action: SchedulerRetirementResolutionAction,
) -> u8 {
    let base = 9 + 4 * role_index(role) as u8;
    match action {
        SchedulerRetirementResolutionAction::SyncQuarantine
        | SchedulerRetirementResolutionAction::RearmOriginal => base,
        _ => base + 2,
    }
}

// The phase preserves the real SQL-drained digest. The native observer copies
// metadata without rereading bytes; None is not a digest equality assertion.
// This join is valid only alongside authenticated phase evidence and the actual
// retained native owner (or a separately verified stopped source on restart).
fn resolution_identity_matches_phase(
    phase: Option<&SchedulerRetirementRoleIdentity>,
    observed: Option<&SchedulerRetirementRoleIdentity>,
) -> bool {
    match (phase, observed) {
        (None, None) => true,
        (Some(phase), Some(observed)) => {
            phase.device == observed.device
                && phase.inode == observed.inode
                && phase.byte_length == observed.byte_length
                && phase.content_sha256.is_some()
                && observed
                    .content_sha256
                    .as_ref()
                    .is_none_or(|digest| phase.content_sha256.as_ref() == Some(digest))
        }
        _ => false,
    }
}

pub(crate) fn authorized_role_sources(
    evidence: &AuthenticatedInterruptedGeneration,
    role: SchedulerRetirementRole,
) -> Result<(
    Option<SchedulerRetirementRoleIdentity>,
    Vec<SchedulerRetirementStoppedSource>,
    bool,
)> {
    use SchedulerRetirementStoppedSource as Source;
    ensure!(
        evidence.phase_records.len() >= 9 && evidence.phase_records.len() <= 27,
        "stopped restart requires authenticated SQL-drained role facts"
    );
    let index = role_index(role);
    let identity = evidence.phase_records[6 + index].role_identity.clone();
    let Some(identity) = identity else {
        return Ok((None, Vec::new(), true));
    };
    ensure!(
        identity.content_sha256.is_some(),
        "stopped restart lacks authenticated role digest"
    );
    let generation = evidence.generation_intent.generation.clone();
    let base = evidence.generation_intent.stopped_intake_plan()?.roles[index]
        .source
        .clone();
    ensure!(
        base != Source::Absent,
        "present drained role lacks its admitted source"
    );
    let start = 9 + 4 * index;
    if evidence.phase_records.len() > start + 3 {
        return Ok((Some(identity), Vec::new(), true));
    }
    if evidence.phase_records.len() <= start {
        return Ok((Some(identity), vec![base], false));
    }
    let quarantine = Source::Quarantine {
        generation: generation.clone(),
    };
    if evidence.phase_records.len() <= start + 1 {
        let durable = evidence
            .resolution_slots
            .iter()
            .rev()
            .find(|slot| slot.intent.role == role)
            .and_then(|slot| slot.outcome.as_ref())
            .is_some_and(|outcome| {
                outcome.state == SchedulerRetirementResolutionState::QuarantinedDurable
            });
        return Ok((
            Some(identity),
            if durable {
                vec![quarantine]
            } else {
                vec![base, quarantine]
            },
            false,
        ));
    }
    if evidence.phase_records.len() <= start + 2 {
        return Ok((Some(identity), vec![quarantine], false));
    }
    let latest = evidence
        .resolution_slots
        .iter()
        .rev()
        .find(|slot| slot.intent.role == role);
    if latest
        .and_then(|slot| slot.outcome.as_ref())
        .is_some_and(|outcome| outcome.state == SchedulerRetirementResolutionState::RemovedDurable)
    {
        return Ok((Some(identity), Vec::new(), true));
    }
    let mut sources = vec![
        quarantine,
        Source::Purge {
            generation: generation.clone(),
        },
    ];
    for slot in evidence.resolution_slots.iter().filter(|slot| {
        slot.intent.role == role
            && resolution_authorizing_sequence(role, slot.intent.action) == start as u8 + 2
    }) {
        match slot.intent.action {
            SchedulerRetirementResolutionAction::FinishPurgeAtU => {
                sources.push(Source::Resolution {
                    generation: generation.clone(),
                    ordinal: slot.intent.ordinal,
                })
            }
            SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine {
                source_ordinal,
            } => {
                sources.push(Source::Resolution {
                    generation: generation.clone(),
                    ordinal: source_ordinal,
                });
                sources.push(Source::Resolution {
                    generation: generation.clone(),
                    ordinal: slot.intent.ordinal,
                });
            }
            _ => {}
        }
    }
    Ok((Some(identity), sources, true))
}

fn validate_stopped_intake_prefix(
    intent: &SchedulerRetirementIntent,
    records: &[SchedulerRetirementRecord],
) -> Result<()> {
    let plan = intent.stopped_intake_plan()?;
    for record in records.iter().take(9) {
        let mut expected = plan.roles[role_index(record.role)].current.clone();
        if record.sequence < 6 {
            if let Some(identity) = &mut expected {
                identity.content_sha256 = None;
            }
        }
        ensure!(
            record.role_identity == expected,
            "recovery child phase differs from its authenticated current remaining source"
        );
    }
    Ok(())
}

fn validate_recovery_source_authorization(
    parent: &AuthenticatedInterruptedGeneration,
    plan: &SchedulerRetirementStoppedPlan,
) -> Result<()> {
    for role in SchedulerRetirementRole::ALL {
        let (identity, allowed, absent) = authorized_role_sources(parent, role)?;
        let selected = &plan.roles[role_index(role)];
        match &selected.source {
            SchedulerRetirementStoppedSource::Absent => ensure!(absent && selected.current.is_none(),
                "recovery child selected absence without same-operation removal authorization"),
            source => ensure!(allowed.contains(source) && selected.current == identity,
                "recovery child source differs from authenticated parent phase/resolution authority"),
        }
    }
    Ok(())
}

fn validate_recovery_edge(
    parent: &AuthenticatedInterruptedGeneration,
    child: &AuthenticatedInterruptedGeneration,
) -> Result<()> {
    let binding = child
        .generation_intent
        .recovery
        .as_ref()
        .context("recovery edge lacks binding")?;
    child
        .generation_intent
        .validate_same_operation(&parent.generation_intent)?;
    let expected_refs = parent
        .resolution_slots
        .iter()
        .map(|slot| SchedulerRetirementRecoveryResolutionRef {
            role: slot.intent.role,
            ordinal: slot.intent.ordinal,
            intent_sha256: slot.intent_sha256.clone(),
            outcome_sha256: slot.outcome_sha256.clone(),
        })
        .collect::<Vec<_>>();
    ensure!(
        binding.parent_generation == parent.generation_intent.generation
            && binding.parent_intent_sha256 == parent.generation_intent_sha256
            && binding.parent_phase_sha256 == parent.phase_sha256
            && binding.parent_resolutions == expected_refs,
        "recovery child differs from exact authenticated parent phase/resolution prefix"
    );
    validate_recovery_source_authorization(parent, &binding.stopped_plan)?;
    validate_stopped_intake_prefix(&child.generation_intent, &child.phase_records)
}

pub(crate) fn recovery_binding(
    parent: &AuthenticatedInterruptedGeneration,
    stopped_plan: SchedulerRetirementStoppedPlan,
) -> Result<SchedulerRetirementRecoveryBinding> {
    validate_recovery_source_authorization(parent, &stopped_plan)?;
    let parent_generation = parent.generation_intent.generation.clone();
    let mut expected_generations = parent.known_generations.clone();
    expected_generations.push(stopped_plan.generation.clone());
    expected_generations.sort();
    expected_generations.dedup();
    ensure!(
        expected_generations == stopped_plan.generations,
        "stopped plan generation set differs from authenticated journal inventory"
    );
    Ok(SchedulerRetirementRecoveryBinding {
        parent_generation,
        parent_intent_sha256: parent.generation_intent_sha256.clone(),
        parent_phase_sha256: parent.phase_sha256.clone(),
        parent_resolutions: parent
            .resolution_slots
            .iter()
            .map(|slot| SchedulerRetirementRecoveryResolutionRef {
                role: slot.intent.role,
                ordinal: slot.intent.ordinal,
                intent_sha256: slot.intent_sha256.clone(),
                outcome_sha256: slot.outcome_sha256.clone(),
            })
            .collect(),
        stopped_plan,
    })
}

fn stopped_source_contract(
    source: &lillux_sqlite::StoppedSqliteRoleSource,
) -> SchedulerRetirementStoppedSource {
    match source {
        lillux_sqlite::StoppedSqliteRoleSource::Absent => SchedulerRetirementStoppedSource::Absent,
        lillux_sqlite::StoppedSqliteRoleSource::Canonical => {
            SchedulerRetirementStoppedSource::Canonical
        }
        lillux_sqlite::StoppedSqliteRoleSource::Quarantine { generation } => {
            SchedulerRetirementStoppedSource::Quarantine {
                generation: generation
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            }
        }
        lillux_sqlite::StoppedSqliteRoleSource::Purge { generation } => {
            SchedulerRetirementStoppedSource::Purge {
                generation: generation
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            }
        }
        lillux_sqlite::StoppedSqliteRoleSource::Resolution {
            generation,
            ordinal,
        } => SchedulerRetirementStoppedSource::Resolution {
            generation: generation
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            ordinal: *ordinal,
        },
    }
}

fn host_role_contract(role: lillux_sqlite::SqliteRole) -> SchedulerRetirementRole {
    match role {
        lillux_sqlite::SqliteRole::Main => SchedulerRetirementRole::Main,
        lillux_sqlite::SqliteRole::Wal => SchedulerRetirementRole::Wal,
        lillux_sqlite::SqliteRole::Shm => SchedulerRetirementRole::Shm,
    }
}

fn cleanup_name(record_name: &str) -> String {
    format!(
        "scheduler-retirement.cleanup.{}.quarantine",
        lillux::sha256_hex(record_name.as_bytes())
    )
}

fn ensure_resolution_coordinate_matches(
    runtime: &lillux::PinnedDirectory,
    name: &str,
    expected: &ryeos_scheduler_contract::SchedulerRetirementObservedCoordinate,
    original: Option<&SchedulerRetirementRoleIdentity>,
    directory_saw_entry: bool,
) -> Result<()> {
    use lillux::secure_fs::PinnedEntryType as EntryType;
    use ryeos_scheduler_contract::SchedulerRetirementObservedCoordinate as Coordinate;

    let observed = runtime.entry_no_follow(OsStr::new(name))?;
    ensure!(
        observed.is_some() == directory_saw_entry,
        "scheduler resolution coordinate changed during bounded scan"
    );
    match (expected, observed) {
        (Coordinate::Absent, None) => Ok(()),
        (Coordinate::SameOriginal { device, inode }, Some(entry)) => {
            ensure!(
                entry.entry_type == EntryType::Regular
                    && entry.containing_device == *device
                    && entry.inode == *inode
                    && original.is_some_and(|identity| {
                        identity.device == *device && identity.inode == *inode
                    }),
                "scheduler resolution original coordinate changed after signed observation"
            );
            Ok(())
        }
        (Coordinate::ForeignRegular { device, inode }, Some(entry)) => {
            ensure!(
                entry.entry_type == EntryType::Regular
                    && entry.containing_device == *device
                    && entry.inode == *inode
                    && !original.is_some_and(|identity| {
                        identity.device == *device && identity.inode == *inode
                    }),
                "scheduler foreign resolution coordinate changed after signed observation"
            );
            Ok(())
        }
        (Coordinate::NonRegular { kind }, Some(entry)) => {
            let actual = match entry.entry_type {
                EntryType::Directory => {
                    ryeos_scheduler_contract::SchedulerRetirementNonRegularKind::Directory
                }
                EntryType::Regular => anyhow::bail!("resolution coordinate became regular"),
                EntryType::Symlink => {
                    ryeos_scheduler_contract::SchedulerRetirementNonRegularKind::Symlink
                }
                EntryType::CharacterDevice => {
                    ryeos_scheduler_contract::SchedulerRetirementNonRegularKind::CharacterDevice
                }
                EntryType::BlockDevice => {
                    ryeos_scheduler_contract::SchedulerRetirementNonRegularKind::BlockDevice
                }
                EntryType::Fifo => {
                    ryeos_scheduler_contract::SchedulerRetirementNonRegularKind::Fifo
                }
                EntryType::Socket => {
                    ryeos_scheduler_contract::SchedulerRetirementNonRegularKind::Socket
                }
                EntryType::Other => {
                    ryeos_scheduler_contract::SchedulerRetirementNonRegularKind::Other
                }
            };
            ensure!(
                &actual == kind,
                "scheduler non-regular resolution coordinate changed"
            );
            Ok(())
        }
        (Coordinate::Unreadable { .. }, _) => {
            anyhow::bail!("unreadable scheduler resolution coordinate requires held recovery")
        }
        _ => anyhow::bail!("scheduler resolution coordinate differs from its signed observation"),
    }
}

/// Remove every surviving record of one retired operation's generations,
/// then the retirement record itself, returning how many were removed.
/// Every record is authenticated as this node's signed record of a listed
/// generation (an intent also of the listed operation) before its exact
/// inode is unlinked. The caller holds the exclusion
/// [`RetirementJournal::retire_acknowledged_evidence`] requires.
fn finish_retired_operation(
    runtime: &lillux::PinnedDirectory,
    identity: &NodeIdentity,
    name: &str,
) -> Result<usize> {
    let runtime_identity = serde_json::to_string(&runtime.identity()?)?;
    let record: RetiredOperationEvidence = read_signed(runtime, name, identity)?;
    record.validate(name, identity, &runtime_identity)?;
    let mut removed = 0usize;
    for entry in runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)? {
        let display = entry.to_string_lossy();
        let Some(generation) = journal_record_generation(&display)? else {
            continue;
        };
        if !record.generations.contains(&generation) {
            continue;
        }
        remove_signed_record(runtime, &display, |bytes| {
            authenticate_journal_record(&display, bytes, identity, &generation, &record.operation_id)
        })?;
        removed += 1;
    }
    remove_signed_record(runtime, name, |bytes| {
        let current: RetiredOperationEvidence = verify_signed(bytes, identity)?;
        ensure!(
            current == record,
            "retired scheduler evidence record changed during removal"
        );
        Ok(())
    })?;
    Ok(removed + 1)
}

/// Accept `bytes` at journal record `name` only as this node's signed record
/// of `generation`; an intent must also name `operation_id`.
fn authenticate_journal_record(
    name: &str,
    bytes: &[u8],
    identity: &NodeIdentity,
    generation: &str,
    operation_id: &str,
) -> Result<()> {
    let (signed_generation, owner) = if name.starts_with(INTENT_PREFIX) {
        let intent: SchedulerRetirementIntent = verify_signed(bytes, identity)?;
        ensure!(
            intent.operation_id == operation_id,
            "retired scheduler generation intent names another operation"
        );
        (intent.generation, intent.owner_fingerprint)
    } else if name.starts_with(PHASE_PREFIX) {
        let phase: SchedulerRetirementRecord = verify_signed(bytes, identity)?;
        (phase.generation, phase.owner_fingerprint)
    } else {
        let rest = name
            .strip_prefix(RESOLUTION_PREFIX)
            .context("retired scheduler record has an unknown kind")?;
        if parse_resolution_name(rest)?.3 {
            let outcome: SchedulerRetirementResolutionOutcome = verify_signed(bytes, identity)?;
            (outcome.generation, outcome.owner_fingerprint)
        } else {
            let intent: SchedulerRetirementResolutionIntent = verify_signed(bytes, identity)?;
            (intent.generation, intent.owner_fingerprint)
        }
    };
    ensure!(
        signed_generation == generation && owner == identity.fingerprint(),
        "retired scheduler record {name} belongs to another generation or signer"
    );
    Ok(())
}

/// Unlink one signed journal record after `authenticate` accepts the bytes
/// its coordinate holds. The inode is pinned without a data descriptor
/// before the guarded read, and the unlink requires the coordinate to still
/// name that pinned inode, so nothing but the authenticated record goes.
fn remove_signed_record(
    runtime: &lillux::PinnedDirectory,
    name: &str,
    authenticate: impl FnOnce(&[u8]) -> Result<()>,
) -> Result<()> {
    let child = OsStr::new(name);
    let pin = runtime
        .open_mount_entry(child)?
        .with_context(|| format!("scheduler journal record {name} is absent before removal"))?;
    ensure!(
        pin.metadata()?.is_file(),
        "scheduler journal record {name} is not a regular file"
    );
    authenticate(&read_signed_bytes(runtime, name)?)
        .with_context(|| format!("authenticate scheduler journal record {name} before removal"))?;
    ensure!(
        runtime
            .unlink_regular_if_same(child, &pin)
            .with_context(|| format!("remove scheduler journal record {name}"))?,
        "scheduler journal record {name} disappeared during removal"
    );
    Ok(())
}

fn validate_completed_flow(records: &[SchedulerRetirementRecord]) -> Result<()> {
    use ryeos_scheduler_contract::SchedulerRetirementPhase as Phase;

    // An intent without a phase record has not authorized a filesystem or
    // SQLite role effect. The same live controller may safely retry its
    // immutable publication; restart admission still refuses it.
    if records.is_empty() {
        return Ok(());
    }

    let expected = [
        (SchedulerRetirementRole::Main, Phase::Captured),
        (SchedulerRetirementRole::Wal, Phase::Captured),
        (SchedulerRetirementRole::Shm, Phase::Captured),
        (SchedulerRetirementRole::Main, Phase::SqlDrainAuthorized),
        (SchedulerRetirementRole::Wal, Phase::SqlDrainAuthorized),
        (SchedulerRetirementRole::Shm, Phase::SqlDrainAuthorized),
        (SchedulerRetirementRole::Main, Phase::SqlDrained),
        (SchedulerRetirementRole::Wal, Phase::SqlDrained),
        (SchedulerRetirementRole::Shm, Phase::SqlDrained),
        (SchedulerRetirementRole::Main, Phase::QuarantineAuthorized),
        (SchedulerRetirementRole::Main, Phase::Quarantined),
        (SchedulerRetirementRole::Main, Phase::PurgeAuthorized),
        (SchedulerRetirementRole::Main, Phase::Purged),
        (SchedulerRetirementRole::Wal, Phase::QuarantineAuthorized),
        (SchedulerRetirementRole::Wal, Phase::Quarantined),
        (SchedulerRetirementRole::Wal, Phase::PurgeAuthorized),
        (SchedulerRetirementRole::Wal, Phase::Purged),
        (SchedulerRetirementRole::Shm, Phase::QuarantineAuthorized),
        (SchedulerRetirementRole::Shm, Phase::Quarantined),
        (SchedulerRetirementRole::Shm, Phase::PurgeAuthorized),
        (SchedulerRetirementRole::Shm, Phase::Purged),
        (SchedulerRetirementRole::Main, Phase::ReleaseAuthorized),
        (SchedulerRetirementRole::Wal, Phase::ReleaseAuthorized),
        (SchedulerRetirementRole::Shm, Phase::ReleaseAuthorized),
        (SchedulerRetirementRole::Main, Phase::Released),
        (SchedulerRetirementRole::Wal, Phase::Released),
        (SchedulerRetirementRole::Shm, Phase::Released),
    ];
    ensure!(
        records.len() == expected.len() || records.len() == expected.len() + 1,
        "unfinished scheduler retirement generation requires exact restart reconciliation"
    );
    for (record, (role, phase)) in records.iter().take(expected.len()).zip(expected) {
        ensure!(
            record.role == role && record.phase == phase,
            "scheduler retirement phase order is invalid"
        );
    }
    for role_index in 0..3 {
        let capture = records[role_index].role_identity.as_ref();
        let drain = records[6 + role_index].role_identity.as_ref();
        match (capture, drain) {
            (None, None) => {}
            (Some(before), Some(after)) => ensure!(
                before.device == after.device
                    && before.inode == after.inode
                    && before.content_sha256.is_none()
                    && after.content_sha256.is_some(),
                "scheduler retirement captured/drained role facts disagree"
            ),
            _ => anyhow::bail!("scheduler retirement role presence changed during SQL drain"),
        }
        let expected_identity = drain;
        for phase_index in [
            9 + role_index * 4,
            10 + role_index * 4,
            11 + role_index * 4,
            12 + role_index * 4,
        ] {
            ensure!(
                records[phase_index].role_identity.as_ref() == expected_identity,
                "scheduler retirement role facts changed during quarantine/purge"
            );
        }
        for phase_index in [21 + role_index, 24 + role_index] {
            ensure!(
                records[phase_index].role_identity.as_ref() == expected_identity,
                "scheduler retirement role facts changed before native release"
            );
        }
    }
    if records.len() == expected.len() + 1 {
        let completion = &records[expected.len()];
        ensure!(
            completion.sequence == 27
                && completion.role == SchedulerRetirementRole::Main
                && completion.phase == Phase::Complete
                && completion.role_identity == records[24].role_identity,
            "scheduler retirement terminal completion record is invalid"
        );
    }
    Ok(())
}

fn load_record_chain(
    runtime: &lillux::PinnedDirectory,
    identity: &NodeIdentity,
    generation: &str,
) -> Result<Vec<SchedulerRetirementRecord>> {
    let mut records = Vec::new();
    let mut previous_hash = None;
    for sequence in 0..MAX_RECORDS_PER_GENERATION {
        let name = phase_name(generation, u8::try_from(sequence)?);
        let Some(signed) = read_optional_signed_bytes(runtime, &name)? else {
            break;
        };
        let record: SchedulerRetirementRecord = verify_signed(&signed, identity)?;
        record.validate()?;
        ensure!(
            usize::from(record.sequence) == sequence
                && record.generation == generation
                && record.previous_sha256 == previous_hash,
            "scheduler retirement phase chain changed during completion"
        );
        previous_hash = Some(lillux::sha256_hex(&signed));
        records.push(record);
    }
    Ok(records)
}

fn publish_signed<T>(
    runtime: &lillux::PinnedDirectory,
    identity: &NodeIdentity,
    name: &str,
    value: &T,
) -> Result<Vec<u8>>
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de> + PartialEq + Clone,
{
    let signed = sign_record_bytes(identity, value)?;
    publish_signed_bytes(runtime, identity, name, value, &signed)
}

fn sign_record_bytes<T>(identity: &NodeIdentity, value: &T) -> Result<Vec<u8>>
where
    T: serde::Serialize,
{
    let body = encode_scheduler_retirement_record(value)?;
    let body = std::str::from_utf8(&body)?;
    let signed = lillux::signature::sign_content(body, identity.signing_key(), "#", None);
    ensure!(
        signed.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES,
        "signed scheduler retirement record exceeds the bounded file size"
    );
    Ok(signed.into_bytes())
}

fn publish_signed_bytes<T>(
    runtime: &lillux::PinnedDirectory,
    identity: &NodeIdentity,
    name: &str,
    value: &T,
    signed: &[u8],
) -> Result<Vec<u8>>
where
    T: for<'de> serde::Deserialize<'de> + serde::Serialize + PartialEq + Clone,
{
    ensure!(
        signed.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES,
        "signed scheduler retirement record exceeds the bounded file size"
    );
    let child = std::ffi::OsStr::new(name);
    if runtime
        .atomic_create_regular(child, signed, 0o600, crate::SCHEDULER_TEMP_PURPOSE)?
        .is_some()
    {
        return Ok(signed.to_vec());
    }
    let existing = read_signed_bytes(runtime, name)?;
    let parsed: T = verify_signed(&existing, identity)?;
    ensure!(
        &parsed == value,
        "immutable scheduler retirement record conflicts with existing evidence"
    );
    ensure!(
        existing == signed,
        "immutable scheduler retirement record bytes differ from the retained publication"
    );
    runtime
        .sync()
        .context("sync existing immutable scheduler retirement evidence")?;
    Ok(existing)
}

fn read_signed<T>(
    runtime: &lillux::PinnedDirectory,
    name: &str,
    identity: &NodeIdentity,
) -> Result<T>
where
    T: for<'de> serde::Deserialize<'de> + serde::Serialize,
{
    read_signed_in(None, runtime, name, identity)
}

fn read_signed_in<T>(
    factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    runtime: &lillux::PinnedDirectory,
    name: &str,
    identity: &NodeIdentity,
) -> Result<T>
where
    T: for<'de> serde::Deserialize<'de> + serde::Serialize,
{
    let bytes = read_signed_bytes_in(factory, runtime, name)?;
    verify_signed(&bytes, identity)
}

fn verify_signed<T>(bytes: &[u8], identity: &NodeIdentity) -> Result<T>
where
    T: for<'de> serde::Deserialize<'de> + serde::Serialize,
{
    let raw = std::str::from_utf8(bytes)?;
    let (body, header) =
        lillux::signature::strip_canonical_signature_with_envelope(raw, "#", None, false)?;
    let header = header.context("scheduler retirement record is unsigned")?;
    ensure!(
        lillux::signature::is_valid_signature_for(
            &header.content_hash,
            &header.signature_b64,
            &header.signer_fingerprint,
            &body,
            identity.verifying_key(),
            identity.fingerprint(),
        ),
        "scheduler retirement record signature is invalid"
    );
    decode_scheduler_retirement_record(body.as_bytes())
}

fn read_optional_signed_bytes(
    runtime: &lillux::PinnedDirectory,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    read_optional_signed_bytes_in(None, runtime, name)
}

thread_local! {
    /// The reset's native factory while this thread runs a scheduler
    /// retirement call under its permit. Weak, so it is never a factory
    /// alias the native host's release would count.
    static RESET_NATIVE_SCOPE: std::cell::RefCell<Option<std::sync::Weak<lillux_sqlite::OriginalNativeFactory>>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs one scheduler retirement call with its guarded reads in the reset
/// permit's native factory scope. The previous selection is restored when
/// this ends. It is not `Send`: the selection belongs to this thread.
pub(crate) struct ResetNativeReadScope {
    previous: Option<std::sync::Weak<lillux_sqlite::OriginalNativeFactory>>,
    _thread: std::marker::PhantomData<*const ()>,
}
impl ResetNativeReadScope {
    pub(crate) fn enter(factory: Option<&std::sync::Arc<lillux_sqlite::OriginalNativeFactory>>) -> Self {
        let previous = RESET_NATIVE_SCOPE.with(|scope| scope.replace(factory.map(std::sync::Arc::downgrade)));
        Self { previous, _thread: std::marker::PhantomData }
    }
}
impl Drop for ResetNativeReadScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        RESET_NATIVE_SCOPE.with(|scope| scope.replace(previous));
    }
}

/// The factory selected by the innermost [`ResetNativeReadScope`]. A
/// selected factory that has already ended refuses.
pub(crate) fn reset_native_scope() -> Result<Option<std::sync::Arc<lillux_sqlite::OriginalNativeFactory>>> {
    RESET_NATIVE_SCOPE.with(|scope| match scope.borrow().as_ref() {
        Some(weak) => weak
            .upgrade()
            .map(Some)
            .context("reset native factory ended during scheduler retirement"),
        None => Ok(None),
    })
}

/// With the owning native factory the guarded read runs in that factory's
/// scope, so its own held projection leases answer for their owner. Without
/// one it uses the factory of the enclosing [`ResetNativeReadScope`]; with
/// neither the read refuses whenever any lease is listed.
fn read_optional_signed_bytes_in(
    factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    runtime: &lillux::PinnedDirectory,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    // Only copied bounded bytes escape the native exclusion gate. Never open
    // an observer File here: closing a hardlink alias could drop SQLite locks
    // before signature validation gets a chance to refuse the evidence.
    let maximum = ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES;
    let scoped = match factory {
        Some(_) => None,
        None => reset_native_scope()?,
    };
    let bytes = match factory.or(scoped.as_deref()) {
        Some(factory) => factory.read_regular_without_sqlite_alias(runtime, OsStr::new(name), maximum),
        None => lillux_sqlite::read_regular_without_sqlite_alias(runtime, OsStr::new(name), maximum),
    }
    .with_context(|| format!("read guarded scheduler retirement record {name}"))?;
    if let Some(bytes) = &bytes {
        ensure!(
            bytes.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES,
            "scheduler retirement record exceeds the bounded file size"
        );
    }
    Ok(bytes)
}

fn read_signed_bytes(runtime: &lillux::PinnedDirectory, name: &str) -> Result<Vec<u8>> {
    read_signed_bytes_in(None, runtime, name)
}

fn read_signed_bytes_in(
    factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    runtime: &lillux::PinnedDirectory,
    name: &str,
) -> Result<Vec<u8>> {
    read_optional_signed_bytes_in(factory, runtime, name)?
        .with_context(|| format!("scheduler retirement record {name} is absent"))
}

fn phase_name(generation: &str, sequence: u8) -> String {
    format!("{PHASE_PREFIX}{generation}.{sequence:02}")
}

fn parse_phase_name(value: &str) -> Result<(String, u8)> {
    let (generation, sequence) = value
        .split_once('.')
        .context("malformed scheduler retirement phase filename")?;
    validate_generation(generation)?;
    ensure!(
        sequence.len() == 2 && sequence.bytes().all(|b| b.is_ascii_digit()),
        "malformed scheduler retirement phase sequence"
    );
    let sequence = sequence.parse::<u8>()?;
    ensure!(
        usize::from(sequence) < MAX_RECORDS_PER_GENERATION,
        "scheduler retirement phase sequence is out of range"
    );
    Ok((generation.to_owned(), sequence))
}

fn parse_resolution_name(value: &str) -> Result<(String, usize, u8, bool)> {
    let parts = value.split('.').collect::<Vec<_>>();
    ensure!(parts.len() == 4, "malformed scheduler resolution filename");
    let generation = parts[0];
    validate_generation(generation)?;
    let role = role_index_from_name(parts[1])?;
    let ordinal = parse_resolution_ordinal(parts[2])?;
    let is_outcome = match parts[3] {
        "intent" => false,
        "outcome" => true,
        _ => anyhow::bail!("malformed scheduler resolution record suffix"),
    };
    Ok((generation.to_owned(), role, ordinal, is_outcome))
}

fn parse_resolution_quarantine_name(value: &str) -> Result<(String, usize, u8)> {
    let parts = value.split('.').collect::<Vec<_>>();
    ensure!(
        parts.len() == 4 && parts[3] == "quarantine",
        "malformed scheduler resolution quarantine filename"
    );
    let generation = parts[0];
    validate_generation(generation)?;
    Ok((
        generation.to_owned(),
        role_index_from_name(parts[1])?,
        parse_resolution_ordinal(parts[2])?,
    ))
}

fn parse_role_artifact_name(value: &str) -> Result<String> {
    let rest = value
        .strip_prefix("scheduler-retirement.role.")
        .context("malformed scheduler role-artifact filename")?;
    let parts = rest.split('.').collect::<Vec<_>>();
    ensure!(
        parts.len() == 3 && matches!(parts[2], "quarantine" | "purge"),
        "malformed scheduler role-artifact filename"
    );
    validate_generation(parts[0])?;
    role_index_from_name(parts[1])?;
    Ok(parts[0].to_owned())
}

fn validate_phase_prefix(records: &[SchedulerRetirementRecord]) -> Result<()> {
    use ryeos_scheduler_contract::SchedulerRetirementPhase as Phase;
    let expected = [
        (SchedulerRetirementRole::Main, Phase::Captured),
        (SchedulerRetirementRole::Wal, Phase::Captured),
        (SchedulerRetirementRole::Shm, Phase::Captured),
        (SchedulerRetirementRole::Main, Phase::SqlDrainAuthorized),
        (SchedulerRetirementRole::Wal, Phase::SqlDrainAuthorized),
        (SchedulerRetirementRole::Shm, Phase::SqlDrainAuthorized),
        (SchedulerRetirementRole::Main, Phase::SqlDrained),
        (SchedulerRetirementRole::Wal, Phase::SqlDrained),
        (SchedulerRetirementRole::Shm, Phase::SqlDrained),
        (SchedulerRetirementRole::Main, Phase::QuarantineAuthorized),
        (SchedulerRetirementRole::Main, Phase::Quarantined),
        (SchedulerRetirementRole::Main, Phase::PurgeAuthorized),
        (SchedulerRetirementRole::Main, Phase::Purged),
        (SchedulerRetirementRole::Wal, Phase::QuarantineAuthorized),
        (SchedulerRetirementRole::Wal, Phase::Quarantined),
        (SchedulerRetirementRole::Wal, Phase::PurgeAuthorized),
        (SchedulerRetirementRole::Wal, Phase::Purged),
        (SchedulerRetirementRole::Shm, Phase::QuarantineAuthorized),
        (SchedulerRetirementRole::Shm, Phase::Quarantined),
        (SchedulerRetirementRole::Shm, Phase::PurgeAuthorized),
        (SchedulerRetirementRole::Shm, Phase::Purged),
        (SchedulerRetirementRole::Main, Phase::ReleaseAuthorized),
        (SchedulerRetirementRole::Wal, Phase::ReleaseAuthorized),
        (SchedulerRetirementRole::Shm, Phase::ReleaseAuthorized),
        (SchedulerRetirementRole::Main, Phase::Released),
        (SchedulerRetirementRole::Wal, Phase::Released),
        (SchedulerRetirementRole::Shm, Phase::Released),
    ];
    ensure!(
        records.len() <= expected.len(),
        "interrupted scheduler generation contains a terminal or extra phase"
    );
    for (sequence, (record, (role, phase))) in records.iter().zip(expected).enumerate() {
        ensure!(
            record.sequence == u8::try_from(sequence)?
                && record.role == role
                && record.phase == phase,
            "interrupted scheduler phase prefix differs from the admitted protocol"
        );
    }
    for role_index in 0..3 {
        // Missing later records are a legal interruption. A present record
        // carrying None is an authenticated absent role, not missing evidence.
        let capture = records.get(role_index);
        let drain = records.get(6 + role_index);
        if let (Some(capture), Some(drain)) = (capture, drain) {
            match (&capture.role_identity, &drain.role_identity) {
                (None, None) => {}
                (Some(before), Some(after)) => ensure!(
                    before.device == after.device
                        && before.inode == after.inode
                        && before.content_sha256.is_none()
                        && after.content_sha256.is_some(),
                    "interrupted scheduler capture/drain role identity changed"
                ),
                _ => anyhow::bail!("interrupted scheduler role presence changed during SQL drain"),
            }
        }
        if let Some(drain) = drain {
            let start = 9 + role_index * 4;
            for sequence in (start..start + 4).chain([21 + role_index, 24 + role_index]) {
                if let Some(record) = records.get(sequence) {
                    ensure!(
                        record.role_identity == drain.role_identity,
                        "interrupted scheduler role identity or presence changed after drain"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Authenticate a terminal recovery child's entire phase chain and every
/// bounded parent edge before the interrupted scanner suppresses those parent
/// generations. The scanner may run while an unrelated interrupted generation
/// exists, so it cannot rely on the later whole-journal admission pass.
pub(crate) fn validate_authenticated_recovery_graph(
    generations: &[AuthenticatedInterruptedGeneration],
) -> Result<()> {
    let by_id = generations
        .iter()
        .map(|e| (e.generation_intent.generation.as_str(), e))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        by_id.len() == generations.len() && by_id.len() <= MAX_GENERATIONS,
        "scheduler recovery generation inventory is duplicated or over capacity"
    );
    let mut children = std::collections::BTreeSet::new();
    for child in generations {
        let Some(binding) = &child.generation_intent.recovery else {
            continue;
        };
        let parent = by_id
            .get(binding.parent_generation.as_str())
            .context("scheduler recovery parent is absent from authenticated inventory")?;
        ensure!(
            children.insert(binding.parent_generation.clone()),
            "scheduler recovery parent has conflicting children"
        );
        ensure!(
            binding
                .stopped_plan
                .generations
                .iter()
                .all(|id| by_id.contains_key(id.as_str())),
            "scheduler recovery plan includes unauthenticated generation"
        );
        validate_recovery_edge(parent, child)?;
        if let Some(prior) = &parent.generation_intent.recovery {
            ensure!(
                prior
                    .stopped_plan
                    .generations
                    .iter()
                    .all(|id| binding.stopped_plan.generations.contains(id)),
                "scheduler recovery child dropped prior fixed generation inventory"
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut cursor = child;
        loop {
            ensure!(
                seen.insert(cursor.generation_intent.generation.clone()),
                "scheduler recovery ancestry contains a cycle"
            );
            ensure!(
                binding
                    .stopped_plan
                    .generations
                    .contains(&cursor.generation_intent.generation),
                "scheduler recovery child omitted an unresolved ancestor generation"
            );
            let Some(edge) = &cursor.generation_intent.recovery else {
                break;
            };
            cursor = by_id
                .get(edge.parent_generation.as_str())
                .context("scheduler recovery ancestor is absent")?;
        }
    }
    Ok(())
}

fn ensure_fixed_inventory_absent(
    runtime: &lillux::PinnedDirectory,
    intent: &SchedulerRetirementIntent,
    generations: &[String],
    canonical: bool,
) -> Result<()> {
    let plan = SchedulerRetirementStoppedPlan {
        generation: intent.generation.clone(),
        generations: generations.to_vec(),
        runtime_identity: intent.runtime_identity.clone(),
        roles: std::array::from_fn(|_| SchedulerRetirementStoppedRolePlan {
            source: SchedulerRetirementStoppedSource::Absent,
            current: None,
        }),
    };
    plan.validate()?;
    for name in plan.expected_coordinate_names() {
        if !canonical
            && matches!(
                name.as_str(),
                "scheduler.sqlite3" | "scheduler.sqlite3-wal" | "scheduler.sqlite3-shm"
            )
        {
            continue;
        }
        ensure!(
            runtime.entry_no_follow(OsStr::new(&name))?.is_none(),
            "terminal scheduler lineage retains fixed coordinate {name}"
        );
    }
    Ok(())
}

fn parse_resolution_ordinal(value: &str) -> Result<u8> {
    ensure!(
        value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_digit()),
        "malformed scheduler resolution ordinal"
    );
    let ordinal = value.parse::<u8>()?;
    ensure!(
        ordinal < ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE,
        "scheduler resolution ordinal is out of range"
    );
    Ok(ordinal)
}

fn role_index_from_name(value: &str) -> Result<usize> {
    match value {
        "main" => Ok(0),
        "wal" => Ok(1),
        "shm" => Ok(2),
        _ => anyhow::bail!("unknown scheduler retirement role"),
    }
}

fn validate_generation(value: &str) -> Result<()> {
    ensure!(
        value.len() == 32
            && value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid scheduler retirement generation"
    );
    Ok(())
}

pub(crate) fn role_identity(
    identity: Option<&lillux_sqlite::SqliteRoleIdentity>,
) -> Result<Option<SchedulerRetirementRoleIdentity>> {
    let Some(identity) = identity else {
        return Ok(None);
    };
    let result = SchedulerRetirementRoleIdentity {
        device: identity.device,
        inode: identity.inode,
        byte_length: identity.byte_length,
        content_sha256: identity.content_sha256.clone(),
    };
    result.validate()?;
    Ok(Some(result))
}

#[cfg(test)]
mod terminal_scanner_controls {
    use super::*;
    use ryeos_scheduler_contract::SchedulerRetirementPhase as Phase;

    fn completed_recovery_fixture(
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
    ) -> Result<String> {
        let parent = "11".repeat(16);
        let child = "22".repeat(16);
        let runtime_identity = serde_json::to_string(&runtime.identity()?)?;
        let mut intent = SchedulerRetirementIntent {
            schema: 3,
            operation_id: parent.clone(),
            operation_sha256: "aa".repeat(32),
            scheduler_accounting: ryeos_scheduler_contract::SchedulerFireHistoryAccounting::Exact {
                counts: Default::default(),
            },
            scheduler_sources: std::array::from_fn(|_| SchedulerRetirementStoppedRolePlan {
                source: SchedulerRetirementStoppedSource::Absent,
                current: None,
            }),
            purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
            generation: parent.clone(),
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: runtime_identity.clone(),
            roles: [None, None, None],
            intake_mode: "stopped-original".to_owned(),
            stopped_plan: None,
            recovery: None,
        };
        intent.stopped_plan = Some(SchedulerRetirementStoppedPlan {
            generation: parent.clone(),
            generations: vec![parent.clone()],
            runtime_identity: runtime_identity.clone(),
            roles: intent.scheduler_sources.clone(),
        });
        intent.validate()?;
        let parent_bytes = publish_signed(
            runtime,
            identity,
            &format!("{INTENT_PREFIX}{parent}"),
            &intent,
        )?;
        let expected = [
            (SchedulerRetirementRole::Main, Phase::Captured),
            (SchedulerRetirementRole::Wal, Phase::Captured),
            (SchedulerRetirementRole::Shm, Phase::Captured),
            (SchedulerRetirementRole::Main, Phase::SqlDrainAuthorized),
            (SchedulerRetirementRole::Wal, Phase::SqlDrainAuthorized),
            (SchedulerRetirementRole::Shm, Phase::SqlDrainAuthorized),
            (SchedulerRetirementRole::Main, Phase::SqlDrained),
            (SchedulerRetirementRole::Wal, Phase::SqlDrained),
            (SchedulerRetirementRole::Shm, Phase::SqlDrained),
            (SchedulerRetirementRole::Main, Phase::QuarantineAuthorized),
            (SchedulerRetirementRole::Main, Phase::Quarantined),
            (SchedulerRetirementRole::Main, Phase::PurgeAuthorized),
            (SchedulerRetirementRole::Main, Phase::Purged),
            (SchedulerRetirementRole::Wal, Phase::QuarantineAuthorized),
            (SchedulerRetirementRole::Wal, Phase::Quarantined),
            (SchedulerRetirementRole::Wal, Phase::PurgeAuthorized),
            (SchedulerRetirementRole::Wal, Phase::Purged),
            (SchedulerRetirementRole::Shm, Phase::QuarantineAuthorized),
            (SchedulerRetirementRole::Shm, Phase::Quarantined),
            (SchedulerRetirementRole::Shm, Phase::PurgeAuthorized),
            (SchedulerRetirementRole::Shm, Phase::Purged),
            (SchedulerRetirementRole::Main, Phase::ReleaseAuthorized),
            (SchedulerRetirementRole::Wal, Phase::ReleaseAuthorized),
            (SchedulerRetirementRole::Shm, Phase::ReleaseAuthorized),
            (SchedulerRetirementRole::Main, Phase::Released),
            (SchedulerRetirementRole::Wal, Phase::Released),
            (SchedulerRetirementRole::Shm, Phase::Released),
        ];
        let mut parent_hashes = Vec::new();
        for generation in [&parent, &child] {
            if generation == &child {
                intent.generation = child.clone();
                intent.recovery = Some(SchedulerRetirementRecoveryBinding {
                    parent_generation: parent.clone(),
                    parent_intent_sha256: lillux::sha256_hex(&parent_bytes),
                    parent_phase_sha256: parent_hashes.clone(),
                    parent_resolutions: Vec::new(),
                    stopped_plan: SchedulerRetirementStoppedPlan {
                        generation: child.clone(),
                        generations: vec![parent.clone(), child.clone()],
                        runtime_identity: runtime_identity.clone(),
                        roles: std::array::from_fn(|_| SchedulerRetirementStoppedRolePlan {
                            source: SchedulerRetirementStoppedSource::Absent,
                            current: None,
                        }),
                    },
                });
                intent.intake_mode = "stopped-recovery".to_owned();
                intent.stopped_plan = None;
                intent.validate()?;
                publish_signed(
                    runtime,
                    identity,
                    &format!("{INTENT_PREFIX}{child}"),
                    &intent,
                )?;
            }
            let mut previous = None;
            let count = if generation == &parent { 9 } else { 28 };
            for sequence in 0..count {
                let (role, phase) = if sequence == 27 {
                    (SchedulerRetirementRole::Main, Phase::Complete)
                } else {
                    expected[sequence]
                };
                let record = SchedulerRetirementRecord {
                    schema: 1,
                    purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
                    generation: generation.clone(),
                    sequence: sequence as u8,
                    previous_sha256: previous,
                    owner_fingerprint: identity.fingerprint().to_owned(),
                    runtime_identity: runtime_identity.clone(),
                    role,
                    phase,
                    role_identity: None,
                };
                record.validate()?;
                let bytes = publish_signed(
                    runtime,
                    identity,
                    &phase_name(generation, sequence as u8),
                    &record,
                )?;
                let hash = lillux::sha256_hex(&bytes);
                if generation == &parent {
                    parent_hashes.push(hash.clone());
                }
                previous = Some(hash);
            }
        }
        // An unrelated legal interruption prevents replacing this scanner with
        // whole-journal admission, which must remain HOLD for this namespace.
        intent.generation = "33".repeat(16);
        intent.operation_id = intent.generation.clone();
        intent.recovery = None;
        intent.intake_mode = "stopped-original".to_owned();
        intent.stopped_plan = Some(SchedulerRetirementStoppedPlan {
            generation: intent.generation.clone(),
            generations: vec![parent.clone(), child.clone(), intent.generation.clone()],
            runtime_identity: intent.runtime_identity.clone(),
            roles: intent.scheduler_sources.clone(),
        });
        publish_signed(
            runtime,
            identity,
            &format!("{INTENT_PREFIX}{}", intent.generation),
            &intent,
        )?;
        Ok(child)
    }

    /// A runtime holding only the completed recovery lineage of operation
    /// `11…` (an interrupted parent settled by its terminal child).
    fn completed_lineage_runtime(
    ) -> Result<(tempfile::TempDir, NodeIdentity, std::path::PathBuf, lillux::PinnedDirectory)> {
        let directory = tempfile::tempdir()?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let runtime_path = directory.path().join("runtime");
        std::fs::create_dir(&runtime_path)?;
        let runtime =
            lillux::PinnedDirectory::open(&runtime_path)?.context("test runtime absent")?;
        completed_recovery_fixture(&runtime, &identity)?;
        std::fs::remove_file(runtime_path.join(format!("{INTENT_PREFIX}{}", "33".repeat(16))))?;
        Ok((directory, identity, runtime_path, runtime))
    }

    fn reserved_names(runtime: &lillux::PinnedDirectory) -> Result<Vec<String>> {
        let mut names = runtime
            .entry_names_bounded(MAX_DIRECTORY_ENTRIES)?
            .into_iter()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| name.starts_with(RESERVED_PREFIX))
            .collect::<Vec<_>>();
        names.sort();
        Ok(names)
    }

    /// The retirement record an acknowledged reset publishes for the
    /// fixture's operation, before it removes anything.
    fn fixture_retired_record(
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
    ) -> Result<(String, RetiredOperationEvidence)> {
        let parent = "11".repeat(16);
        let child = "22".repeat(16);
        let complete = read_signed_bytes(runtime, &phase_name(&child, COMPLETE_SEQUENCE))?;
        let record = RetiredOperationEvidence {
            schema: 1,
            purpose: RETIRED_PURPOSE.to_owned(),
            operation_id: parent.clone(),
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: serde_json::to_string(&runtime.identity()?)?,
            terminal_generation: child.clone(),
            terminal_sha256: lillux::sha256_hex(&complete),
            generations: vec![parent.clone(), child],
        };
        Ok((format!("{RETIRED_PREFIX}{parent}"), record))
    }

    /// Once acknowledged, a completed lineage's every signed record goes,
    /// including the settled parent's, and the namespace admits as empty.
    #[test]
    fn acknowledged_retirement_removes_every_record_of_a_completed_lineage() -> Result<()> {
        let (_directory, identity, _runtime_path, runtime) = completed_lineage_runtime()?;
        let before = reserved_names(&runtime)?;
        // Parent intent and 9 phases, child intent and 28 phases.
        assert_eq!(before.len(), 1 + 9 + 1 + 28, "{before:?}");
        let removed = RetirementJournal::retire_acknowledged_evidence(&runtime, &identity)?;
        // Every journal record plus the transient retirement record.
        assert_eq!(removed, before.len() + 1);
        assert!(reserved_names(&runtime)?.is_empty(), "{:?}", reserved_names(&runtime)?);
        assert_eq!(RetirementJournal::validate_admission(&runtime, &identity)?, 0);
        // A second acknowledged reset finds nothing left to retire.
        assert_eq!(RetirementJournal::retire_acknowledged_evidence(&runtime, &identity)?, 0);
        Ok(())
    }

    /// A process that dies after the retirement record is durable, partway
    /// through removal, leaves a namespace every scanner admits as settled;
    /// the next acknowledged reset finishes it.
    #[test]
    fn partial_retirement_is_admitted_and_finished_by_the_next_reset() -> Result<()> {
        let (_directory, identity, runtime_path, runtime) = completed_lineage_runtime()?;
        let (name, record) = fixture_retired_record(&runtime, &identity)?;
        publish_signed(&runtime, &identity, &name, &record)?;
        // Every scanner admits the retiring lineage before any removal.
        assert!(RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.is_empty());
        RetirementJournal::validate_admission(&runtime, &identity)?;
        RetirementJournal::reserve_pre_effect_capacity(&runtime, Some(&identity), true)?;
        // A partial removal: the terminal record, the child intent and a
        // middle parent phase are gone, so no chain is dense any more.
        let child = "22".repeat(16);
        let parent = "11".repeat(16);
        for removed in [
            phase_name(&child, COMPLETE_SEQUENCE),
            format!("{INTENT_PREFIX}{child}"),
            phase_name(&parent, 4),
        ] {
            std::fs::remove_file(runtime_path.join(removed))?;
        }
        assert!(RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.is_empty());
        RetirementJournal::validate_admission(&runtime, &identity)?;
        RetirementJournal::reserve_pre_effect_capacity(&runtime, Some(&identity), true)?;
        let remaining = reserved_names(&runtime)?.len();
        let removed = RetirementJournal::retire_acknowledged_evidence(&runtime, &identity)?;
        assert_eq!(removed, remaining);
        assert!(reserved_names(&runtime)?.is_empty(), "{:?}", reserved_names(&runtime)?);
        Ok(())
    }

    /// After a crash partway through removal, a scheduling daemon recreates
    /// the scheduler database; the next reset's new generation binds an
    /// inventory without the retired generations, exactly as the plan built
    /// from `authenticated_generation_ids` does, and begins. Once that
    /// operation's own lineage is out of the way, the next acknowledgement
    /// finishes the leftover.
    #[test]
    fn new_generation_begins_beside_a_partially_retired_lineage() -> Result<()> {
        let (_directory, identity, runtime_path, runtime) = completed_lineage_runtime()?;
        let (name, record) = fixture_retired_record(&runtime, &identity)?;
        publish_signed(&runtime, &identity, &name, &record)?;
        let child = "22".repeat(16);
        std::fs::remove_file(runtime_path.join(phase_name(&child, COMPLETE_SEQUENCE)))?;
        std::fs::remove_file(runtime_path.join(phase_name(&child, 3)))?;
        std::fs::write(runtime_path.join("scheduler.sqlite3"), b"recreated main role")?;

        let generation = "66".repeat(16);
        assert_eq!(
            RetirementJournal::authenticated_generation_ids(&runtime, &identity)?,
            Vec::<String>::new(),
            "retired generations are outside the authenticated inventory"
        );
        let mut intent = SchedulerRetirementIntent {
            schema: 3,
            purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
            operation_id: generation.clone(),
            operation_sha256: "aa".repeat(32),
            scheduler_accounting: ryeos_scheduler_contract::SchedulerFireHistoryAccounting::Exact {
                counts: Default::default(),
            },
            scheduler_sources: std::array::from_fn(|_| SchedulerRetirementStoppedRolePlan {
                source: SchedulerRetirementStoppedSource::Absent,
                current: None,
            }),
            generation: generation.clone(),
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: serde_json::to_string(&runtime.identity()?)?,
            roles: [None, None, None],
            intake_mode: "stopped-original".to_owned(),
            stopped_plan: None,
            recovery: None,
        };
        intent.stopped_plan = Some(SchedulerRetirementStoppedPlan {
            generation: generation.clone(),
            generations: vec![generation.clone()],
            runtime_identity: intent.runtime_identity.clone(),
            roles: intent.scheduler_sources.clone(),
        });
        let journal = RetirementJournal::begin_with_intent(&runtime, &identity, intent)?;
        assert_eq!(journal.next_sequence(), 0);
        drop(journal);
        let interrupted = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?;
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].generation_intent.generation, generation);
        assert_eq!(interrupted[0].known_generations, vec![generation.clone()]);

        // The new operation's own lineage is not this test's subject: with it
        // and the recreated role gone, the acknowledgement finishes the
        // retired leftover and the namespace holds no scheduler record.
        std::fs::remove_file(runtime_path.join(format!("{INTENT_PREFIX}{generation}")))?;
        std::fs::remove_file(runtime_path.join("scheduler.sqlite3"))?;
        RetirementJournal::retire_acknowledged_evidence(&runtime, &identity)?;
        assert!(reserved_names(&runtime)?.is_empty(), "{:?}", reserved_names(&runtime)?);
        Ok(())
    }

    /// The native stopped intake admits only its own plan's journal names.
    /// After a crash partway through retiring an acknowledged lineage and a
    /// scheduling boot that recreated the database, the leftover records and
    /// an earlier operation's begun fire retirement make that intake refuse;
    /// finishing them first, as `begin_retirement` does before any intake,
    /// lets the real stopped intake capture and retire the new database.
    #[cfg(target_os = "linux")]
    #[test]
    fn stopped_intake_captures_after_acknowledged_leftovers_are_finished() -> Result<()> {
        use lillux_sqlite::{
            SqliteRole, SqliteRoleIdentity, SqliteRoleMutationOutcome, StoppedSqliteProjectionPlan,
            StoppedSqliteRolePlan, StoppedSqliteRoleSource,
        };
        let (_directory, identity, runtime_path, runtime) = completed_lineage_runtime()?;
        let (name, record) = fixture_retired_record(&runtime, &identity)?;
        publish_signed(&runtime, &identity, &name, &record)?;
        let child = "22".repeat(16);
        std::fs::remove_file(runtime_path.join(phase_name(&child, COMPLETE_SEQUENCE)))?;
        std::fs::remove_file(runtime_path.join(phase_name(&child, 3)))?;
        // An earlier operation's begun fire retirement, its tree half gone.
        let earlier = "77".repeat(16);
        publish_signed(
            &runtime,
            &identity,
            &format!("{FIRES_PREFIX}{earlier}"),
            &FireHistoryRetirementIntent {
                schema: 1,
                purpose: FIRES_PURPOSE.to_owned(),
                operation_id: earlier.clone(),
                operation_sha256: "ab".repeat(32),
                owner_fingerprint: identity.fingerprint().to_owned(),
                runtime_identity: serde_json::to_string(&runtime.identity()?)?,
                schedules: 1,
                files: 1,
            },
        )?;
        std::fs::create_dir_all(runtime_path.join(FIRES_ROOT).join("alpha"))?;
        std::fs::write(
            runtime_path.join(FIRES_ROOT).join("alpha").join(FIRE_JOURNAL_LOCK),
            b"",
        )?;

        // The scheduling boot after the crash recreated a closed database.
        let host = ryeos_scheduler::test_native::TestSchedulerHost::acquire();
        let created = host.open_at(&runtime_path).map_err(|failure| anyhow::anyhow!("{failure}"))?;
        host.settle(created);
        let generation = [0x66; 16];
        let role_identity = |name: &str| -> Result<Option<SqliteRoleIdentity>> {
            let Some(entry) = runtime.entry_no_follow(OsStr::new(name))? else {
                return Ok(None);
            };
            let file = runtime
                .open_regular(OsStr::new(name), false)?
                .context("closed scheduler role absent")?;
            let length = file.metadata()?.len();
            let (digest, _) = lillux::digest_open_regular_file_stable_exact(&file, length)?;
            Ok(Some(SqliteRoleIdentity {
                device: entry.containing_device,
                inode: entry.inode,
                byte_length: length,
                content_sha256: Some(digest),
            }))
        };
        let mut roles = Vec::new();
        for name in ["scheduler.sqlite3", "scheduler.sqlite3-wal", "scheduler.sqlite3-shm"] {
            roles.push(match role_identity(name)? {
                Some(current) => StoppedSqliteRolePlan {
                    source: StoppedSqliteRoleSource::Canonical,
                    current: Some(current),
                },
                None => StoppedSqliteRolePlan {
                    source: StoppedSqliteRoleSource::Absent,
                    current: None,
                },
            });
        }
        let plan = StoppedSqliteProjectionPlan {
            generation,
            generations: vec![generation],
            runtime_identity: runtime.identity()?,
            roles: roles.try_into().map_err(|_| anyhow::anyhow!("three roles"))?,
        };
        let intake_runtime = std::sync::Arc::new(
            lillux::PinnedDirectory::open(&runtime_path)?.context("test runtime absent")?,
        );

        let factory = std::sync::Arc::clone(host.factory());
        let failure = factory
            .with_original_stopped_intake(|_| {
                ryeos_scheduler::SchedulerDb::capture_stopped_projection(
                    std::sync::Arc::clone(&intake_runtime),
                    plan.clone(),
                )
                .err()
            })
            .map_err(|failure| anyhow::anyhow!("{failure}"))?;
        let failure = failure.context("the leftover records must refuse the native intake")?;
        let refusal = failure.to_string();
        assert!(
            refusal.contains("unknown reserved stopped namespace artifact"),
            "the intake must refuse the leftovers as unknown reserved artifacts: {refusal}"
        );

        let removed = RetirementJournal::finish_acknowledged_leftovers(
            &runtime,
            &identity,
            &"66".repeat(16),
            &"aa".repeat(32),
        )?;
        assert!(removed > 0);
        assert!(reserved_names(&runtime)?.is_empty(), "{:?}", reserved_names(&runtime)?);
        assert!(
            std::fs::read_dir(runtime_path.join(FIRES_ROOT))?.next().is_none(),
            "the earlier begun fire retirement is finished"
        );

        let db = factory
            .with_original_stopped_intake(|_| failure.retry_stopped_projection(&plan))
            .map_err(|failure| anyhow::anyhow!("{failure}"))?;
        let db = db.map_err(|failure| anyhow::anyhow!("{failure}"))?;
        let db = factory
            .with_original_service_scope(|_| -> Result<_> {
            db.drain_projection_retirement()?;
            for role in [SqliteRole::Main, SqliteRole::Wal, SqliteRole::Shm] {
                let present = plan.roles[role as usize].current.is_some();
                let quarantined = db.quarantine_projection_role(role)?;
                let purged = db.purge_projection_role(role)?;
                if present {
                    assert_eq!(quarantined, SqliteRoleMutationOutcome::QuarantinedDurable);
                    assert_eq!(purged, SqliteRoleMutationOutcome::RemovedDurable);
                }
            }
            db.release_projection_retirement()?;
            Ok(db)
        })
            .map_err(|failure| anyhow::anyhow!("{failure}"))??;
        factory
            .with_original_service_scope(move |_| drop(db))
            .map_err(|failure| anyhow::anyhow!("{failure}"))?;
        assert!(!runtime_path.join("scheduler.sqlite3").exists());
        Ok(())
    }

    /// Records skipped as retired must still be this node's signed records
    /// of a listed generation: a forged one refuses at admission.
    #[test]
    fn forged_record_of_a_retired_generation_refuses_at_admission() -> Result<()> {
        let (_directory, identity, runtime_path, runtime) = completed_lineage_runtime()?;
        let (name, record) = fixture_retired_record(&runtime, &identity)?;
        publish_signed(&runtime, &identity, &name, &record)?;
        let child = "22".repeat(16);
        std::fs::write(
            runtime_path.join(phase_name(&child, 5)),
            b"unsigned bytes at a retired generation's phase name",
        )?;
        assert!(RetirementJournal::scan_interrupted_evidence(&runtime, &identity).is_err());
        assert!(RetirementJournal::validate_admission(&runtime, &identity).is_err());
        assert!(RetirementJournal::retire_acknowledged_evidence(&runtime, &identity).is_err());
        Ok(())
    }

    /// An interrupted generation keeps every record: nothing is removed.
    #[test]
    fn interrupted_generation_refuses_acknowledged_retirement_untouched() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let runtime_path = directory.path().join("runtime");
        std::fs::create_dir(&runtime_path)?;
        let runtime =
            lillux::PinnedDirectory::open(&runtime_path)?.context("test runtime absent")?;
        completed_recovery_fixture(&runtime, &identity)?;
        let before = reserved_names(&runtime)?;
        assert!(RetirementJournal::retire_acknowledged_evidence(&runtime, &identity).is_err());
        assert_eq!(reserved_names(&runtime)?, before);
        Ok(())
    }

    /// A retirement record must bind the surviving terminal record and keep
    /// every later generation out of its operation; otherwise every scanner
    /// refuses.
    #[test]
    fn retirement_record_refuses_an_unbound_terminal_or_a_joining_generation() -> Result<()> {
        let (_directory, identity, _runtime_path, runtime) = completed_lineage_runtime()?;
        let (name, mut record) = fixture_retired_record(&runtime, &identity)?;
        record.terminal_sha256 = "00".repeat(32);
        publish_signed(&runtime, &identity, &name, &record)?;
        assert!(RetirementJournal::scan_interrupted_evidence(&runtime, &identity).is_err());
        assert!(RetirementJournal::validate_admission(&runtime, &identity).is_err());
        assert!(RetirementJournal::retire_acknowledged_evidence(&runtime, &identity).is_err());

        let (_directory, identity, _runtime_path, runtime) = completed_lineage_runtime()?;
        let (name, mut record) = fixture_retired_record(&runtime, &identity)?;
        // Retire only the terminal child: the parent intent of the same
        // operation now joins a retired operation.
        record.generations = vec![record.terminal_generation.clone()];
        publish_signed(&runtime, &identity, &name, &record)?;
        assert!(RetirementJournal::scan_interrupted_evidence(&runtime, &identity).is_err());
        assert!(RetirementJournal::retire_acknowledged_evidence(&runtime, &identity).is_err());
        Ok(())
    }

    struct HeldProjectionLease;
    impl lillux_sqlite::OriginalNativeProjectionLeaseAuthority for HeldProjectionLease {
        fn require_original(
            &self,
            _factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
            _cleanup: bool,
        ) -> Result<()> {
            Ok(())
        }
        fn fork_deadline(
            &self,
            _factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
            _cleanup: bool,
        ) -> Result<lillux::time::MonotonicDeadline> {
            Ok(lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(60)))
        }
    }

    /// Startup admission runs after the State projection lease is held. A
    /// completed retirement journal must then still be admitted through the
    /// live factory's scope, while the unscoped read keeps refusing.
    #[test]
    fn startup_admission_reads_completed_journal_under_a_held_projection_lease() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let runtime_path = directory.path().join("runtime");
        std::fs::create_dir(&runtime_path)?;
        let runtime =
            lillux::PinnedDirectory::open(&runtime_path)?.context("test runtime absent")?;
        completed_recovery_fixture(&runtime, &identity)?;
        // Leave only the completed parent and its terminal recovery child.
        std::fs::remove_file(runtime_path.join(format!("{INTENT_PREFIX}{}", "33".repeat(16))))?;
        let unleased = RetirementJournal::validate_admission(&runtime, &identity)?;
        let host = lillux_sqlite::test_host::TestNativeHost::acquire();
        let lease_root = std::sync::Arc::new(
            lillux::PinnedDirectory::open(directory.path())?.context("lease root absent")?,
        );
        std::fs::File::create(directory.path().join("projection.lease"))?;
        let metadata = lease_root
            .open_mount_entry(OsStr::new("projection.lease"))?
            .context("lease metadata absent")?;
        let authority: std::sync::Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority> =
            std::sync::Arc::new(HeldProjectionLease);
        // SAFETY: test-only lease under the host's own funded factory; the
        // metadata inode is a fresh regular file no SQLite role uses.
        let inputs = unsafe {
            lillux_sqlite::OriginalProjectionLeaseInputs::issued(
                lease_root,
                metadata,
                lillux_sqlite::OriginalProjectionLeaseMode::Exclusive,
                authority,
            )
        };
        let lease = match host.factory().acquire_projection_lease(inputs) {
            Ok(lease) => lease,
            Err(failure) => panic!("test lease refused: {failure}"),
        };
        let unscoped = RetirementJournal::validate_admission(&runtime, &identity)
            .map_err(|error| format!("{error:#}"));
        let scoped = crate::validate_retirement_startup(host.factory(), &runtime, &identity)
            .map_err(|error| format!("{error:#}"));
        match lease.close() {
            Ok(lillux_sqlite::OriginalProjectionLeaseDisposition::Closed(closed)) => {
                host.retain_lease(closed)
            }
            Ok(_) => panic!("held lease closed as never acquired"),
            Err(failure) => panic!("test lease close retained: {failure}"),
        }
        let refused = unscoped.expect_err("unscoped admission must refuse under a held lease");
        assert!(refused.contains("read guarded scheduler retirement record"), "{refused}");
        let admitted = scoped.expect("scoped startup admission accepts the completed journal");
        assert_eq!(admitted, unleased);
        Ok(())
    }

    #[test]
    fn signed_terminal_child_cannot_splice_original_operation_evidence() -> Result<()> {
        for changed in ["id", "hash", "source"] {
            let directory = tempfile::tempdir()?;
            let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
            let runtime_path = directory.path().join("runtime");
            std::fs::create_dir(&runtime_path)?;
            let runtime =
                lillux::PinnedDirectory::open(&runtime_path)?.context("test runtime absent")?;
            let child = completed_recovery_fixture(&runtime, &identity)?;
            assert_eq!(
                RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.len(),
                1
            );
            let name = format!("{INTENT_PREFIX}{child}");
            let mut intent: SchedulerRetirementIntent = read_signed(&runtime, &name, &identity)?;
            match changed {
                "id" => intent.operation_id = "ff".repeat(16),
                "hash" => intent.operation_sha256 = "ff".repeat(32),
                _ => {
                    intent.scheduler_sources[0] = SchedulerRetirementStoppedRolePlan {
                        source: SchedulerRetirementStoppedSource::Canonical,
                        current: Some(SchedulerRetirementRoleIdentity {
                            device: 1,
                            inode: 2,
                            byte_length: 3,
                            content_sha256: Some("ff".repeat(32)),
                        }),
                    }
                }
            }
            intent.validate()?;
            std::fs::write(
                runtime_path.join(&name),
                sign_record_bytes(&identity, &intent)?,
            )?;
            let names_before = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
            let error = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)
                .err()
                .context("terminal child suppressed parent across original operation splice")?;
            assert!(
                error
                    .to_string()
                    .contains("generations differ from the original operation evidence"),
                "{error:#}"
            );
            assert_eq!(
                runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?,
                names_before
            );
        }
        Ok(())
    }

    #[test]
    fn evidence_scan_acts_only_on_scheduler_owned_secure_temporaries() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let runtime_path = directory.path().join("runtime");
        std::fs::create_dir(&runtime_path)?;
        let runtime =
            lillux::PinnedDirectory::open(&runtime_path)?.context("test runtime absent")?;
        completed_recovery_fixture(&runtime, &identity)?;
        let foreign = [".secure.tmp.reset.4242.1", ".secure.tmp.sqlite.4242.2"];
        for name in foreign {
            std::fs::write(runtime_path.join(name), b"another owner's debris")?;
        }
        assert_eq!(
            RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.len(),
            1
        );

        let own = format!(
            ".secure.tmp.{}.4242.3",
            crate::SCHEDULER_TEMP_PURPOSE.as_str()
        );
        std::fs::write(runtime_path.join(&own), b"interrupted journal record")?;
        let error = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)
            .err()
            .context("own interrupted publication was ignored")?;
        assert!(error.to_string().contains("requires held recovery"), "{error:#}");
        std::fs::remove_file(runtime_path.join(&own))?;

        std::fs::write(runtime_path.join(".secure.tmp.4242.4"), b"unattributed")?;
        let error = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)
            .err()
            .context("retired-format temporary was accepted")?;
        assert!(
            error.to_string().contains("without an exact owner token"),
            "{error:#}"
        );
        for name in foreign.into_iter().chain([".secure.tmp.4242.4"]) {
            assert!(runtime_path.join(name).is_file(), "{name}");
        }
        Ok(())
    }

    #[test]
    fn completed_child_cannot_suppress_pre_effect_parent_role_artifacts() -> Result<()> {
        for suffix in ["quarantine", "purge", "resolution"] {
            let directory = tempfile::tempdir()?;
            let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
            let runtime_path = directory.path().join("runtime");
            std::fs::create_dir(&runtime_path)?;
            let runtime = lillux::PinnedDirectory::open(&runtime_path)?
                .context("test runtime directory is absent")?;
            completed_recovery_fixture(&runtime, &identity)?;
            assert_eq!(
                RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.len(),
                1
            );
            let parent = "11".repeat(16);
            let name = if suffix == "resolution" {
                format!("scheduler-retirement.resolve.{parent}.main.00.quarantine")
            } else {
                format!("scheduler-retirement.role.{parent}.main.{suffix}")
            };
            assert!(runtime
                .atomic_create_regular(
                    OsStr::new(&name),
                    b"unexpected parent artifact",
                    0o600,
                    crate::SCHEDULER_TEMP_PURPOSE,
                )?
                .is_some());
            assert!(
                RetirementJournal::scan_interrupted_evidence(&runtime, &identity).is_err(),
                "completed child suppressed pre-effect parent {suffix}"
            );
        }
        Ok(())
    }

    #[test]
    fn latest_terminal_r_snapshot_rejects_reappearance_and_scan_race() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let runtime = lillux::PinnedDirectory::open(directory.path())?
            .context("test runtime directory is absent")?;
        let name = format!(
            "scheduler-retirement.resolve.{}.main.00.quarantine",
            "22".repeat(16)
        );
        let absent = SchedulerRetirementObservedCoordinate::Absent;
        ensure_resolution_coordinate_matches(&runtime, &name, &absent, None, false)?;
        assert!(runtime
            .atomic_create_regular(
                OsStr::new(&name),
                b"foreign replacement",
                0o600,
                crate::SCHEDULER_TEMP_PURPOSE,
            )?
            .is_some());
        // Both a stable occupied coordinate and a replacement after the directory
        // snapshot must refuse the signed absence testimony.
        assert!(
            ensure_resolution_coordinate_matches(&runtime, &name, &absent, None, true).is_err()
        );
        assert!(
            ensure_resolution_coordinate_matches(&runtime, &name, &absent, None, false).is_err()
        );
        Ok(())
    }

    #[test]
    fn terminal_recovery_child_cannot_hide_resolution_records_or_orphan_r() -> Result<()> {
        for residue in [
            "unsigned-intent",
            "signed-invalid-intent",
            "orphan-outcome",
            "orphan-r",
        ] {
            let directory = tempfile::tempdir()?;
            let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
            let runtime_path = directory.path().join("runtime");
            std::fs::create_dir(&runtime_path)?;
            let runtime = lillux::PinnedDirectory::open(&runtime_path)?
                .context("test runtime directory is absent")?;
            let child = completed_recovery_fixture(&runtime, &identity)?;
            let evidence = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?;
            assert_eq!(evidence.len(), 1);
            assert_eq!(evidence[0].generation_intent.generation, "33".repeat(16));
            assert!(RetirementJournal::validate_admission(&runtime, &identity).is_err());
            let name = match residue {
                "orphan-r" => format!("scheduler-retirement.resolve.{child}.main.00.quarantine"),
                "orphan-outcome" => {
                    format!("scheduler-retirement.resolution.{child}.main.00.outcome")
                }
                _ => format!("scheduler-retirement.resolution.{child}.main.00.intent"),
            };
            let bytes = if residue == "signed-invalid-intent" {
                sign_record_bytes(&identity, &serde_json::json!({"schema": 99}))?
            } else {
                b"untrusted terminal residue".to_vec()
            };
            assert!(runtime
                .atomic_create_regular(
                    OsStr::new(&name),
                    &bytes,
                    0o600,
                    crate::SCHEDULER_TEMP_PURPOSE,
                )?
                .is_some());
            assert!(
                RetirementJournal::scan_interrupted_evidence(&runtime, &identity).is_err(),
                "terminal child suppressed {residue}"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod interrupted_prefix_presence_controls {
    use super::*;
    use ryeos_scheduler_contract::SchedulerRetirementPhase as Phase;

    fn absent_records() -> Vec<SchedulerRetirementRecord> {
        let expected = [
            (SchedulerRetirementRole::Main, Phase::Captured),
            (SchedulerRetirementRole::Wal, Phase::Captured),
            (SchedulerRetirementRole::Shm, Phase::Captured),
            (SchedulerRetirementRole::Main, Phase::SqlDrainAuthorized),
            (SchedulerRetirementRole::Wal, Phase::SqlDrainAuthorized),
            (SchedulerRetirementRole::Shm, Phase::SqlDrainAuthorized),
            (SchedulerRetirementRole::Main, Phase::SqlDrained),
            (SchedulerRetirementRole::Wal, Phase::SqlDrained),
            (SchedulerRetirementRole::Shm, Phase::SqlDrained),
            (SchedulerRetirementRole::Main, Phase::QuarantineAuthorized),
            (SchedulerRetirementRole::Main, Phase::Quarantined),
            (SchedulerRetirementRole::Main, Phase::PurgeAuthorized),
            (SchedulerRetirementRole::Main, Phase::Purged),
            (SchedulerRetirementRole::Wal, Phase::QuarantineAuthorized),
            (SchedulerRetirementRole::Wal, Phase::Quarantined),
            (SchedulerRetirementRole::Wal, Phase::PurgeAuthorized),
            (SchedulerRetirementRole::Wal, Phase::Purged),
            (SchedulerRetirementRole::Shm, Phase::QuarantineAuthorized),
            (SchedulerRetirementRole::Shm, Phase::Quarantined),
            (SchedulerRetirementRole::Shm, Phase::PurgeAuthorized),
            (SchedulerRetirementRole::Shm, Phase::Purged),
            (SchedulerRetirementRole::Main, Phase::ReleaseAuthorized),
            (SchedulerRetirementRole::Wal, Phase::ReleaseAuthorized),
            (SchedulerRetirementRole::Shm, Phase::ReleaseAuthorized),
            (SchedulerRetirementRole::Main, Phase::Released),
            (SchedulerRetirementRole::Wal, Phase::Released),
            (SchedulerRetirementRole::Shm, Phase::Released),
        ];
        expected
            .into_iter()
            .enumerate()
            .map(|(sequence, (role, phase))| SchedulerRetirementRecord {
                schema: 1,
                purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
                generation: "11".repeat(16),
                sequence: sequence as u8,
                previous_sha256: None,
                owner_fingerprint: String::new(),
                runtime_identity: String::new(),
                role,
                phase,
                role_identity: None,
            })
            .collect()
    }

    fn role_identity(digested: bool) -> SchedulerRetirementRoleIdentity {
        SchedulerRetirementRoleIdentity {
            device: 1,
            inode: 2,
            byte_length: 4096,
            content_sha256: digested.then(|| "ab".repeat(32)),
        }
    }

    #[test]
    fn every_legal_absent_prefix_keeps_missing_evidence_distinct_from_absence() -> Result<()> {
        let records = absent_records();
        for length in 0..=27 {
            validate_phase_prefix(&records[..length])?;
        }
        for role in 0..3 {
            let mut records = absent_records();
            records[role].role_identity = Some(role_identity(false));
            // No drain observation yet: this is still a legal incomplete prefix.
            validate_phase_prefix(&records[..6 + role])?;
            assert!(validate_phase_prefix(&records[..7 + role]).is_err());
            records[role].role_identity = None;
            records[6 + role].role_identity = Some(role_identity(true));
            assert!(validate_phase_prefix(&records[..7 + role]).is_err());
        }
        Ok(())
    }

    #[test]
    fn absent_and_present_roles_cannot_change_at_any_observed_effect_or_release() -> Result<()> {
        for role in 0..3 {
            for sequence in (9 + role * 4..13 + role * 4).chain([21 + role, 24 + role]) {
                let mut absent = absent_records();
                absent[sequence].role_identity = Some(role_identity(true));
                assert!(validate_phase_prefix(&absent[..=sequence]).is_err());

                let mut present = absent_records();
                present[role].role_identity = Some(role_identity(false));
                present[3 + role].role_identity = Some(role_identity(false));
                present[6 + role].role_identity = Some(role_identity(true));
                for phase in (9 + role * 4..13 + role * 4).chain([21 + role, 24 + role]) {
                    present[phase].role_identity = Some(role_identity(true));
                }
                validate_phase_prefix(&present[..=sequence])?;
                present[sequence].role_identity = None;
                assert!(validate_phase_prefix(&present[..=sequence]).is_err());
            }
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod guarded_journal_read_controls {
    use super::*;

    #[test]
    fn signed_evidence_and_phase_probe_refuse_live_role_aliases() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let runtime_path = directory.path().join("runtime");
        std::fs::create_dir(&runtime_path)?;
        let runtime = std::sync::Arc::new(
            lillux::PinnedDirectory::open(&runtime_path)?
                .context("test runtime directory is absent")?,
        );
        // The live scheduler owner is one armed bound ticket of the test
        // host, holding its WAL sidecars open across every aliased read.
        let host = lillux_sqlite::test_host::TestNativeHost::acquire();
        let owner = match host.open_bound(
            runtime.clone(),
            OsStr::new("scheduler.sqlite3"),
            lillux_sqlite::OriginalNativeBoundMode::CreateExclusive,
            lillux_sqlite::OriginalNativeDomain::NodeScheduler,
        ) {
            Ok(owner) => owner,
            Err(failure) => {
                let error = failure.to_string();
                host.retire_open_failure(failure);
                panic!("test scheduler owner open refused: {error}");
            }
        };
        let probed = owner.with_sql(|connection| {
            Ok(connection.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE guard_probe(value INTEGER); INSERT INTO guard_probe VALUES(7);",
            )?)
        });
        let outcome = probed
            .and_then(|()| owner.establish_scheduler_wal_binding())
            .and_then(|()| refuse_live_role_aliases(&runtime_path, &runtime, &identity, &owner));
        host.close(owner);
        outcome
    }

    /// Alias each live role of `owner` under journal intent and phase names
    /// and require every guarded read to refuse it while the owner's keeper
    /// and its bytes stay intact.
    fn refuse_live_role_aliases(
        runtime_path: &std::path::Path,
        runtime: &std::sync::Arc<lillux::PinnedDirectory>,
        identity: &NodeIdentity,
        owner: &lillux_sqlite::OriginalNativeConnection,
    ) -> Result<()> {
        for (index, role) in [
            "scheduler.sqlite3",
            "scheduler.sqlite3-wal",
            "scheduler.sqlite3-shm",
        ]
        .into_iter()
        .enumerate()
        {
            let generation = format!("{:032x}", index + 1);
            let intent_name = format!("{INTENT_PREFIX}{generation}");
            let phase = phase_name(&generation, 0);
            std::fs::hard_link(runtime_path.join(role), runtime_path.join(&intent_name))?;
            std::fs::hard_link(runtime_path.join(role), runtime_path.join(&phase))?;
            // Refusal must occur in the byte intake and phase-presence path,
            // before the aliased SQLite bytes reach signature decoding.
            let error = read_signed_bytes(runtime, &intent_name).unwrap_err();
            assert!(error
                .to_string()
                .contains("read guarded scheduler retirement record"));
            assert!(load_record_chain(runtime, identity, &generation).is_err());
            assert!(RetirementJournal::scan_interrupted_evidence(runtime, identity).is_err());
            owner.require_scheduler_runtime(runtime)?;
            let value: i64 = owner.with_sql(|connection| {
                Ok(connection.query_row("SELECT value FROM guard_probe", [], |row| row.get(0))?)
            })?;
            assert_eq!(value, 7);
        }
        Ok(())
    }

    #[test]
    fn guarded_optional_evidence_keeps_absence_nonregular_and_size_distinct() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let runtime = lillux::PinnedDirectory::open(directory.path())?
            .context("test runtime directory is absent")?;
        assert!(read_optional_signed_bytes(&runtime, "absent")?.is_none());
        assert!(read_signed_bytes(&runtime, "absent").is_err());
        std::fs::create_dir(directory.path().join("nonregular"))?;
        assert!(read_optional_signed_bytes(&runtime, "nonregular").is_err());
        let limit = ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES;
        let exact = vec![b'x'; limit];
        assert!(runtime
            .atomic_create_regular(
                OsStr::new("exact"),
                &exact,
                0o600,
                crate::SCHEDULER_TEMP_PURPOSE,
            )?
            .is_some());
        assert_eq!(read_signed_bytes(&runtime, "exact")?, exact);
        assert!(runtime
            .atomic_create_regular(
                OsStr::new("oversized"),
                &vec![b'x'; limit + 1],
                0o600,
                crate::SCHEDULER_TEMP_PURPOSE,
            )?
            .is_some());
        assert!(read_optional_signed_bytes(&runtime, "oversized").is_err());
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod compact_native_projection_controls {
    use super::*;
    use lillux_sqlite::{SqliteRole as Role, SqliteRoleCoordinateObservation as Coordinate};

    pub(crate) fn native_observation(
        runtime_identity: lillux::PinnedDirectoryIdentity,
    ) -> lillux_sqlite::SqliteRoleResolutionObservation {
        let generation = [4; 16];
        let generations = (1..=4).map(|i| [i; 16]).collect::<Vec<_>>();
        let generation_hex = "04".repeat(16);
        let roles = [Role::Main, Role::Wal, Role::Shm];
        let public_names = [
            "scheduler.sqlite3",
            "scheduler.sqlite3-wal",
            "scheduler.sqlite3-shm",
        ];
        let identity = |index: usize| lillux_sqlite::SqliteRoleIdentity {
            device: u64::MAX,
            inode: u64::MAX - index as u64,
            byte_length: u64::MAX,
            content_sha256: Some("ab".repeat(32)),
        };
        let quarantine_name = format!("scheduler-retirement.role.{generation_hex}.main.quarantine");
        let mut stopped_coordinates = Vec::new();
        for (index, role) in roles.into_iter().enumerate() {
            let role_name = ["main", "wal", "shm"][index];
            let mut names = vec![public_names[index].to_owned()];
            for id in &generations {
                let id = id
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                names.extend(
                    ["quarantine", "purge"].map(|suffix| {
                        format!("scheduler-retirement.role.{id}.{role_name}.{suffix}")
                    }),
                );
                names.extend((0..4).map(|ordinal| {
                    format!("scheduler-retirement.resolve.{id}.{role_name}.{ordinal:02}.quarantine")
                }));
            }
            for name in names {
                let present = (index == 0 && name == quarantine_name)
                    || (index > 0 && name == public_names[index]);
                stopped_coordinates.push(lillux_sqlite::StoppedSqliteCoordinateObservation {
                    role: Some(role),
                    name: name.clone(),
                    observation: if present {
                        Coordinate::SameOriginal {
                            device: u64::MAX,
                            inode: u64::MAX - index as u64,
                        }
                    } else {
                        Coordinate::Absent
                    },
                });
                if role == Role::Main {
                    stopped_coordinates.push(lillux_sqlite::StoppedSqliteCoordinateObservation {
                        role: None,
                        name: format!("{name}-journal"),
                        observation: Coordinate::Absent,
                    });
                }
            }
        }
        let failure = lillux_sqlite::SqliteRoleMutationFailureObservation {
            operation: lillux_sqlite::SqliteRoleMutationOperation::Quarantine,
            error: lillux_sqlite::SqliteRoleMutationError {
                role: Role::Main,
                disposition: lillux::secure_fs::PlannedQuarantineDisposition::DurabilityUncertain,
                stage: lillux::secure_fs::PlannedQuarantineStage::QuarantinedOriginal,
                message: "actual retained directory synchronization failure".repeat(8),
            },
            substep_observed: true,
        };
        let mut original = identity(0);
        original.content_sha256 = None;
        lillux_sqlite::SqliteRoleResolutionObservation {
            generation,
            role: Role::Main,
            runtime_identity,
            epoch: u64::MAX - 1,
            barriers_held: true,
            recovery_required: true,
            original: Some(original),
            original_link_count: Some(u64::MAX),
            public: Coordinate::Absent,
            source: Coordinate::Absent,
            selected_source: Some(lillux_sqlite::StoppedSqliteRoleSource::Canonical),
            stopped_coordinates,
            stopped_plan: Some(lillux_sqlite::StoppedSqliteProjectionPlan {
                generation,
                generations,
                runtime_identity,
                roles: std::array::from_fn(|index| lillux_sqlite::StoppedSqliteRolePlan {
                    source: lillux_sqlite::StoppedSqliteRoleSource::Canonical,
                    current: Some(identity(index)),
                }),
            }),
            quarantine: Coordinate::SameOriginal {
                device: u64::MAX,
                inode: u64::MAX,
            },
            purge: Coordinate::Absent,
            quarantine_name,
            purge_name: format!("scheduler-retirement.role.{generation_hex}.main.purge"),
            original_failure: Some(failure.clone()),
            latest_failure: Some(failure),
            resolutions: (0..3)
                .map(|ordinal| lillux_sqlite::SqliteRoleResolutionAttempt {
                    ordinal,
                    action: lillux_sqlite::SqliteRoleResolutionAction::SyncQuarantine,
                    outcome: None,
                    effect_failure: None,
                    substep: None,
                    original_unlink_observed: false,
                    error: Some("actual prior held synchronization diagnostic".repeat(8)),
                })
                .collect(),
            resolution_coordinates: (0..3)
                .map(|ordinal| lillux_sqlite::SqliteRoleResolutionCoordinate {
                    ordinal,
                    name: format!(
                        "scheduler-retirement.resolve.{generation_hex}.main.{ordinal:02}.quarantine"
                    ),
                    observation: Coordinate::Absent,
                })
                .collect(),
        }
    }

    fn begin_fixture<'a>(
        runtime: &'a lillux::PinnedDirectory,
        identity: &'a NodeIdentity,
        generation: [u8; 16],
    ) -> Result<RetirementJournal<'a>> {
        let generation = generation
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        RetirementJournal::begin_with_intent(runtime, identity, {
            let mut intent = SchedulerRetirementIntent {
                schema: 3,
                purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
                operation_id: generation.clone(),
                operation_sha256: "aa".repeat(32),
                scheduler_accounting:
                    ryeos_scheduler_contract::SchedulerFireHistoryAccounting::Exact {
                        counts: Default::default(),
                    },
                scheduler_sources: std::array::from_fn(|_| SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Absent,
                    current: None,
                }),
                generation,
                owner_fingerprint: identity.fingerprint().to_owned(),
                runtime_identity: serde_json::to_string(&runtime.identity()?)?,
                roles: [None, None, None],
                intake_mode: "stopped-original".to_owned(),
                stopped_plan: None,
                recovery: None,
            };
            intent.stopped_plan = Some(SchedulerRetirementStoppedPlan {
                generation: intent.generation.clone(),
                generations: vec![intent.generation.clone()],
                runtime_identity: intent.runtime_identity.clone(),
                roles: intent.scheduler_sources.clone(),
            });
            intent
        })
    }

    #[test]
    fn pre_effect_capacity_reserves_all_generation_remainders_and_staged_operation_bytes(
    ) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let runtime =
            lillux::PinnedDirectory::open(directory.path())?.context("test runtime absent")?;
        RetirementJournal::reserve_pre_effect_capacity(&runtime, None, true)?;
        // Synthetic bounded evidence names are adversarial inventory only;
        // this capacity API does not authenticate records or issue a permit.
        for index in 1..=4 {
            let name = format!("{INTENT_PREFIX}{index:032x}");
            assert!(runtime
                .atomic_create_regular(
                    OsStr::new(&name),
                    b"untrusted inventory",
                    0o600,
                    crate::SCHEDULER_TEMP_PURPOSE,
                )?
                .is_some());
        }
        RetirementJournal::reserve_pre_effect_capacity(&runtime, None, false)?;
        assert!(RetirementJournal::reserve_pre_effect_capacity(&runtime, None, true).is_err());
        // Four complete maximum-generation reservations consume228 slots;
        //19 existing operation records plus9 op/cleanup margins exactly fill
        //256. The20th refuses even though present evidence bytes are tiny.
        for index in 1..=19 {
            let name = format!("{ORIGINAL_OPERATION_PREFIX}{index:032x}");
            assert!(runtime
                .atomic_create_regular(
                    OsStr::new(&name),
                    b"untrusted operation inventory",
                    0o600,
                    crate::SCHEDULER_TEMP_PURPOSE,
                )?
                .is_some());
        }
        RetirementJournal::reserve_pre_effect_capacity(&runtime, None, false)?;
        let name = format!("{ORIGINAL_OPERATION_PREFIX}{:032x}", 20);
        assert!(runtime
            .atomic_create_regular(
                OsStr::new(&name),
                b"untrusted operation inventory",
                0o600,
                crate::SCHEDULER_TEMP_PURPOSE,
            )?
            .is_some());
        let before = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        assert!(RetirementJournal::reserve_pre_effect_capacity(&runtime, None, false).is_err());
        assert_eq!(runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?, before);
        Ok(())
    }

    fn first_resolution_intent(
        native: &lillux_sqlite::SqliteRoleResolutionObservation,
        identity: &NodeIdentity,
    ) -> Result<SchedulerRetirementResolutionIntent> {
        let expected = native_resolution_projection(native)?;
        let intent = SchedulerRetirementResolutionIntent {
            schema: 2,
            operation_id: expected.generation.clone(),
            operation_sha256: "aa".repeat(32),
            purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE.to_owned(),
            generation: expected.generation.clone(),
            role: expected.role,
            ordinal: 0,
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: expected.runtime_identity.clone(),
            parent_journal_sha256: "cd".repeat(32),
            previous_outcome_sha256: None,
            expected,
            action: SchedulerRetirementResolutionAction::SyncQuarantine,
        };
        intent.validate()?;
        Ok(intent)
    }

    #[test]
    fn signed_original_evidence_substitution_preserves_live_reservation_and_phase_cursor(
    ) -> Result<()> {
        for substitution in ["id", "hash", "accounting", "source"] {
            let directory = tempfile::tempdir()?;
            let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
            let runtime_path = directory.path().join("runtime");
            std::fs::create_dir(&runtime_path)?;
            let runtime =
                lillux::PinnedDirectory::open(&runtime_path)?.context("test runtime absent")?;
            let mut native = native_observation(runtime.identity()?);
            native.resolutions.clear();
            native.resolution_coordinates.clear();
            let mut journal = begin_fixture(&runtime, &identity, [4; 16])?;
            journal.previous_hash = Some("cd".repeat(32));
            let generation_name = format!("{INTENT_PREFIX}{}", journal.generation);
            let mut original = journal.generation_intent.clone();
            original.scheduler_sources = native_resolution_projection(&native)?
                .stopped_plan
                .unwrap()
                .roles;
            original.scheduler_accounting = ryeos_scheduler_contract::SchedulerFireHistoryAccounting::UnavailableIncompatibleSchema {};
            original.recovery = Some(SchedulerRetirementRecoveryBinding {
                parent_generation: "01".repeat(16),
                parent_intent_sha256: "cd".repeat(32),
                parent_phase_sha256: vec!["ef".repeat(32); 9],
                parent_resolutions: Vec::new(),
                stopped_plan: native_resolution_projection(&native)?.stopped_plan.unwrap(),
            });
            original.intake_mode = "stopped-recovery".to_owned();
            original.stopped_plan = None;
            original.validate()?;
            journal.generation_intent = original.clone();
            let intent = first_resolution_intent(&native, &identity)?;
            intent.validate_for_generation(&original)?;
            let signed_intent = sign_record_bytes(&identity, &intent)?;
            let mut substituted = original.clone();
            match substitution {
                "id" => substituted.operation_id = "ff".repeat(16),
                "hash" => substituted.operation_sha256 = "ff".repeat(32),
                "accounting" => {
                    substituted.scheduler_accounting =
                        ryeos_scheduler_contract::SchedulerFireHistoryAccounting::Exact {
                            counts: ryeos_scheduler_contract::SchedulerFireHistoryDiscardReport {
                                fires: 19,
                                outbox_snapshots: 4,
                                cursors: 1,
                            },
                        }
                }
                _ => {
                    substituted.scheduler_sources[0]
                        .current
                        .as_mut()
                        .unwrap()
                        .content_sha256 = Some("ff".repeat(32))
                }
            }
            substituted.validate()?;
            // The replacement is genuinely signed under this fixture's own
            // node key and independently valid, but differs from the evidence
            // memoized at this journal's original admission.
            std::fs::write(
                runtime_path.join(&generation_name),
                sign_record_bytes(&identity, &substituted)?,
            )?;
            let names_before = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
            for durable in [None, Some(false), Some(true)] {
                let pending = durable.map(|intent_is_durable| ResolutionReservation {
                    intent: intent.clone(),
                    signed_intent_sha256: lillux::sha256_hex(&signed_intent),
                    signed_intent: signed_intent.clone(),
                    host_expected: native.clone(),
                    intent_is_durable,
                });
                journal.pending_resolution[0] = pending.clone();
                journal.next_resolution_ordinal[0] = u8::from(durable.is_some());
                let error = journal
                    .authorize_resolution(
                        SchedulerRetirementRole::Main,
                        native.clone(),
                        SchedulerRetirementResolutionAction::SyncQuarantine,
                    )
                    .err()
                    .context("live reservation accepted changed original evidence")?;
                assert!(
                    error
                        .to_string()
                        .contains("changed its retained original operation evidence"),
                    "{error:#}"
                );
                assert!(journal.pending_resolution[0] == pending);
                assert_eq!(
                    journal.next_resolution_ordinal,
                    [u8::from(durable.is_some()), 0, 0]
                );
                assert_eq!(journal.resolution_record_count, 0);
                assert_eq!(journal.next_sequence, 0);
                assert_eq!(
                    journal.previous_hash.as_deref(),
                    Some(intent.parent_journal_sha256.as_str())
                );
                assert_eq!(journal.generation_intent, original);
                assert_eq!(
                    runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?,
                    names_before
                );
            }
            assert!(journal
                .append(
                    SchedulerRetirementRole::Main,
                    ryeos_scheduler_contract::SchedulerRetirementPhase::Captured,
                    None
                )
                .is_err());
            assert_eq!(journal.next_sequence, 0);
            assert_eq!(
                runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?,
                names_before
            );
        }
        Ok(())
    }

    #[test]
    fn live_generation_join_refuses_before_reservation_and_preserves_pending_bytes() -> Result<()> {
        for substitution in ["ordinary", "shorter-plan", "selected-source"] {
            let directory = tempfile::tempdir()?;
            let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
            let runtime_path = directory.path().join("runtime");
            std::fs::create_dir(&runtime_path)?;
            let runtime =
                lillux::PinnedDirectory::open(&runtime_path)?.context("test runtime absent")?;
            let mut native = native_observation(runtime.identity()?);
            native.resolutions.clear();
            native.resolution_coordinates.clear();
            let mut journal = begin_fixture(&runtime, &identity, [4; 16])?;
            journal.previous_hash = Some("cd".repeat(32));
            let generation_name = format!("{INTENT_PREFIX}{}", journal.generation);
            let mut generation: SchedulerRetirementIntent =
                read_signed(&runtime, &generation_name, &identity)?;
            if substitution != "ordinary" {
                generation.recovery = Some(SchedulerRetirementRecoveryBinding {
                    parent_generation: "01".repeat(16),
                    parent_intent_sha256: "cd".repeat(32),
                    parent_phase_sha256: vec!["ef".repeat(32); 9],
                    parent_resolutions: Vec::new(),
                    stopped_plan: native_resolution_projection(&native)?.stopped_plan.unwrap(),
                });
                generation.intake_mode = "stopped-recovery".to_owned();
                generation.stopped_plan = None;
                generation.validate()?;
                // Replace only this fixture's own generation record with a
                // signed recovery intent; live authorization must read that
                // record rather than trust the observation's chosen plan.
                std::fs::write(
                    runtime_path.join(&generation_name),
                    sign_record_bytes(&identity, &generation)?,
                )?;
                journal.generation_intent = generation.clone();
                if substitution == "shorter-plan" {
                    native.stopped_plan.as_mut().unwrap().generations.remove(1);
                    let mut shorter_plan =
                        generation.recovery.as_ref().unwrap().stopped_plan.clone();
                    shorter_plan.generations.remove(1);
                    let names = shorter_plan.expected_coordinate_names();
                    native
                        .stopped_coordinates
                        .retain(|item| names.contains(&item.name));
                } else {
                    let source = lillux_sqlite::StoppedSqliteRoleSource::Quarantine {
                        generation: [1; 16],
                    };
                    native.stopped_plan.as_mut().unwrap().roles[0].source = source.clone();
                    native.selected_source = Some(source);
                }
            }
            let intent = first_resolution_intent(&native, &identity)?;
            let refusal =
                "scheduler resolution stopped plan/source differs from authenticated intake";
            let names_before = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
            let error = journal
                .authorize_resolution(
                    SchedulerRetirementRole::Main,
                    native.clone(),
                    SchedulerRetirementResolutionAction::SyncQuarantine,
                )
                .err()
                .context("live join accepted substituted plan")?;
            assert!(error.to_string().contains(refusal), "{error:#}");
            assert_eq!(journal.next_resolution_ordinal, [0; 3]);
            assert!(journal.pending_resolution.iter().all(Option::is_none));
            assert_eq!(journal.resolution_record_count, 0);
            assert_eq!(
                runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?,
                names_before
            );

            // Both a previously durable intent and an uncertain publication
            // must be refused without losing the original memoized reservation.
            let signed_intent = sign_record_bytes(&identity, &intent)?;
            for intent_is_durable in [false, true] {
                let pending = ResolutionReservation {
                    intent: intent.clone(),
                    signed_intent_sha256: lillux::sha256_hex(&signed_intent),
                    signed_intent: signed_intent.clone(),
                    host_expected: native.clone(),
                    intent_is_durable,
                };
                journal.pending_resolution[0] = Some(pending.clone());
                journal.next_resolution_ordinal[0] = 1;
                journal.resolution_record_count = usize::from(intent_is_durable);
                let error = journal
                    .authorize_resolution(
                        SchedulerRetirementRole::Main,
                        native.clone(),
                        SchedulerRetirementResolutionAction::SyncQuarantine,
                    )
                    .err()
                    .context("pending join accepted substituted plan")?;
                assert!(error.to_string().contains(refusal), "{error:#}");
                assert!(journal.pending_resolution[0].as_ref() == Some(&pending));
                assert_eq!(journal.next_resolution_ordinal, [1, 0, 0]);
                assert_eq!(
                    journal.resolution_record_count,
                    usize::from(intent_is_durable)
                );
                assert_eq!(
                    journal.previous_hash.as_deref(),
                    Some(intent.parent_journal_sha256.as_str())
                );
                assert_eq!(
                    runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?,
                    names_before
                );
            }
        }
        Ok(())
    }

    #[test]
    fn scanner_and_admission_refuse_signed_stopped_plan_in_ordinary_generation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let runtime_path = directory.path().join("runtime");
        std::fs::create_dir(&runtime_path)?;
        let runtime =
            lillux::PinnedDirectory::open(&runtime_path)?.context("test runtime absent")?;
        let _journal = begin_fixture(&runtime, &identity, [4; 16])?;
        assert_eq!(
            RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.len(),
            1
        );
        let mut native = native_observation(runtime.identity()?);
        native.resolutions.clear();
        native.resolution_coordinates.clear();
        let intent = first_resolution_intent(&native, &identity)?;
        publish_signed(&runtime, &identity, &intent.intent_filename()?, &intent)?;
        let names_before = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        let error = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)
            .err()
            .context("scanner accepted stopped facts in ordinary generation")?;
        assert!(
            error.to_string().contains(
                "scheduler resolution stopped plan/source differs from authenticated intake"
            ),
            "{error:#}"
        );
        assert_eq!(
            runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?,
            names_before
        );
        assert_eq!(
            read_signed::<SchedulerRetirementResolutionIntent>(
                &runtime,
                &intent.intent_filename()?,
                &identity,
            )?,
            intent
        );
        let mut after = native;
        after.epoch = u64::MAX;
        after.recovery_required = false;
        after
            .resolutions
            .push(lillux_sqlite::SqliteRoleResolutionAttempt {
                ordinal: 0,
                action: lillux_sqlite::SqliteRoleResolutionAction::SyncQuarantine,
                outcome: Some(lillux_sqlite::SqliteRoleResolutionOutcome::QuarantinedDurable),
                effect_failure: None,
                substep: None,
                original_unlink_observed: false,
                error: None,
            });
        after
            .resolution_coordinates
            .push(lillux_sqlite::SqliteRoleResolutionCoordinate {
                ordinal: 0,
                name: intent.quarantine_filename()?,
                observation: Coordinate::Absent,
            });
        let intent_sha256 =
            lillux::sha256_hex(&read_signed_bytes(&runtime, &intent.intent_filename()?)?);
        let outcome = SchedulerRetirementResolutionOutcome {
            schema: 1,
            purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE.to_owned(),
            generation: intent.generation.clone(),
            role: intent.role,
            ordinal: 0,
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: intent.runtime_identity.clone(),
            intent_sha256: intent_sha256.clone(),
            state: SchedulerRetirementResolutionState::QuarantinedDurable,
            observed: native_resolution_projection(&after)?,
        };
        // A coherent intent/outcome pair cannot supply generation authority.
        outcome.validate_for_intent(&intent, &intent_sha256)?;
        publish_signed(&runtime, &identity, &intent.outcome_filename()?, &outcome)?;
        let names_before = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
        let error = RetirementJournal::preflight(&runtime, &identity)
            .err()
            .context("admission accepted stopped facts in ordinary generation")?;
        assert!(
            error.to_string().contains(
                "scheduler resolution stopped plan/source differs from authenticated intake"
            ),
            "{error:#}"
        );
        assert_eq!(
            runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?,
            names_before
        );
        Ok(())
    }

    #[test]
    fn full_native_inventory_and_signed_intent_outcome_fit_existing_envelope() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let runtime =
            lillux::PinnedDirectory::open(directory.path())?.context("test runtime absent")?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let native = native_observation(runtime.identity()?);
        let observation = native_resolution_projection(&native)?;
        assert_eq!(observation.stopped_coordinates.len(), 100);
        let intent = SchedulerRetirementResolutionIntent {
            schema: 2,
            operation_id: observation.generation.clone(),
            operation_sha256: "aa".repeat(32),
            purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE.to_owned(),
            generation: observation.generation.clone(),
            role: observation.role,
            ordinal: 3,
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: observation.runtime_identity.clone(),
            parent_journal_sha256: "cd".repeat(32),
            previous_outcome_sha256: Some("ef".repeat(32)),
            action: SchedulerRetirementResolutionAction::SyncQuarantine,
            expected: observation.clone(),
        };
        intent.validate()?;
        let signed_intent = sign_record_bytes(&identity, &intent)?;
        assert!(
            signed_intent.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES
        );
        assert_eq!(
            verify_signed::<SchedulerRetirementResolutionIntent>(&signed_intent, &identity)?,
            intent
        );
        let mut after = native.clone();
        after.epoch = u64::MAX;
        after.recovery_required = false;
        after
            .resolutions
            .push(lillux_sqlite::SqliteRoleResolutionAttempt {
                ordinal: 3,
                action: lillux_sqlite::SqliteRoleResolutionAction::SyncQuarantine,
                outcome: Some(lillux_sqlite::SqliteRoleResolutionOutcome::QuarantinedDurable),
                effect_failure: None,
                substep: None,
                original_unlink_observed: false,
                error: None,
            });
        after
            .resolution_coordinates
            .push(lillux_sqlite::SqliteRoleResolutionCoordinate {
                ordinal: 3,
                name: format!(
                    "scheduler-retirement.resolve.{}.main.03.quarantine",
                    observation.generation
                ),
                observation: Coordinate::Absent,
            });
        let outcome = SchedulerRetirementResolutionOutcome {
            schema: 1,
            purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE.to_owned(),
            generation: observation.generation.clone(),
            role: observation.role,
            ordinal: 3,
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: observation.runtime_identity.clone(),
            intent_sha256: lillux::sha256_hex(&signed_intent),
            state: SchedulerRetirementResolutionState::QuarantinedDurable,
            observed: native_resolution_projection(&after)?,
        };
        outcome.validate_for_intent(&intent, &outcome.intent_sha256)?;
        assert!(!outcome.observed.recovery_required);
        assert!(outcome
            .observed
            .stopped_coordinates
            .iter()
            .any(|item| item.name == "scheduler.sqlite3-wal"
                && matches!(
                    item.coordinate,
                    SchedulerRetirementObservedCoordinate::SameOriginal { .. }
                )));
        let mut false_state = outcome.clone();
        false_state.observed.recovery_required = true;
        assert!(false_state
            .validate_for_intent(&intent, &outcome.intent_sha256)
            .is_err());
        let signed_outcome = sign_record_bytes(&identity, &outcome)?;
        assert!(
            signed_outcome.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES
        );
        assert_eq!(
            verify_signed::<SchedulerRetirementResolutionOutcome>(&signed_outcome, &identity)?,
            outcome
        );
        let mut swapped = native.clone();
        swapped.stopped_coordinates[0].role = Some(Role::Wal);
        assert!(native_resolution_projection(&swapped).is_err());
        let mut truncated = native.clone();
        truncated.stopped_coordinates.pop();
        assert!(native_resolution_projection(&truncated).is_err());
        let mut foreign = native;
        foreign.stopped_coordinates[0].observation = Coordinate::ForeignRegular {
            device: 1,
            inode: 2,
        };
        assert!(native_resolution_projection(&foreign).is_err());
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn fixture_signed_file_hash(
    runtime: &lillux::PinnedDirectory,
    name: &str,
) -> Result<String> {
    Ok(lillux::sha256_hex(&read_signed_bytes(runtime, name)?))
}

#[cfg(test)]
pub(crate) fn fixture_publish_resolution(
    runtime: &lillux::PinnedDirectory,
    identity: &NodeIdentity,
    generation: &SchedulerRetirementIntent,
    intent: &SchedulerRetirementResolutionIntent,
) -> Result<()> {
    intent.validate_for_generation(generation)?;
    publish_signed(runtime, identity, &intent.intent_filename()?, intent)?;
    Ok(())
}

// These controls publish genuine signed fixtures but never mint an App permit.
// They are authored UNRUN under this source-only allocation.
#[cfg(test)]
pub(crate) mod late_recovery_controls {
    use super::*;
    use ryeos_scheduler_contract::SchedulerRetirementPhase as Phase;

    pub(crate) fn signed_prefix(
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
        intent: &SchedulerRetirementIntent,
        count: usize,
    ) -> Result<()> {
        publish_signed(
            runtime,
            identity,
            &format!("{INTENT_PREFIX}{}", intent.generation),
            intent,
        )?;
        let sources = intent
            .recovery
            .as_ref()
            .map_or(&intent.scheduler_sources, |binding| {
                &binding.stopped_plan.roles
            });
        let mut previous = None;
        for sequence in 0..count {
            let (role, phase) = match sequence {
                0..=2 => (SchedulerRetirementRole::ALL[sequence], Phase::Captured),
                3..=5 => (
                    SchedulerRetirementRole::ALL[sequence - 3],
                    Phase::SqlDrainAuthorized,
                ),
                6..=8 => (
                    SchedulerRetirementRole::ALL[sequence - 6],
                    Phase::SqlDrained,
                ),
                9..=20 => (
                    SchedulerRetirementRole::ALL[(sequence - 9) / 4],
                    [
                        Phase::QuarantineAuthorized,
                        Phase::Quarantined,
                        Phase::PurgeAuthorized,
                        Phase::Purged,
                    ][(sequence - 9) % 4],
                ),
                21..=23 => (
                    SchedulerRetirementRole::ALL[sequence - 21],
                    Phase::ReleaseAuthorized,
                ),
                24..=26 => (SchedulerRetirementRole::ALL[sequence - 24], Phase::Released),
                27 => (SchedulerRetirementRole::Main, Phase::Complete),
                _ => anyhow::bail!("fixture exceeds dense phase protocol"),
            };
            let mut role_identity = sources[role_index(role)].current.clone();
            if sequence < 6 {
                if let Some(identity) = &mut role_identity {
                    identity.content_sha256 = None;
                }
            }
            let record = SchedulerRetirementRecord {
                schema: 1,
                purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
                generation: intent.generation.clone(),
                sequence: u8::try_from(sequence)?,
                previous_sha256: previous,
                owner_fingerprint: identity.fingerprint().to_owned(),
                runtime_identity: intent.runtime_identity.clone(),
                role,
                phase,
                role_identity,
            };
            let bytes = publish_signed(
                runtime,
                identity,
                &phase_name(&intent.generation, record.sequence),
                &record,
            )?;
            previous = Some(lillux::sha256_hex(&bytes));
        }
        Ok(())
    }

    pub(crate) fn initial_intent(
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
        roles: [SchedulerRetirementStoppedRolePlan; 3],
    ) -> Result<SchedulerRetirementIntent> {
        let mut intent = SchedulerRetirementIntent {
            schema: 3, operation_id: "11".repeat(16), operation_sha256: "aa".repeat(32),
            scheduler_accounting: ryeos_scheduler_contract::SchedulerFireHistoryAccounting::UnavailableIncompatibleSchema {},
            scheduler_sources: roles, purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
            generation: "11".repeat(16), owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: serde_json::to_string(&runtime.identity()?)?, roles: [None, None, None], intake_mode: "stopped-original".to_owned(), stopped_plan: None,
            recovery: None,
        };
        intent.stopped_plan = Some(SchedulerRetirementStoppedPlan {
            generation: intent.generation.clone(),
            generations: vec![intent.generation.clone()],
            runtime_identity: intent.runtime_identity.clone(),
            roles: intent.scheduler_sources.clone(),
        });
        intent.validate()?;
        Ok(intent)
    }

    #[test]
    fn schema3_original_and_full_recovery_intents_fit_existing_signed_envelope() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let runtime =
            lillux::PinnedDirectory::open(directory.path())?.context("fixture runtime absent")?;
        let sources = std::array::from_fn(|index| SchedulerRetirementStoppedRolePlan {
            source: SchedulerRetirementStoppedSource::Canonical,
            current: Some(SchedulerRetirementRoleIdentity {
                device: u64::MAX,
                inode: u64::MAX - index as u64,
                byte_length: u64::MAX,
                content_sha256: Some("ab".repeat(32)),
            }),
        });
        let mut original = initial_intent(&runtime, &identity, sources)?;
        let generations = ["11", "22", "33", "44"].map(|id| id.repeat(16)).to_vec();
        original.stopped_plan.as_mut().unwrap().generations = generations.clone();
        original.validate()?;
        let signed = sign_record_bytes(&identity, &original)?;
        assert!(signed.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES);
        assert_eq!(
            verify_signed::<SchedulerRetirementIntent>(&signed, &identity)?,
            original
        );
        let mut child = original.clone();
        child.generation = generations[3].clone();
        child.intake_mode = "stopped-recovery".to_owned();
        child.stopped_plan = None;
        let plan = SchedulerRetirementStoppedPlan {
            generation: child.generation.clone(),
            generations,
            runtime_identity: child.runtime_identity.clone(),
            // Worst permitted wire shape only; parent hashes are format
            // fixtures, not authenticated phase/mutation authority.
            roles: std::array::from_fn(|index| SchedulerRetirementStoppedRolePlan {
                source: SchedulerRetirementStoppedSource::Resolution {
                    generation: original.generation.clone(),
                    ordinal: 3,
                },
                current: original.scheduler_sources[index].current.clone(),
            }),
        };
        child.recovery = Some(SchedulerRetirementRecoveryBinding {
            parent_generation: original.generation.clone(),
            parent_intent_sha256: "cd".repeat(32),
            parent_phase_sha256: vec!["ef".repeat(32); 27],
            parent_resolutions: SchedulerRetirementRole::ALL
                .into_iter()
                .flat_map(|role| {
                    (0..4).map(move |ordinal| SchedulerRetirementRecoveryResolutionRef {
                        role,
                        ordinal,
                        intent_sha256: "ab".repeat(32),
                        outcome_sha256: Some("cd".repeat(32)),
                    })
                })
                .collect(),
            stopped_plan: plan,
        });
        child.validate()?;
        let signed = sign_record_bytes(&identity, &child)?;
        assert!(signed.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RECORD_MAX_BYTES);
        assert_eq!(
            verify_signed::<SchedulerRetirementIntent>(&signed, &identity)?,
            child
        );
        Ok(())
    }

    #[test]
    fn original_stopped_all_early_prefixes_bind_copied_plan_and_hold_older_intake() -> Result<()> {
        for count in 0..=9 {
            let directory = tempfile::tempdir()?;
            let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
            let path = directory.path().join("runtime");
            std::fs::create_dir(&path)?;
            let runtime =
                lillux::PinnedDirectory::open(&path)?.context("fixture runtime absent")?;
            let sources = std::array::from_fn(|index| SchedulerRetirementStoppedRolePlan {
                source: SchedulerRetirementStoppedSource::Canonical,
                current: Some(SchedulerRetirementRoleIdentity {
                    device: 17,
                    inode: 100 + index as u64,
                    byte_length: 29,
                    content_sha256: Some("ab".repeat(32)),
                }),
            });
            let intent = initial_intent(&runtime, &identity, sources)?;
            signed_prefix(&runtime, &identity, &intent, count)?;
            let observed = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?;
            assert_eq!(observed.len(), 1);
            assert_eq!(observed[0].phase_records.len(), count);
            assert_eq!(
                observed[0].generation_intent.stopped_plan,
                intent.stopped_plan
            );
            let name = format!("{INTENT_PREFIX}{}", intent.generation);
            let mut old = serde_json::to_value(&intent)?;
            old["schema"] = serde_json::json!(2);
            old.as_object_mut().unwrap().remove("intake_mode");
            old.as_object_mut().unwrap().remove("stopped_plan");
            let old_signed = sign_record_bytes(&identity, &old)?;
            std::fs::write(path.join(&name), &old_signed)?; // owned adversarial predecessor fixture
            let before = runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?;
            assert!(RetirementJournal::scan_interrupted_evidence(&runtime, &identity).is_err());
            assert!(RetirementJournal::validate_admission(&runtime, &identity).is_err());
            assert_eq!(runtime.entry_names_bounded(MAX_DIRECTORY_ENTRIES)?, before);
            assert_eq!(read_signed_bytes(&runtime, &name)?, old_signed);
        }
        Ok(())
    }

    #[test]
    fn pending_signed_resolution_is_conserved_through_terminal_descendant() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let path = directory.path().join("runtime");
        std::fs::create_dir(&path)?;
        let runtime = lillux::PinnedDirectory::open(&path)?.context("fixture runtime absent")?;
        let native =
            super::compact_native_projection_controls::native_observation(runtime.identity()?);
        let mut observation = native_resolution_projection(&native)?;
        let sources = observation.stopped_plan.as_ref().unwrap().roles.clone();
        let mut parent_intent = initial_intent(&runtime, &identity, sources)?;
        parent_intent.generation = observation.generation.clone();
        parent_intent.operation_id = observation.generation.clone();
        parent_intent.stopped_plan.as_mut().unwrap().generation = parent_intent.generation.clone();
        parent_intent.stopped_plan.as_mut().unwrap().generations =
            vec![parent_intent.generation.clone()];
        let plan = parent_intent.stopped_plan.as_ref().unwrap();
        observation.stopped_plan = Some(plan.clone());
        let names = plan.expected_coordinate_names();
        observation
            .stopped_coordinates
            .retain(|item| names.contains(&item.name));
        observation.resolutions.clear();
        observation.resolution_quarantines.clear();
        signed_prefix(&runtime, &identity, &parent_intent, 10)?;
        let parent_hash = lillux::sha256_hex(&read_signed_bytes(
            &runtime,
            &phase_name(&parent_intent.generation, 9),
        )?);
        let intent = SchedulerRetirementResolutionIntent {
            schema: 2,
            operation_id: parent_intent.operation_id.clone(),
            operation_sha256: parent_intent.operation_sha256.clone(),
            purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE.to_owned(),
            generation: parent_intent.generation.clone(),
            role: SchedulerRetirementRole::Main,
            ordinal: 0,
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: parent_intent.runtime_identity.clone(),
            parent_journal_sha256: parent_hash,
            previous_outcome_sha256: None,
            action: SchedulerRetirementResolutionAction::SyncQuarantine,
            expected: observation,
        };
        intent.validate_for_generation(&parent_intent)?;
        publish_signed(&runtime, &identity, &intent.intent_filename()?, &intent)?;
        let parent = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.remove(0);
        assert!(parent.resolution_slots[0].outcome_sha256.is_none());
        let mut child = parent_intent.clone();
        child.generation = "55".repeat(16);
        let mut remaining = parent_intent.scheduler_sources.clone();
        remaining[0].source = SchedulerRetirementStoppedSource::Quarantine {
            generation: parent_intent.generation.clone(),
        };
        let plan = SchedulerRetirementStoppedPlan {
            generation: child.generation.clone(),
            generations: vec![parent_intent.generation.clone(), child.generation.clone()],
            runtime_identity: child.runtime_identity.clone(),
            roles: remaining,
        };
        child.recovery = Some(recovery_binding(&parent, plan)?);
        child.intake_mode = "stopped-recovery".to_owned();
        child.stopped_plan = None;
        signed_prefix(&runtime, &identity, &child, 9)?;
        let chain = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?;
        assert_eq!(chain.len(), 2);
        let mut lost = chain.clone();
        lost[1]
            .generation_intent
            .recovery
            .as_mut()
            .unwrap()
            .parent_resolutions
            .clear();
        assert!(validate_authenticated_recovery_graph(&lost).is_err());
        let mut invented = chain.clone();
        invented[1]
            .generation_intent
            .recovery
            .as_mut()
            .unwrap()
            .parent_resolutions[0]
            .outcome_sha256 = Some("bb".repeat(32));
        assert!(validate_authenticated_recovery_graph(&invented).is_err());
        // The descendant consumes the admitted remaining plan, not an old FD.
        // This signed format control has no App mutation permit/native effects.
        signed_prefix(&runtime, &identity, &child, 28)?;
        assert!(RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.is_empty());
        assert!(runtime
            .entry_no_follow(OsStr::new(&intent.outcome_filename()?))?
            .is_none());
        let residue = format!(
            "scheduler-retirement.role.{}.main.purge",
            parent_intent.generation
        );
        std::fs::write(path.join(&residue), b"unexpected old alias")?;
        assert!(RetirementJournal::scan_interrupted_evidence(&runtime, &identity).is_err());
        Ok(())
    }

    #[test]
    fn every_late_signed_prefix_preserves_authorized_role_source_and_removal_boundary() -> Result<()>
    {
        for count in 9..=27 {
            let directory = tempfile::tempdir()?;
            let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
            let path = directory.path().join("runtime");
            std::fs::create_dir(&path)?;
            let runtime =
                lillux::PinnedDirectory::open(&path)?.context("fixture runtime absent")?;
            let roles = std::array::from_fn(|index| SchedulerRetirementStoppedRolePlan {
                source: SchedulerRetirementStoppedSource::Canonical,
                current: Some(SchedulerRetirementRoleIdentity {
                    device: 7,
                    inode: 100 + index as u64,
                    byte_length: 17,
                    content_sha256: Some("ab".repeat(32)),
                }),
            });
            let intent = initial_intent(&runtime, &identity, roles)?;
            signed_prefix(&runtime, &identity, &intent, count)?;
            let evidence = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?;
            assert_eq!(evidence.len(), 1);
            assert_eq!(evidence[0].phase_sha256.len(), count);
            for (index, role) in SchedulerRetirementRole::ALL.into_iter().enumerate() {
                let (original, sources, absence) = authorized_role_sources(&evidence[0], role)?;
                assert_eq!(original, intent.scheduler_sources[index].current);
                let start = 9 + 4 * index;
                assert_eq!(absence, count > start + 2);
                if count <= start {
                    assert_eq!(sources, vec![SchedulerRetirementStoppedSource::Canonical]);
                }
                if count > start + 3 {
                    assert!(sources.is_empty());
                }
            }
            assert!(RetirementJournal::validate_admission(&runtime, &identity).is_err());
        }
        Ok(())
    }

    #[test]
    fn signed_child_keeps_remaining_plan_and_exact_unresolved_parent_references() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let path = directory.path().join("runtime");
        std::fs::create_dir(&path)?;
        let runtime = lillux::PinnedDirectory::open(&path)?.context("fixture runtime absent")?;
        let roles = std::array::from_fn(|index| SchedulerRetirementStoppedRolePlan {
            source: SchedulerRetirementStoppedSource::Canonical,
            current: Some(SchedulerRetirementRoleIdentity {
                device: 7,
                inode: 100 + index as u64,
                byte_length: 17,
                content_sha256: Some("ab".repeat(32)),
            }),
        });
        let parent_intent = initial_intent(&runtime, &identity, roles)?;
        signed_prefix(&runtime, &identity, &parent_intent, 13)?;
        let parent = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.remove(0);
        let mut child = parent_intent.clone();
        child.generation = "22".repeat(16);
        let mut remaining = parent_intent.scheduler_sources.clone();
        remaining[0] = SchedulerRetirementStoppedRolePlan {
            source: SchedulerRetirementStoppedSource::Absent,
            current: None,
        };
        let plan = SchedulerRetirementStoppedPlan {
            generation: child.generation.clone(),
            generations: vec![parent_intent.generation.clone(), child.generation.clone()],
            runtime_identity: child.runtime_identity.clone(),
            roles: remaining,
        };
        child.recovery = Some(recovery_binding(&parent, plan)?);
        child.intake_mode = "stopped-recovery".to_owned();
        child.stopped_plan = None;
        signed_prefix(&runtime, &identity, &child, 9)?;
        let chain = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?;
        assert_eq!(chain.len(), 2);
        assert_eq!(
            chain[1].generation_intent.scheduler_accounting,
            parent_intent.scheduler_accounting
        );
        assert!(chain[1].phase_records[6].role_identity.is_none());
        let mut truncated = chain.clone();
        truncated[1]
            .generation_intent
            .recovery
            .as_mut()
            .unwrap()
            .parent_phase_sha256
            .pop();
        assert!(validate_authenticated_recovery_graph(&truncated).is_err());
        let mut changed = chain.clone();
        changed[1].generation_intent.operation_sha256 = "bb".repeat(32);
        assert!(validate_authenticated_recovery_graph(&changed).is_err());
        let mut invented = chain.clone();
        invented[1]
            .generation_intent
            .recovery
            .as_mut()
            .unwrap()
            .stopped_plan
            .roles[1] = SchedulerRetirementStoppedRolePlan {
            source: SchedulerRetirementStoppedSource::Absent,
            current: None,
        };
        assert!(validate_authenticated_recovery_graph(&invented).is_err());
        let mut fork = chain.clone();
        let mut sibling = chain[1].clone();
        sibling.generation_intent.generation = "33".repeat(16);
        fork.push(sibling);
        assert!(validate_authenticated_recovery_graph(&fork).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod fire_history_retirement_controls {
    use super::*;

    const OPERATION: &str = "44444444444444444444444444444444";

    fn operation_sha256() -> String {
        "ab".repeat(32)
    }

    /// A runtime whose fires root holds `alpha` (journal and lock) and
    /// `beta` (journal only).
    fn fired_runtime(
    ) -> Result<(tempfile::TempDir, NodeIdentity, std::path::PathBuf, lillux::PinnedDirectory)> {
        let directory = tempfile::tempdir()?;
        let identity = NodeIdentity::create(&directory.path().join("identity.key"))?;
        let runtime_path = directory.path().join("runtime");
        for (schedule, files) in [
            ("alpha", &[FIRE_JOURNAL, FIRE_JOURNAL_LOCK][..]),
            ("beta", &[FIRE_JOURNAL][..]),
        ] {
            let schedule_dir = runtime_path.join(FIRES_ROOT).join(schedule);
            std::fs::create_dir_all(&schedule_dir)?;
            for file in files {
                std::fs::write(schedule_dir.join(file), b"{\"fire\":\"record\"}\n")?;
            }
        }
        let runtime =
            lillux::PinnedDirectory::open(&runtime_path)?.context("test runtime absent")?;
        Ok((directory, identity, runtime_path, runtime))
    }

    fn fires_tree(runtime_path: &std::path::Path) -> Result<Vec<String>> {
        let mut found = Vec::new();
        let root = runtime_path.join(FIRES_ROOT);
        for schedule in std::fs::read_dir(&root)? {
            let schedule = schedule?.path();
            found.push(schedule.strip_prefix(&root)?.display().to_string());
            if schedule.is_dir() {
                for file in std::fs::read_dir(&schedule)? {
                    found.push(file?.path().strip_prefix(&root)?.display().to_string());
                }
            }
        }
        found.sort();
        Ok(found)
    }

    fn intent_names(runtime: &lillux::PinnedDirectory) -> Result<Vec<String>> {
        Ok(runtime
            .entry_names_bounded(MAX_DIRECTORY_ENTRIES)?
            .into_iter()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| name.starts_with(FIRES_PREFIX))
            .collect())
    }

    fn begun_intent(
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
        operation_id: &str,
    ) -> Result<()> {
        let intent = FireHistoryRetirementIntent {
            schema: 1,
            purpose: FIRES_PURPOSE.to_owned(),
            operation_id: operation_id.to_owned(),
            operation_sha256: operation_sha256(),
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: serde_json::to_string(&runtime.identity()?)?,
            schedules: 2,
            files: 3,
        };
        publish_signed(runtime, identity, &format!("{FIRES_PREFIX}{operation_id}"), &intent)?;
        Ok(())
    }

    /// Every schedule's fire files and directory go; the empty root stays
    /// for the post-acknowledgement removal, and no intent survives.
    #[test]
    fn fire_history_is_retired_leaving_only_the_empty_root() -> Result<()> {
        let (_directory, identity, runtime_path, runtime) = fired_runtime()?;
        let removed = retire_fire_history(&runtime, &identity, OPERATION, &operation_sha256())?;
        assert_eq!(removed, 3 + 2, "three fire files and two schedule directories");
        assert!(fires_tree(&runtime_path)?.is_empty());
        assert!(runtime_path.join(FIRES_ROOT).is_dir());
        assert!(intent_names(&runtime)?.is_empty());
        // Nothing left: a repeated completion is a no-op.
        assert_eq!(retire_fire_history(&runtime, &identity, OPERATION, &operation_sha256())?, 0);
        Ok(())
    }

    /// A process that died after the intent was durable and partway through
    /// removal leaves a namespace every scanner admits; the next completion
    /// (of the same operation, or of a later one) finishes it.
    #[test]
    fn begun_fire_retirement_is_admitted_and_finished_on_resumption() -> Result<()> {
        for resumed_by in [OPERATION, "55555555555555555555555555555555"] {
            let (_directory, identity, runtime_path, runtime) = fired_runtime()?;
            begun_intent(&runtime, &identity, OPERATION)?;
            std::fs::remove_file(runtime_path.join(FIRES_ROOT).join("alpha").join(FIRE_JOURNAL))?;
            std::fs::remove_file(runtime_path.join(FIRES_ROOT).join("beta").join(FIRE_JOURNAL))?;
            std::fs::remove_dir(runtime_path.join(FIRES_ROOT).join("beta"))?;
            assert!(RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.is_empty());
            RetirementJournal::validate_admission(&runtime, &identity)?;
            RetirementJournal::reserve_pre_effect_capacity(&runtime, Some(&identity), true)?;

            let removed =
                retire_fire_history(&runtime, &identity, resumed_by, &operation_sha256())?;
            assert_eq!(removed, 1 + 1, "the lock file and the alpha directory remained");
            assert!(fires_tree(&runtime_path)?.is_empty());
            assert!(intent_names(&runtime)?.is_empty());
        }
        Ok(())
    }

    /// A foreign entry inside a schedule directory refuses before any
    /// effect: it is named, and every fire file stays where it was.
    #[test]
    fn foreign_file_in_a_schedule_directory_is_refused_and_kept() -> Result<()> {
        let (_directory, identity, runtime_path, runtime) = fired_runtime()?;
        let foreign = runtime_path.join(FIRES_ROOT).join("alpha").join("notes.txt");
        std::fs::write(&foreign, b"not fire history")?;
        let before = fires_tree(&runtime_path)?;
        let error = retire_fire_history(&runtime, &identity, OPERATION, &operation_sha256())
            .unwrap_err();
        // Not retried for the settlement window: the reset surfaces it at once.
        assert!(error.chain().any(|cause| {
            cause
                .downcast_ref::<ryeos_app::execution_history_reset::SchedulerPermanentRefusal>()
                .is_some()
        }));
        let refusal = error.to_string();
        assert!(refusal.contains("foreign entry"), "{refusal}");
        assert!(refusal.contains("notes.txt"), "{refusal}");
        assert_eq!(fires_tree(&runtime_path)?, before);
        assert_eq!(std::fs::read(&foreign)?, b"not fire history");
        assert!(intent_names(&runtime)?.is_empty(), "no intent precedes a refusal");

        // A begun retirement refuses the same way and keeps its intent.
        begun_intent(&runtime, &identity, OPERATION)?;
        assert!(retire_fire_history(&runtime, &identity, OPERATION, &operation_sha256()).is_err());
        assert_eq!(fires_tree(&runtime_path)?, before);
        assert_eq!(intent_names(&runtime)?.len(), 1);
        Ok(())
    }

    /// A non-directory schedule entry and a symlinked fire journal are
    /// foreign too, and are kept.
    #[test]
    fn non_directory_schedule_and_symlinked_journal_are_refused_and_kept() -> Result<()> {
        let (_directory, identity, runtime_path, runtime) = fired_runtime()?;
        let stray = runtime_path.join(FIRES_ROOT).join("gamma");
        std::fs::write(&stray, b"not a schedule directory")?;
        assert!(retire_fire_history(&runtime, &identity, OPERATION, &operation_sha256()).is_err());
        assert!(stray.is_file());
        std::fs::remove_file(&stray)?;

        let beta = runtime_path.join(FIRES_ROOT).join("beta");
        std::fs::remove_file(beta.join(FIRE_JOURNAL))?;
        lillux::PinnedDirectory::open(&beta)?
            .context("beta absent")?
            .create_symlink(OsStr::new(FIRE_JOURNAL), b"/nonexistent/fires.jsonl")?;
        let before = fires_tree(&runtime_path)?;
        assert!(retire_fire_history(&runtime, &identity, OPERATION, &operation_sha256()).is_err());
        assert_eq!(fires_tree(&runtime_path)?, before);
        assert!(intent_names(&runtime)?.is_empty());
        Ok(())
    }

    /// An intent of this operation must bind its operation digest.
    #[test]
    fn begun_intent_with_another_operation_digest_refuses() -> Result<()> {
        let (_directory, identity, runtime_path, runtime) = fired_runtime()?;
        begun_intent(&runtime, &identity, OPERATION)?;
        let before = fires_tree(&runtime_path)?;
        assert!(retire_fire_history(&runtime, &identity, OPERATION, &"cd".repeat(32)).is_err());
        assert_eq!(fires_tree(&runtime_path)?, before);
        Ok(())
    }
}
