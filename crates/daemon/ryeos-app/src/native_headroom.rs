//! The node's live native service headroom, and whether a fresh external
//! placement fits inside it.
//!
//! A daemon incarnation runs on a prepaid envelope of native work (SQL VM
//! steps) and inside a fixed service window. Spending the envelope, or
//! reaching the window's edge, stops the daemon in an orderly way, and a
//! placement that is still running at that moment loses its relay and its
//! turn. A placement's binding therefore declares the native work it may
//! spend and the lifetime it may hold, and a fresh placement is admitted
//! only when what remains of this incarnation covers both. The node reports
//! what remains; the workload declares what it needs; neither side reasons
//! about the other. The headroom ends where the planned rollover begins (the
//! signed drain and work margins), not at the service edge, because the
//! rollover's graceful stop cuts whatever is still running. A refusal is a
//! typed `native_headroom_insufficient` naming the dimension that does not
//! fit. A node composed without a native service (a test state,
//! a standalone tool) has no envelope to fit, so it admits every placement.
//! This is a floor at admission, not a reservation: concurrent placements
//! share the remaining envelope.

use anyhow::{Context as _, Result};

use crate::node_config::sections::external_execution::ExternalPlacementBackendContract;
use crate::state::AppState;

/// What remains of this incarnation's native service, read at the moment of
/// an admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeServiceHeadroom {
    /// Prepaid native work still available before the planned work
    /// rollover, in VM steps; the cleanup reserve and the rollover work
    /// margin are already excluded.
    pub work_steps: u64,
    /// Seconds until this incarnation's planned rollover begins its drain.
    pub service_seconds: u64,
}

/// Reads the live headroom. The daemon composition root supplies the reader
/// from its native factory; its absence means the node has no native
/// envelope to admit against.
pub struct NativeServiceHeadroomReader {
    read: Box<dyn Fn() -> Result<NativeServiceHeadroom> + Send + Sync>,
}

impl NativeServiceHeadroomReader {
    pub fn new(read: impl Fn() -> Result<NativeServiceHeadroom> + Send + Sync + 'static) -> Self {
        Self {
            read: Box::new(read),
        }
    }

    pub fn current(&self) -> Result<NativeServiceHeadroom> {
        (self.read)()
    }
}

/// What a placement declares it may consume of the node, from its signed
/// binding: the native work across its whole life, and the lifetime it may
/// hold including its cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeWorkNeed {
    pub work_steps: u64,
    pub lifetime_seconds: u64,
}

impl NativeWorkNeed {
    pub(crate) fn of_contract(contract: &ExternalPlacementBackendContract) -> Self {
        Self {
            work_steps: contract.native_work_steps,
            lifetime_seconds: u64::from(contract.timeout_seconds)
                .saturating_add(u64::from(contract.cleanup_timeout_seconds)),
        }
    }
}

/// Admit a fresh placement against the node's live headroom. `what` names
/// the refused admission in the error.
pub fn admit_fresh_placement_headroom(
    state: &AppState,
    what: &str,
    need: NativeWorkNeed,
) -> Result<()> {
    let Some(reader) = state.extensions.get::<NativeServiceHeadroomReader>() else {
        return Ok(());
    };
    let headroom = reader
        .current()
        .with_context(|| format!("{what} refused: read this node's native service headroom"))?;
    headroom_fits(what, &headroom, &need)
}

/// Which part of the node's headroom a placement does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeHeadroomDimension {
    WorkSteps,
    ServiceSeconds,
}

impl NativeHeadroomDimension {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkSteps => "work_steps",
            Self::ServiceSeconds => "service_seconds",
        }
    }
}

/// A fresh placement refused because what it declares exceeds what remains
/// of this incarnation before its planned rollover. A driver waits for the
/// rollover (or stops and starts the node for a fresh window) and retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeHeadroomInsufficient {
    /// The refused admission, e.g. "external qualification start".
    pub what: String,
    pub dimension: NativeHeadroomDimension,
    pub declared: u64,
    pub remaining: u64,
}

impl NativeHeadroomInsufficient {
    pub const CODE: &'static str = "native_headroom_insufficient";

