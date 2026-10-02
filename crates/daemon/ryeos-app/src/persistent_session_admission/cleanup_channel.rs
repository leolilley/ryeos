//! Product-private normal-close transport. A candidate frame is NOT accepted
//! driver retirement. The executable owner must separately authenticate the
//! admitted producer, original reservation/channel and actual held occurrence,
//! then join independent group retirement and the existing financial owner.
//!
//! This transport remains unwired while those executable prerequisites refuse.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

const MAX_FRAME_BYTES: usize = 4096;
pub(crate) const CHANNEL_ENV: &str = "RYEOS_RESOURCE_CLEANUP_FD";
pub(crate) const TARGET_FD: u32 = 4;

/// Comparison coordinates only; construction does not authenticate an owner.
/// The reservation ID is the existing canonical original reservation digest;
/// process/resource digests come from ExecutionProcessIdentity's existing APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupBindings {
    pub(crate) teardown: String,
    pub(crate) capsule_hash: String,
    pub(crate) cleanup_contract: String,
    pub(crate) process_owner: String,
    pub(crate) resource_set: String,
}

impl CleanupBindings {
    fn validate(&self) -> Result<()> {
        for digest in [
            &self.teardown,
            &self.capsule_hash,
            &self.cleanup_contract,
            &self.process_owner,
            &self.resource_set,
        ] {
            ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "cleanup comparison coordinate is not an existing canonical digest"
            );
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    version: u32,
    kind: String,
    teardown: String,
    capsule_hash: String,
    cleanup_contract: String,
    process_owner: String,
    resource_set: String,
}

impl Frame {
    fn new(kind: &str, bindings: &CleanupBindings) -> Self {
        Self {
            version: 1,
            kind: kind.to_owned(),
            teardown: bindings.teardown.clone(),
            capsule_hash: bindings.capsule_hash.clone(),
            cleanup_contract: bindings.cleanup_contract.clone(),
            process_owner: bindings.process_owner.clone(),
            resource_set: bindings.resource_set.clone(),
        }
    }

