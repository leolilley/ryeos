//! Signed Q expectation input preparation and finite response DTOs.
//! Product transcript interpretation lives only in the selected checker.
//!
//! These checks do not establish native death, provider settlement, writer
//! exclusion or an issued qualification. Those facts have separate owners.
use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_external_execution_contract::qualification_fixture::{
    MAX_Q_FIXTURE_BYTES, QFixtureContract,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QCommandExpectation {
    pub schema: String,
    pub environment_id: String,
    pub command: String,
    pub command_shell: String,
    pub command_presentation: String,
    pub cwd: String,
    pub output: String,
    pub exit_code: i32,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub expected_final_content_b64: Option<String>,
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl QCommandExpectation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == "ryeos.codex.q_command.v3",
            "unsupported Q command expectation"
        );
        ensure!(
            self.environment_id == "ryeos-external-candidate",
            "Q command must name the external Codex environment"
        );
        for value in [&self.command, &self.command_presentation] {
            ensure!(
                !value.is_empty() && value.len() <= 4096 && !value.contains('\0'),
                "Q command expectation exceeds its text bound"
            );
        }
        ensure!(
            !self.output.is_empty() && self.output.len() <= 16384 && !self.output.contains('\0'),
            "Q output expectation exceeds its text bound"
        );
        ensure!(
            self.cwd == "/workspace",
            "Q command cwd differs from the admitted external workspace"
        );
        ensure!(
            self.exit_code == 0,
            "Q positive command expectation must require exit zero"
        );
        ensure!(
            self.command_presentation_for(&self.command)? == self.command_presentation,
            "Q positive command presentation differs from its signed shell argv"
        );
        ensure!(
            self.expected_final_content_b64.is_some(),
            "Q v3 positive write requires expected final fixture content"
        );
        self.decode_expected_content(MAX_Q_FIXTURE_BYTES)?;
        Ok(())
    }

    fn command_presentation_for(&self, command: &str) -> Result<String> {
        // Pinned Codex core/shell.rs derives these three argv elements for
        // Sh/Bash/Zsh with login=false. App-server item_builders.rs displays
        // shlex::try_join(argv), then redacts secrets. Any transformed display
        // that differs is refused; this is not an applied-shell proof.
        let shell = self.command_shell.as_str();
        ensure!(
            !shell.is_empty()
                && shell.len() <= 1024
                && shell
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
                && (matches!(shell, "sh" | "bash" | "zsh")
                    || (shell.starts_with('/')
                        && shell[1..]
                            .split('/')
                            .all(|part| !part.is_empty() && part != "." && part != "..")
                        && matches!(shell.rsplit('/').next(), Some("sh" | "bash" | "zsh")))),
            "Q command shell is not an exact bounded POSIX shell argv selection"
        );
        shlex::try_join([shell, "-c", command]).context("Q command argv is not representable")
    }

    fn decode_expected_content(&self, maximum: u64) -> Result<Option<Vec<u8>>> {
        let Some(encoded) = self.expected_final_content_b64.as_ref() else {
            return Ok(None);
        };
        ensure!(
            maximum <= MAX_Q_FIXTURE_BYTES && encoded.len() as u64 <= maximum.div_ceil(3) * 4,
            "Q expected fixture content exceeds its bound"
        );
        let mut bytes = vec![0; maximum as usize];
        let count = STANDARD
            .decode_slice(encoded, &mut bytes)
            .context("Q expected fixture content is not bounded base64")?;
        bytes.truncate(count);
        ensure!(
            STANDARD.encode(&bytes) == *encoded,
            "Q expected fixture content is not canonical base64"
        );
        Ok(Some(bytes))
    }

    pub(crate) fn expected_fixture_content(
        &self,
        fixture: Option<&QFixtureContract>,
    ) -> Result<Option<Vec<u8>>> {
        self.validate()?;
        match (fixture, self.expected_final_content_b64.as_ref()) {
            (Some(fixture), Some(_)) => {
                let initial = fixture.decoded_initial_content()?;
                let expected = self
                    .decode_expected_content(fixture.max_bytes)?
                    .context("Q expected fixture bytes missing")?;
                ensure!(
                    expected != initial,
                    "Q expected final fixture content must differ from its signed initial content"
                );
                Ok(Some(expected))
            }
            _ => bail!("Q expected fixture presence differs from the signed Worker request"),
        }
    }
}

/// Checked app-server semantics, not a qualification result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckedQConversation {
    pub thread_id: String,
    pub turn_id: String,
    pub fixture_write_call_id: String,
    pub canary_read_call_id: String,
    pub local_refusal_call_id: String,
    pub notification_count: usize,
}

// Construction is private to the retained host's complete terminal join.
pub use crate::consumer_checker_host::VerifiedQConversation;

#[cfg(test)]
mod tests {
    use super::*;

    fn expectation() -> QCommandExpectation {
        QCommandExpectation {
            schema: "ryeos.codex.q_command.v3".into(),
            environment_id: "ryeos-external-candidate".into(),
            command: "printf probe".into(),
            command_shell: "zsh".into(),
            command_presentation: "zsh -c 'printf probe'".into(),
            cwd: "/workspace".into(),
            output: "probe".into(),
            exit_code: 0,
            expected_final_content_b64: Some(STANDARD.encode(b"after")),
        }
    }

    // Input-authoring controls only; no transcript interpretation or launch.
    #[test]
    fn signed_expectation_requires_exact_shell_presentation_and_final_bytes() {
        let original = expectation();
        original.validate().unwrap();
        let mut changed = original.clone();
        changed.command_presentation = "different argv".into();
        assert!(changed.validate().is_err());
        changed = original;
        changed.expected_final_content_b64 = None;
        assert!(changed.validate().is_err());
    }

    #[test]
    fn signed_fixture_input_must_change_its_original_bytes() {
        let expected = expectation();
        assert!(expected.expected_fixture_content(None).is_err());
        assert_eq!(
            expected
                .decode_expected_content(MAX_Q_FIXTURE_BYTES)
                .unwrap()
                .unwrap(),
            b"after"
        );
        let mut changed = expected;
        changed.expected_final_content_b64 = Some("YQ==\n".into());
        assert!(changed.validate().is_err());
    }
}
