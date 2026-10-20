//! Locating the transfers one seat's signed history names.
//!
//! Discovery is a locator: an identity found here says a transfer touched the
//! seat, never that it is complete or ready to settle, and the absence of an
//! identity from one page says nothing about the seat's older history. Only a
//! scan that reaches genesis is exhaustive for the seat's own chain, and even
//! that cannot name a transfer a counterpart began without writing to this
//! seat.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail};
use ryeos_app::state_store::StateStore;
use ryeos_state::objects::thread_event::ThreadEvent;
use ryeos_ui_presentation_contract::transfer::TransferIdentity;

use super::reader::decode_transfer_phase;

/// Events one discovery page reads. Mechanical read granularity only: no page
/// boundary is a limit on a seat's history.
const DISCOVERY_PAGE_EVENTS: u64 = 64;
/// Bytes one discovery page reads, under the same reading.
const DISCOVERY_PAGE_BYTES: u64 = 2 * 1024 * 1024;

/// One bounded, newest-first page of transfer identities found in an exact
/// signed seat history. Identities may recur on later pages.
pub struct SignedSeatTransferIdentityPage {
    /// Positive ordinary freeze-preparation locators, not authenticated
    /// lifecycle clearance. Retirement must discharge these independently.
    pub freeze_preparations: Vec<TransferIdentity>,
    pub identities: Vec<TransferIdentity>,
    pub progress: Option<ryeos_state::AuthoritativeThreadEventReadProgress>,
}

/// Enumerate only the supplied fixed signed head. The caller must retain the
/// anchor and resume with the returned opaque progress until it is `None`;
/// absence in one page is never absence from the seat's history. Decoding is
/// deliberately exact-owner and fail-closed, even for lifecycle testimony.
pub fn read_signed_seat_transfer_identity_page(
    store: &StateStore,
    seat_root_id: &str,
    anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    progress: Option<&ryeos_state::AuthoritativeThreadEventReadProgress>,
    max_events: u64,
    max_bytes: u64,
) -> anyhow::Result<SignedSeatTransferIdentityPage> {
    if anchor.chain_root_id() != seat_root_id || anchor.thread_id() != seat_root_id {
        bail!("transfer enumeration requires an exact seat-root anchor");
    }
    let page = store.read_thread_event_page(anchor, progress, max_events, max_bytes)?;
    if page.events.is_empty() && page.progress.is_some() {
        bail!("signed transfer enumeration page made no progress");
    }
    let mut identities = Vec::new();
    let mut freeze_preparations = Vec::new();
    for (_, event) in page.events {
        if event.chain_root_id != seat_root_id || event.thread_id != seat_root_id {
            bail!("signed transfer enumeration changed exact seat");
        }
        if let Some(identity) = signed_transfer_identity_for_seat(seat_root_id, &event)? {
            if event.event_type == "seat.presentation_operation.prepared" {
                freeze_preparations.push(identity.clone());
            }
            identities.push(identity);
        }
    }
    Ok(SignedSeatTransferIdentityPage {
        freeze_preparations,
        identities,
        progress: page.progress,
    })
}

pub(super) fn signed_transfer_identity_for_seat(
    seat_root_id: &str,
    event: &ThreadEvent,
) -> anyhow::Result<Option<TransferIdentity>> {
    if event.chain_root_id != seat_root_id || event.thread_id != seat_root_id {
        bail!("signed transfer enumeration changed exact seat");
    }
    if event
        .event_type
        .starts_with("contract.ryeos_ui.move_freeze_cancelled")
        && event.event_type
            != ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT
    {
        bail!("signed freeze cancellation enumeration has unsupported testimony");
    }
    // Locator testimony only. Ordinary preparation is authenticated by its
    // receipt and initialization only when the shared fixed scans complete.
    // This positive hint never establishes absence or permits publication.
    let freeze_identity = if event.event_type == "seat.presentation_operation.prepared" {
        let semantic = event
            .payload
            .get("payload")
            .ok_or_else(|| anyhow!("signed preparation semantic payload absent"))?;
        ryeos_ui_presentation_contract::move_freeze_lifecycle::preparation_commitment(semantic)
            .map_err(anyhow::Error::msg)?
            .map(|commitment| commitment.transfer)
    } else if event.event_type
        == ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT
    {
        Some(
            ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(
                &event.to_value(),
            )
            .map_err(anyhow::Error::msg)?
            .transfer,
        )
    } else {
        None
    };
    if let Some(identity) = freeze_identity {
        if identity.source.chain_root_id != seat_root_id
            || identity.source.thread_id != seat_root_id
        {
            bail!("signed freeze locator has another exact source owner");
        }
        return Ok(Some(identity));
    }
    let Some((record, _)) = decode_transfer_phase(event)? else {
        return Ok(None);
    };
    if record.event_owner().chain_root_id != seat_root_id
        || record.event_owner().thread_id != seat_root_id
    {
        bail!("signed transfer phase has another exact seat owner");
    }
    Ok(Some(record.transfer))
}

/// Every transfer identity the seat's own signed history names, read at the
/// supplied exact anchor until genesis. A freeze preparation counts as a
/// locator. The result is a locator set, not a settlement proof.
pub fn discover_seat_transfer_identities(
    store: &StateStore,
    seat_root_id: &str,
    anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
) -> anyhow::Result<BTreeMap<String, TransferIdentity>> {
    let mut found: BTreeMap<String, TransferIdentity> = BTreeMap::new();
    let mut progress = None;
    loop {
        let page = read_signed_seat_transfer_identity_page(
            store,
            seat_root_id,
            anchor,
            progress.as_ref(),
            DISCOVERY_PAGE_EVENTS,
            DISCOVERY_PAGE_BYTES,
        )?;
        for identity in page.identities {
            match found.get(&identity.operation_id) {
                Some(prior) if prior != &identity => {
                    bail!("seat history names one transfer operation with two identities")
                }
                Some(_) => {}
                None => {
                    found.insert(identity.operation_id.clone(), identity);
                }
            }
        }
        progress = page.progress;
        if progress.is_none() {
            return Ok(found);
        }
    }
}