    fn matches(&self, kind: &str, bindings: &CleanupBindings) -> Result<()> {
        ensure!(
            self.version == 1 && self.kind == kind,
            "unexpected cleanup frame"
        );
        let actual = CleanupBindings {
            teardown: self.teardown.clone(),
            capsule_hash: self.capsule_hash.clone(),
            cleanup_contract: self.cleanup_contract.clone(),
            process_owner: self.process_owner.clone(),
            resource_set: self.resource_set.clone(),
        };
        actual.validate()?;
        ensure!(
            &actual == bindings,
            "cleanup frame belongs to different comparison coordinates"
        );
        Ok(())
    }
}

fn write_frame(writer: &mut impl Write, kind: &str, bindings: &CleanupBindings) -> Result<()> {
    bindings.validate()?;
    let body = serde_json::to_vec(&Frame::new(kind, bindings))?;
    ensure!(
        !body.is_empty() && body.len() <= MAX_FRAME_BYTES,
        "cleanup frame exceeds its bound"
    );
    writer.write_all(&(body.len() as u32).to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn read_retired(reader: &mut impl Read, bindings: &CleanupBindings) -> Result<()> {
    let mut header = [0u8; 4];
    reader
        .read_exact(&mut header)
        .context("read bounded cleanup header")?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=MAX_FRAME_BYTES).contains(&length),
        "cleanup frame exceeds its bound"
    );
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .context("read bounded cleanup body")?;
    let frame: Frame = serde_json::from_slice(&body).context("decode strict cleanup frame")?;
    frame.matches("retired", bindings)?;
    // Successful frame alone is insufficient. Require producer write closure;
    // duplicate frames, trailing bytes and a stalled peer all refuse.
    let mut extra = [0u8; 1];
    loop {
        match reader.read(&mut extra) {
            Ok(0) => return Ok(()),
            Ok(_) => bail!("cleanup producer sent trailing or duplicate bytes"),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("observe cleanup producer write closure"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Created,
    InitQueued,
    ShutdownSent,
    CandidateRead,
    Failed,
}

/// One original channel and absolute deadline, never a per-frame timeout.
/// No API returns RuntimeDb cleanup evidence, financial authority or release.
pub(crate) struct TrustedCleanupChannel {
    channel: lillux::InheritedDuplexChannel,
    bindings: Option<CleanupBindings>,
    deadline: lillux::time::MonotonicDeadline,
    phase: Phase,
}

pub(crate) struct PendingDriverClose {
    bindings: CleanupBindings,
}

impl PendingDriverClose {
    pub(crate) fn bindings(&self) -> &CleanupBindings {
        &self.bindings
    }
}

impl TrustedCleanupChannel {
    pub(crate) fn prepare(
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<(
        Self,
        ryeos_engine::isolation::IsolationTargetChannelAuthority,
    )> {
        ensure!(
            !deadline.remaining().is_zero(),
            "cleanup original deadline expired"
        );
        let (channel, child) =
            lillux::inherited_duplex_channel_pair().map_err(anyhow::Error::msg)?;
        let target = ryeos_engine::isolation::IsolationTargetChannelAuthority::new(
            child,
            TARGET_FD,
            CHANNEL_ENV,
        )?;
        Ok((
            Self {
                channel,
                bindings: None,
                deadline,
                phase: Phase::Created,
            },
            target,
        ))
    }

    /// Queue only AFTER actual held birth/durable attachment and BEFORE release.
    /// No read/child acknowledgment occurs while the child is held.
    pub(crate) fn queue_init(&mut self, bindings: CleanupBindings) -> Result<()> {
        let previous = std::mem::replace(&mut self.phase, Phase::Failed);
        ensure!(
            previous == Phase::Created,
            "cleanup initialization was reused"
        );
        bindings.validate()?;
        write_frame(
            &mut self.channel.with_deadline(self.deadline),
            "init",
            &bindings,
        )?;
        self.bindings = Some(bindings);
        self.phase = Phase::InitQueued;
        Ok(())
    }

    /// Caller closes/quiesces the existing session request owner first.
    pub(crate) fn request_retirement(&mut self) -> Result<()> {
        let previous = std::mem::replace(&mut self.phase, Phase::Failed);
        ensure!(
            previous == Phase::InitQueued,
            "cleanup teardown was premature or reused"
        );
        self.channel
            .ensure_peer_live_and_quiet()
            .context("cleanup producer sent data before authorized teardown")?;
        write_frame(
            &mut self.channel.with_deadline(self.deadline),
            "shutdown",
            self.bindings
                .as_ref()
                .context("cleanup original bindings are missing")?,
        )?;
        self.phase = Phase::ShutdownSent;
        Ok(())
    }

    pub(crate) fn receive_candidate(&mut self) -> Result<PendingDriverClose> {
        let previous = std::mem::replace(&mut self.phase, Phase::Failed);
        ensure!(
            previous == Phase::ShutdownSent,
            "cleanup observation was premature or reused"
        );
        let bindings = self
            .bindings
            .as_ref()
            .context("cleanup original bindings are missing")?;
        read_retired(&mut self.channel.with_deadline(self.deadline), bindings)?;
        self.phase = Phase::CandidateRead;
        Ok(PendingDriverClose {
            bindings: bindings.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn bindings() -> CleanupBindings {
        CleanupBindings {
            teardown: "1".repeat(64),
            capsule_hash: "2".repeat(64),
            cleanup_contract: "3".repeat(64),
            process_owner: "4".repeat(64),
            resource_set: "5".repeat(64),
        }
    }

    fn framed(body: &[u8]) -> Vec<u8> {
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn cleanup_wire_requires_exact_single_retired_and_write_closure() {
        let bindings = bindings();
        let mut bytes = Vec::new();
        write_frame(&mut bytes, "retired", &bindings).unwrap();
        read_retired(&mut Cursor::new(&bytes), &bindings).unwrap();
        let mut duplicate = bytes.clone();
        duplicate.extend_from_slice(&bytes);
        assert!(read_retired(&mut Cursor::new(duplicate), &bindings).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(read_retired(&mut Cursor::new(trailing), &bindings).is_err());
        for end in 0..bytes.len() {
            assert!(read_retired(&mut Cursor::new(&bytes[..end]), &bindings).is_err());
        }
    }

    #[test]
    fn cleanup_wire_refuses_foreign_coordinates_and_protocol_shapes() {
        let expected = bindings();
        let original = serde_json::to_value(Frame::new("retired", &expected)).unwrap();
        for name in [
            "teardown",
            "capsule_hash",
            "cleanup_contract",
            "process_owner",
            "resource_set",
        ] {
            let mut altered = original.clone();
            altered[name] = serde_json::json!("a".repeat(64));
            assert!(
                read_retired(
                    &mut Cursor::new(framed(&serde_json::to_vec(&altered).unwrap())),
                    &expected
                )
                .is_err()
            );
        }
        for name in ["init", "shutdown", "error", "final", "observation_batch"] {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, name, &expected).unwrap();
            assert!(read_retired(&mut Cursor::new(bytes), &expected).is_err());
        }
        let body = serde_json::to_string(&original).unwrap();
        let duplicate = body.replacen('{', "{\"version\":1,", 1);
        let extra = body.replacen('{', "{\"verified\":true,", 1);
        for body in [
            duplicate.as_bytes(),
            extra.as_bytes(),
            b"null",
            b"\xff",
            b"{}",
        ] {
            assert!(read_retired(&mut Cursor::new(framed(body)), &expected).is_err());
        }
        for length in [0u32, 4097, u32::MAX] {
            assert!(read_retired(&mut Cursor::new(length.to_be_bytes()), &expected).is_err());
        }
    }
}
