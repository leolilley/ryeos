//! Direct native transport refusals, not app-server or qualification evidence.
//! Initialization must succeed before fault injection, so staging/namespace
//! refusal cannot accidentally satisfy a negative transport test.

use super::direct_guest::DirectGuest;
use super::*;

fn refusal(guest: &DirectGuest) -> Result<String> {
    ensure!(
        !guest.root.join("finish").exists(),
        "test accidentally requested capture"
    );
    ensure!(
        !guest.root.join("guest-observation.json").exists(),
        "failed transport published successful observation"
    );
    let failure = guest.read_result("guest-failure.json")?;
    ensure!(
        failure["schema"] == "test.routed_guest_failure.v1"
            && failure["settlement"] == "not_attested",
        "failure diagnostic claimed settlement"
    );
    Ok(failure["reason"]
        .as_str()
        .context("failure reason absent")?
        .to_owned())
}

#[test]
#[ignore = "exact pinned Codex + closed tool inputs + native namespace support; no model contact"]
fn native_guest_controller_stdin_eof_without_finish_refuses() -> Result<()> {
    let mut guest = DirectGuest::start(4096)?;
    guest.initialize()?;
    // Real initialization response proves that this occurrence passed staging,
    // native preparation/release and the guest exec-server protocol boundary.
    drop(guest.input.take());
    let status = guest.settle_until(MonotonicDeadline::after(
        lillux::time::Duration::from_secs(15),
    ))?;
    ensure!(!status.success, "controller EOF unexpectedly succeeded");
    let reason = refusal(&guest)?;
    // The released exec-server may close stdout immediately when the relay
    // propagates stdin EOF; either observed side of that closure is refusal.
    ensure!(
        reason.contains("controller input ended without finish")
            || reason.contains("guest output ended before verifier finish")
            || reason.contains("guest exited before verifier finish"),
        "wrong refusal boundary: {reason}"
    );
    // Nonzero exit/no observation do not independently attest namespace death.
    Ok(())
}

#[test]
#[ignore = "exact pinned Codex + native namespaces; exercises the real 90-second output deadline"]
fn native_guest_blocked_controller_stdout_expires_without_capture() -> Result<()> {
    let started = lillux::time::MonotonicTimer::start();
    // Harness-owned synthetic input, not a guest mutation or a captured result.
    // The resulting base64 response is below the transcript ceiling but much
    // larger than the default inherited Linux x86-64 stdout pipe capacity.
    let seed = vec![b'x'; 512 * 1024];
    let mut guest = DirectGuest::start_with_candidate(4096, Some(("backpressure.bin", &seed)))?;
    guest.initialize()?;
    guest.send(json!({"id":2002,"method":"fs/readFile","params":{
        "path":"file:///workspace/backpressure.bin","sandbox":null
    }}))?;
    ensure!(
        guest.output.read_frame_until(
            b'{',
            1,
            MonotonicDeadline::after(lillux::time::Duration::from_secs(10))
        )? == b"{",
        "native guest did not begin its response"
    );
    // Keep stdin AND the unread stdout endpoint alive. No EOF, finish file,
    // reader task, or kill may release the relay before its own deadline.
    let status = guest.settle_until(MonotonicDeadline::after(
        lillux::time::Duration::from_secs(110),
    ))?;
    ensure!(!status.success, "blocked output unexpectedly succeeded");
    ensure!(
        started.elapsed() >= lillux::time::Duration::from_secs(85),
        "refusal did not exercise the real active deadline"
    );
    let reason = refusal(&guest)?;
    ensure!(
        reason.contains("inherited pipe deadline elapsed")
            && !reason.contains("read controller protocol"),
        "expected inherited output expiry, not an idle/protocol refusal: {reason}"
    );
    // Deliberately never drain or retry the uncertain output prefix.
    Ok(())
}
