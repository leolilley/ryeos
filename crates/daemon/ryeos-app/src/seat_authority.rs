//! Who may read a thread from a seat, and what project a seat is bound to.
//!
//! A thread read is judged by key and principal only, never by the kind of
//! caller. The thread's sealed project identity names its owner; a reader key
//! that speaks for that owner now reads it, unless the seat is a view of
//! another project. A thread with no project identity is read by the key that
//! launched it. Every path also requires the reader key to be active, so a
//! retired or revoked key reads nothing. A checkout path is never authority:
//! two principals sharing a folder see only their own threads.
//!
//! A seat's project state is reported to the client as data. It is never a
//! reason a thread read gives: missing and refused threads are both
//! `NotFound`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ryeos_engine::trust::TrustStore;
use ryeos_state::objects::ExecutionProjectAuthority;
use ryeos_state::project_identity::ProjectIdentity;
use serde::{Deserialize, Serialize};

use crate::handler_error::HandlerError;
use crate::project_registry::LiveSignerOutcome;
use crate::state::AppState;
use crate::state_store::AuthoritativeThreadSubject;

/// What a seat is bound to, as the compiled binding recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatProjectSubject {
    /// A checkout the binding resolved to a project identity.
    Project {
        identity: ProjectIdentity,
        checkout: PathBuf,
    },
    /// A host folder registered in a seat registry. It has no identity.
    Folder { local_id: String },
    /// No project at all.
    Projectless,
}

/// Why a seat bound to a checkout states no project identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatProjectAbsence {
    /// No registration of the checkout under the seat key's owner, or the
    /// seat key does not hold live project write.
    Unregistered,
    /// The seat key is retired or revoked.
    OwnerKeyNotActive,
    /// The checkout's manifest declares no name.
    NoProjectName,
    /// The checkout is registered under another name than its manifest now
    /// declares.
    CheckoutNameChanged,
}

impl SeatProjectAbsence {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unregistered => "unregistered",
            Self::OwnerKeyNotActive => "owner_key_not_active",
            Self::NoProjectName => "no_project_name",
            Self::CheckoutNameChanged => "checkout_name_changed",
        }
    }
}

