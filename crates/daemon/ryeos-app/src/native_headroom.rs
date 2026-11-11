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
//! about the other. A node composed without a native service (a test state,
//! a standalone tool) has no envelope to fit, so it admits every placement.
//! This is a floor at admission, not a reservation: concurrent placements
//! share the remaining envelope.

use anyhow::{Context as _, Result, ensure};

use crate::node_config::sections::external_execution::ExternalPlacementBackendContract;
use crate::state::AppState;

/// What remains of this incarnation's native service, read at the moment of
/// an admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeServiceHeadroom {
    /// Prepaid native work still available to service SQL, in VM steps; the
    /// cleanup reserve is already excluded.
    pub work_steps: u64,
    /// Seconds until this incarnation's service SQL must end.
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

pub(crate) fn headroom_fits(
    what: &str,
    headroom: &NativeServiceHeadroom,
    need: &NativeWorkNeed,
) -> Result<()> {
    ensure!(
        need.work_steps <= headroom.work_steps,
        "{what} refused: this placement declares {} steps of native work and this node \
         incarnation has {} left; it would end the incarnation before the placement settles",
        need.work_steps,
        headroom.work_steps
    );
    ensure!(
        need.lifetime_seconds <= headroom.service_seconds,
        "{what} refused: this placement may hold {}s and this node incarnation's service ends \
         in {}s; the placement would outlive the incarnation",
        need.lifetime_seconds,
        headroom.service_seconds
    );
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
        let refusal = fits(10_000_001, 900).unwrap_err().to_string();
        assert!(
            refusal.starts_with("placement refused: this placement declares 10000001 steps")
                && refusal.contains("has 10000000 left"),
            "{refusal}"
        );
        let refusal = fits(1, 3_601).unwrap_err().to_string();
        assert!(
            refusal.contains("may hold 3601s") && refusal.contains("ends in 3600s"),
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
