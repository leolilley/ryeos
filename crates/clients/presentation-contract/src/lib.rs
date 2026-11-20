//! UI-owned data-only presentation testimony, independent of renderer, state and I/O.
//! Validation and retention links are NOT transfer admission or edit authority.
pub mod append_bound;
pub mod freeze_witness;
pub mod identity;
pub mod initialization;
pub mod move_freeze_lifecycle;
pub mod payload_digest;
pub mod seat_control;
pub mod transfer;
pub mod transfer_lifecycle;
pub mod transfer_proof;
pub mod transfer_state;

#[cfg(test)]
mod seat_control_tests;
#[cfg(test)]
mod transfer_proof_tests;
#[cfg(test)]
mod transfer_tests;