    pub fn code(&self) -> &'static str {
        Self::CODE
    }

    /// The structured fields a refusal carries on every error surface.
    pub fn details(&self) -> serde_json::Value {
        serde_json::json!({
            "dimension": self.dimension.as_str(),
            "declared": self.declared,
            "remaining": self.remaining,
        })
    }

    /// The refusal in `error`'s cause chain, if any.
    pub fn find(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

impl std::fmt::Display for NativeHeadroomInsufficient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = &self.what;
        let (declared, remaining) = (self.declared, self.remaining);
        match self.dimension {
            NativeHeadroomDimension::WorkSteps => write!(
                formatter,
                "{what} refused: this placement declares {declared} steps of native work and \
                 this node incarnation has {remaining} left before its planned rollover; wait \
                 for the rollover, or stop and start the node for a fresh window"
            ),
            NativeHeadroomDimension::ServiceSeconds => write!(
                formatter,
                "{what} refused: this placement may hold {declared}s and this node \
                 incarnation's planned rollover begins in {remaining}s; wait for the rollover, \
                 or stop and start the node for a fresh window"
            ),
        }
    }
}

impl std::error::Error for NativeHeadroomInsufficient {}

pub(crate) fn headroom_fits(
    what: &str,
    headroom: &NativeServiceHeadroom,
    need: &NativeWorkNeed,
) -> Result<()> {
    let refuse = |dimension, declared, remaining| {
        anyhow::Error::new(NativeHeadroomInsufficient {
            what: what.to_owned(),
            dimension,
            declared,
            remaining,
        })
    };
    if need.work_steps > headroom.work_steps {
        return Err(refuse(
            NativeHeadroomDimension::WorkSteps,
            need.work_steps,
            headroom.work_steps,
        ));
    }
    if need.lifetime_seconds > headroom.service_seconds {
        return Err(refuse(
            NativeHeadroomDimension::ServiceSeconds,
            need.lifetime_seconds,
            headroom.service_seconds,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placement_fits_only_when_both_its_work_and_its_lifetime_remain() {
        let headroom = NativeServiceHeadroom {
            work_steps: 10_000_000,
            service_seconds: 3_600,
        };
        let fits = |work_steps, lifetime_seconds| {
            headroom_fits(
                "placement",
                &headroom,
                &NativeWorkNeed {
                    work_steps,
                    lifetime_seconds,
                },
            )
        };
        fits(8_000_000, 900).unwrap();
        fits(10_000_000, 3_600).unwrap();
        let error = fits(10_000_001, 900).unwrap_err();
        let refusal = NativeHeadroomInsufficient::find(&error).expect("typed refusal");
        assert_eq!(refusal.code(), "native_headroom_insufficient");
        assert_eq!(
            (refusal.dimension, refusal.declared, refusal.remaining),
            (NativeHeadroomDimension::WorkSteps, 10_000_001, 10_000_000)
        );
        let message = error.to_string();
        assert!(
            message.starts_with("placement refused: this placement declares 10000001 steps")
                && message.contains("has 10000000 left before its planned rollover"),
            "{message}"
        );
        let error = fits(1, 3_601).unwrap_err().context("start the placement");
        let refusal = NativeHeadroomInsufficient::find(&error).expect("typed refusal");
        assert_eq!(
            (refusal.dimension, refusal.declared, refusal.remaining),
            (NativeHeadroomDimension::ServiceSeconds, 3_601, 3_600)
        );
        assert_eq!(
            refusal.details(),
            serde_json::json!({"dimension": "service_seconds", "declared": 3601, "remaining": 3600})
        );
        assert!(
            refusal.to_string().contains("may hold 3601s")
                && refusal.to_string().contains("begins in 3600s"),
            "{refusal}"
        );
    }

    #[test]
    fn a_reader_failure_is_a_refusal_that_names_the_admission() {
        let reader =
            NativeServiceHeadroomReader::new(|| anyhow::bail!("live native service refused"));
        let error = reader.current().unwrap_err().to_string();
        assert_eq!(error, "live native service refused");
    }
}
