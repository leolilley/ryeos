//! Parent-side half of the feature-only settled-command lost-ACK gate.

use std::io::Read as _;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use ryeos_app::dedicated_session_service::test_support::{
    COMMAND_SETTLED_FD_ENV, COMMAND_SETTLED_KEY_ENV, SettledCommandCutEvidence,
};
use tokio::process::Command;

pub struct DedicatedCommandCutGate {
    selected_key: String,
    reader: Option<lillux::InheritedDuplexChannel>,
}

pub struct DedicatedCommandCutChild(Option<lillux::InheritedDuplexChannelChildAuthority>);

impl DedicatedCommandCutGate {
    pub fn pair(selected_key: &str) -> Result<(Self, DedicatedCommandCutChild)> {
        ensure!(
            !selected_key.is_empty()
                && selected_key.len() <= 128
                && selected_key.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                }),
            "selected command key is not bounded ASCII"
        );
        let (reader, writer) = lillux::inherited_duplex_channel_pair()
            .map_err(anyhow::Error::msg)
            .context("create command-settlement crash channel")?;
        Ok((
            Self {
                selected_key: selected_key.to_owned(),
                reader: Some(reader),
            },
            DedicatedCommandCutChild(Some(writer)),
        ))
    }

    pub async fn wait_reached(&mut self) -> Result<SettledCommandCutEvidence> {
        let mut reader = self
            .reader
            .take()
            .context("command crash gate consumed twice")?;
        let selected_key = self.selected_key.clone();
        let bytes = tokio::time::timeout(
            Duration::from_secs(45),
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                let mut record = Vec::with_capacity(384);
                for _ in 0..=1024 {
                    let mut byte = [0u8; 1];
                    reader
                        .read_exact(&mut byte)
                        .context("read command-settlement crash evidence")?;
                    if byte[0] == b'\n' {
                        return Ok(record);
                    }
                    record.push(byte[0]);
                }
                anyhow::bail!("command-settlement crash evidence exceeds 1024 bytes")
            }),
        )
        .await
        .context("timed out waiting for settled-command crash cut")?
        .context("join settled-command crash reader")??;
        let evidence: SettledCommandCutEvidence =
            serde_json::from_slice(&bytes).context("decode settled-command crash evidence")?;
        evidence.validate()?;
        ensure!(
            evidence.idempotency_key == selected_key,
            "settled-command crash gate observed the wrong key"
        );
        Ok(evidence)
    }
}

impl DedicatedCommandCutChild {
    pub fn configure_command(&mut self, command: &mut Command, selected_key: &str) -> Result<()> {
        self.0
            .take()
            .context("settled-command child authority consumed twice")?
            .bind_to_command(command.as_std_mut(), COMMAND_SETTLED_FD_ENV)
            .map_err(anyhow::Error::msg)
            .context("bind command-settlement crash channel")?;
        command.env(COMMAND_SETTLED_KEY_ENV, selected_key);
        Ok(())
    }
}
