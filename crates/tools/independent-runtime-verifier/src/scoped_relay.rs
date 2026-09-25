//! Verifier-owned half of the daemon's held direct-Codex relay handoff.
//!
//! This does not authorize the target or qualify its result. The signed recipe
//! and daemon own the executable and held process; this module admits only the
//! exact transferred listener for the credential-free scripted provider.

use anyhow::{Context as _, Result, ensure};
use lillux::loopback::ExactLoopbackListener;
use lillux::time::MonotonicDeadline;
use lillux::{InheritedDuplexChannel, PinnedDirectory};
use ryeos_runtime::scoped_relay_handoff::{
    ScopedRelayHandoff, receive_handoff, send_ready,
};
use ryeos_state::external_content::products::ProducerLoopbackIngress;
use ryeos_state::external_content::products::qualification::ProductProducerRecipeSourceIdentity;
use std::ffi::OsString;
use std::net::SocketAddr;

use crate::scripted_relay::{self, RunningScriptedRelay};

/// Keep both the peer channel and relay alive until the daemon-owned target
/// has settled. Dropping this before settlement is a failed attempt, not a
/// clean observation or permission to replay provider traffic.
pub struct RunningScopedRelay {
    handoff: ScopedRelayHandoff,
    channel: InheritedDuplexChannel,
    relay: RunningScriptedRelay,
}

impl RunningScopedRelay {
    pub fn handoff(&self) -> &ScopedRelayHandoff {
        &self.handoff
    }

    /// Only the daemon's settled target and joined observation permit a
    /// provider transcript to be accepted. Retain the exact peer channel
    /// while the relay is settling; timeout returns both live owners rather
    /// than silently dropping an uncertain task.
    pub fn finish_after_target_settlement(
        self,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<Result<usize>, Self> {
        let Self {
            handoff,
            channel,
            relay,
        } = self;
        match relay.finish_after_codex_stop(deadline) {
            Ok(result) => Ok(result),
            Err(relay) => Err(Self {
                handoff,
                channel,
                relay,
            }),
        }
    }
}

/// Called concurrently with the root's START point request: START waits for
/// this exact READY before releasing the held target. A transport failure is
/// terminal for this handoff; the daemon must perform its checked abort.
pub fn receive_and_ack(
    root_thread_id: &str,
    scenario_id: &str,
    expected_source: &ProductProducerRecipeSourceIdentity,
    expected_ingress: &ProducerLoopbackIngress,
    expected_origin: &str,
    provider_directory: PinnedDirectory,
    provider_socket_name: OsString,
    deadline: MonotonicDeadline,
) -> Result<RunningScopedRelay> {
    // SAFETY: the daemon's admitted root launch assigns unique ownership of
    // this connected channel to this verifier process under this exact name.
    // Lillux adopts and validates the descriptor before returning it.
    let mut channel = unsafe {
        lillux::take_inherited_duplex_channel_from_env("RYEOS_SCOPED_RELAY_FD")
    }
    .map_err(anyhow::Error::msg)
    .context("daemon-owned scoped relay channel absent")?;
    receive_and_ack_over_channel(
        &mut channel,
        root_thread_id,
        scenario_id,
        expected_source,
        expected_ingress,
        expected_origin,
        provider_directory,
        provider_socket_name,
        deadline,
    )
    .map(|(handoff, relay)| RunningScopedRelay {
        handoff,
        channel,
        relay,
    })
}

fn receive_and_ack_over_channel(
    channel: &mut InheritedDuplexChannel,
    root_thread_id: &str,
    scenario_id: &str,
    expected_source: &ProductProducerRecipeSourceIdentity,
    expected_ingress: &ProducerLoopbackIngress,
    expected_origin: &str,
    provider_directory: PinnedDirectory,
    provider_socket_name: OsString,
    deadline: MonotonicDeadline,
) -> Result<(ScopedRelayHandoff, RunningScriptedRelay)> {
    expected_ingress.validate()?;
    let origin = format!("http://{}", expected_ingress.address);
    let address: SocketAddr = expected_ingress.address.parse()?;
    ensure!(
        origin == format!("http://{address}") && origin == expected_origin,
        "signed scoped relay ingress differs from scripted provider origin"
    );
    let listener =
        ExactLoopbackListener::receive_over_inherited_duplex(channel, address, deadline)
            .context("exact scoped relay listener was not transferred")?;
    let handoff = receive_handoff(channel, deadline)?;
    handoff.validate_for_verifier(
        root_thread_id,
        scenario_id,
        expected_source,
        expected_ingress,
    )?;
    let relay = scripted_relay::start_from_transferred(
        &origin,
        listener,
        provider_directory,
        provider_socket_name,
        deadline,
    )?;
    // The relay is running before READY. If ACK delivery is ambiguous, its
    // Drop interrupts and settles the relay; no second ACK is attempted.
    send_ready(channel, &handoff, deadline)?;
    Ok((handoff, relay))
}