/// A seat's project. Serialized as
/// `{"state": "stated"|"absent"|"projectless", "project"?: "<identity>", "absence"?: "<code>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SeatProjectStateWire", into = "SeatProjectStateWire")]
pub enum SeatProjectState {
    Stated(ProjectIdentity),
    Absent(SeatProjectAbsence),
    Projectless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SeatProjectStateTag {
    Stated,
    Absent,
    Projectless,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeatProjectStateWire {
    state: SeatProjectStateTag,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<ProjectIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    absence: Option<SeatProjectAbsence>,
}

impl From<SeatProjectState> for SeatProjectStateWire {
    fn from(state: SeatProjectState) -> Self {
        match state {
            SeatProjectState::Stated(identity) => Self {
                state: SeatProjectStateTag::Stated,
                project: Some(identity),
                absence: None,
            },
            SeatProjectState::Absent(absence) => Self {
                state: SeatProjectStateTag::Absent,
                project: None,
                absence: Some(absence),
            },
            SeatProjectState::Projectless => Self {
                state: SeatProjectStateTag::Projectless,
                project: None,
                absence: None,
            },
        }
    }
}

impl TryFrom<SeatProjectStateWire> for SeatProjectState {
    type Error = String;

    fn try_from(wire: SeatProjectStateWire) -> std::result::Result<Self, Self::Error> {
        match (wire.state, wire.project, wire.absence) {
            (SeatProjectStateTag::Stated, Some(identity), None) => Ok(Self::Stated(identity)),
            (SeatProjectStateTag::Absent, None, Some(absence)) => Ok(Self::Absent(absence)),
            (SeatProjectStateTag::Projectless, None, None) => Ok(Self::Projectless),
            _ => Err(
                "seat project state: `stated` carries only `project`, `absent` only `absence`, \
                 `projectless` neither"
                    .to_string(),
            ),
        }
    }
}

impl SeatProjectState {
    /// The project a seat states from the identity its binding sealed at
    /// mint. A seat bound to a checkout without an identity is `Absent`.
    pub fn from_sealed(
        bound_to_checkout: bool,
        signer_project: &ryeos_state::project_identity::LogicalProject,
    ) -> Self {
        use ryeos_state::project_identity::{LogicalProject, LogicalProjectAbsence};
        match (bound_to_checkout, signer_project) {
            (false, _) => Self::Projectless,
            (true, LogicalProject::Stated(identity)) => Self::Stated(identity.clone()),
            (true, LogicalProject::Absent(LogicalProjectAbsence::NoProjectName)) => {
                Self::Absent(SeatProjectAbsence::NoProjectName)
            }
            (true, LogicalProject::Absent(_)) => Self::Absent(SeatProjectAbsence::Unregistered),
        }
    }
}

/// The seat's project as this node judges it now: the identity the binding
/// recorded is `Stated` only while the seat key still resolves the checkout
/// to it. Called at mint, launch and dispatch revalidation, for the client.
/// A registry or checkout that cannot be read refuses.
pub fn resolve_seat_project(
    state: &AppState,
    key: &str,
    scopes: &[String],
    subject: &SeatProjectSubject,
) -> Result<SeatProjectState> {
    match subject {
        SeatProjectSubject::Projectless => Ok(SeatProjectState::Projectless),
        SeatProjectSubject::Folder { .. } => {
            Ok(SeatProjectState::Absent(SeatProjectAbsence::Unregistered))
        }
        SeatProjectSubject::Project { identity, checkout } => {
            let outcome = live_outcome(state, key, scopes, checkout)?;
            Ok(seat_state_from_outcome(identity, outcome))
        }
    }
}

fn live_outcome(
    state: &AppState,
    key: &str,
    scopes: &[String],
    checkout: &Path,
) -> Result<LiveSignerOutcome> {
    let holds_live_write = crate::project_registry::holds_live_project_write(state, scopes);
    crate::project_registry::live_signer_project_outcome(
        &state.config.app_root,
        state.identity.verifying_key(),
        &state.engine.trust_store,
        key,
        holds_live_write,
        checkout,
    )
}

fn seat_state_from_outcome(
    recorded: &ProjectIdentity,
    outcome: LiveSignerOutcome,
) -> SeatProjectState {
    match outcome {
        LiveSignerOutcome::Stated(current) if &current == recorded => {
            SeatProjectState::Stated(current)
        }
        LiveSignerOutcome::Stated(_)
        | LiveSignerOutcome::NoLiveWrite
        | LiveSignerOutcome::NotRegistered => {
            SeatProjectState::Absent(SeatProjectAbsence::Unregistered)
        }
        LiveSignerOutcome::OwnerKeyNotActive => {
            SeatProjectState::Absent(SeatProjectAbsence::OwnerKeyNotActive)
        }
        LiveSignerOutcome::NoProjectName => {
            SeatProjectState::Absent(SeatProjectAbsence::NoProjectName)
        }
        LiveSignerOutcome::CheckoutNameChanged { .. } => {
            SeatProjectState::Absent(SeatProjectAbsence::CheckoutNameChanged)
        }
    }
}

/// One reader of threads: a key, the project its seat is bound to, and
/// whether the read is made by a source a project-authored item declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadReader {
    /// The reader key, `fp:<hex>`.
    pub key: String,
    pub seat_project: SeatProjectState,
    /// A source a project-authored view or Surface declared reads threads of
    /// its seat's own project and nothing else.
    pub project_authored_source: bool,
}

impl ThreadReader {
    /// Whether this reader may read `subject`, judged against `trust` (the
    /// node registry store) now.
    pub fn admits(&self, trust: &TrustStore, subject: &AuthoritativeThreadSubject) -> bool {
        // A retired or revoked key reads nothing, whichever path would admit
        // it. A key the registry has never recorded acts for itself.
        if trust.owner_for(&self.key).is_err() {
            return false;
        }
        let (sealed, live) = match &subject.project_authority {
            ExecutionProjectAuthority::LiveProject { signer_project, .. } => {
                (signer_project.identity(), true)
            }
            ExecutionProjectAuthority::PinnedGeneration { signer_project, .. } => {
                (signer_project.identity(), false)
            }
            ExecutionProjectAuthority::Projectless { .. } => (None, false),
        };
        // A stated project's threads are read by the keys of its owner: an
        // ownership consequence, never a grant of any act. These three arms
        // are this file's entry in the principal-owner guard's
        // SPEAKS_FOR_CALLERS, which pins their count; unstated threads never
        // ask it.
        if self.project_authored_source {
            return live
                && sealed.is_some_and(|project| {
                    self.seat_project == SeatProjectState::Stated(project.clone())
                        && trust.speaks_for(&self.key, project.owner())
                });
        }
        match sealed {
            Some(project) => match &self.seat_project {
                // A project view reads only its own project.
                SeatProjectState::Stated(seat) => {
                    seat == project && trust.speaks_for(&self.key, project.owner())
                }
                SeatProjectState::Projectless => trust.speaks_for(&self.key, project.owner()),
                // Bound to a checkout that states no identity now (renamed, or
                // its registration lapsed): the key still reads what it
                // launched itself, and nothing else of the project.
                SeatProjectState::Absent(_) => {
                    subject.requested_by.as_deref() == Some(self.key.as_str())
                }
            },
            None => subject.requested_by.as_deref() == Some(self.key.as_str()),
        }
    }
}

/// The signed subjects of `thread_ids` when `reader` may read every one of
/// them. Missing and refused subjects are both `NotFound`.
pub fn authorize_thread_subjects(
    state: &AppState,
    reader: &ThreadReader,
    thread_ids: &[&str],
) -> Result<Vec<AuthoritativeThreadSubject>, HandlerError> {
    let subjects = load_thread_subjects(state, thread_ids)?;
    if subjects.iter().any(Option::is_none) {
        return Err(HandlerError::NotFound);
    }
    let subjects = subjects.into_iter().flatten().collect::<Vec<_>>();
    let trust = &state.engine.trust_store;
    if subjects.iter().all(|subject| reader.admits(trust, subject)) {
        Ok(subjects)
    } else {
        Err(HandlerError::NotFound)
    }
}

/// The signed subjects of `thread_ids`, `None` for each that does not exist.
/// Unjudged: a caller judging one subject for many readers uses this and
/// [`ThreadReader::admits`].
pub fn load_thread_subjects(
    state: &AppState,
    thread_ids: &[&str],
) -> Result<Vec<Option<AuthoritativeThreadSubject>>, HandlerError> {
    state
        .state_store
        .authoritative_thread_subjects(thread_ids)
        .map_err(|error| {
            tracing::error!(error = %format!("{error:#}"), "exact thread authority read failed");
            HandlerError::Internal("exact thread authority read failed".to_string())
        })
}

#[cfg(test)]
#[path = "seat_authority_tests.rs"]
mod tests;
