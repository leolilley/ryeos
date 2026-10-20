//! Parent-side half of the feature-only runtime crash-qualification gate.

use std::io::Read as _;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::process::Command;

const PHASE_ENV: &str = "RYEOSD_TEST_RUNTIME_PHASE_CUT";
const CHANNEL_FD_ENV: &str = "RYEOSD_TEST_RUNTIME_PHASE_CUT_FD";

pub struct RuntimePhaseCutGate {
    expected: String,
    reader: Option<lillux::InheritedDuplexChannel>,
}

pub struct RuntimePhaseCutChild(Option<lillux::InheritedDuplexChannelChildAuthority>);

impl RuntimePhaseCutGate {
    pub fn pair(expected: &str) -> Result<(Self, RuntimePhaseCutChild)> {
        anyhow::ensure!(
            !expected.is_empty()
                && expected.len() <= 128
                && expected
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
            "runtime phase is not bounded lower-snake ASCII"
        );
        let (reader, authority) = lillux::inherited_duplex_channel_pair()
            .map_err(anyhow::Error::msg)
            .context("create runtime phase-cut channel")?;
        Ok((
            Self {
                expected: expected.to_owned(),
                reader: Some(reader),
            },
            RuntimePhaseCutChild(Some(authority)),
        ))
    }

    pub async fn wait_reached(&mut self) -> Result<()> {
        let mut reader = self
            .reader
            .take()
            .context("runtime phase-cut gate was already consumed")?;
        let expected = self.expected.clone();
        let observed = tokio::time::timeout(
            Duration::from_secs(45),
            tokio::task::spawn_blocking(move || -> Result<String> {
                let mut record = Vec::with_capacity(129);
                for _ in 0..=128 {
                    let mut byte = [0u8; 1];
                    reader
                        .read_exact(&mut byte)
                        .context("read runtime phase-cut evidence")?;
                    if byte[0] == b'\n' {
                        return String::from_utf8(record)
                            .context("runtime phase-cut evidence is not UTF-8");
                    }
                    record.push(byte[0]);
                }
                anyhow::bail!("runtime phase-cut evidence exceeds 128 bytes")
            }),
        )
        .await
        .context("timed out waiting for runtime phase-cut evidence")?
        .context("join runtime phase-cut reader")??;
        anyhow::ensure!(
            observed == expected,
            "runtime phase-cut gate expected `{expected}` but observed `{observed}`"
        );
        Ok(())
    }
}

impl RuntimePhaseCutChild {
    /// Configure one daemon spawn while retaining this exact authority until
    /// that spawn completes.
    pub fn configure_command(&mut self, command: &mut Command, phase: &str) -> Result<()> {
        self.0
            .take()
            .context("runtime phase-cut child authority was already consumed")?
            .bind_to_command(command.as_std_mut(), CHANNEL_FD_ENV)
            .map_err(anyhow::Error::msg)?;
        command.env(PHASE_ENV, phase);
        Ok(())
    }
}
