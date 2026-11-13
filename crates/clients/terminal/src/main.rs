//! Native terminal renderer for an admitted RyeOS shared presentation.
//! The authored Config selects shared mounts and initial Surface compositions;
//! this entry point does not manufacture a Surface or infer project authority.

mod app;
mod render;
mod render_text;
mod terminal;

use std::io::{self, IsTerminal};
use std::rc::Rc;

use ryeos_client_base::ui::BrowserSession;
use ryeos_client_runtime::platform::native::{MintRequest, NativePlatform};
use ryeos_client_runtime::platform::{Auth, EstablishMode, Platform};
use ryeos_client_runtime::runtime::StartMode;
use ryeos_client_runtime::seat::RetainedClaim;

/// Pre-launch messages go to stderr before the alternate screen opens, in the
/// same plain line form as the CLI's non-terminal diagnostics.
fn exit_with_error(message: impl Into<String>, hint: Option<&str>) -> ! {
    ryeos_client_base::diagnostic::fatal(&message.into(), hint)
}

#[derive(Debug, PartialEq)]
enum LaunchOptions {
    Fresh {
        presentation: String,
        project: Option<String>,
    },
    Resume {
        presentation_id: String,
    },
}

fn parse_options(args: &[String]) -> Result<LaunchOptions, String> {
    let mut presentation = None;
    let mut project = None;
    let mut resume_presentation = None;
    let mut i = 0;
    while i < args.len() {
        let destination = match args[i].as_str() {
            "--presentation" => &mut presentation,
            "--project" => &mut project,
            "--resume-presentation" => &mut resume_presentation,
            flag => return Err(format!("unknown option: {flag}")),
        };
        if destination.is_some() {
            return Err(format!("duplicate option: {}", args[i]));
        }
        i += 1;
        let value = args
            .get(i)
            .filter(|value| !value.is_empty() && !value.starts_with('-'))
            .ok_or_else(|| format!("{} requires an argument", args[i - 1]))?;
        *destination = Some(value.clone());
        i += 1;
    }
    if let Some(presentation_id) = resume_presentation {
        if presentation.is_some() || project.is_some() {
            return Err(
                "--resume-presentation cannot be combined with --presentation or --project".into(),
            );
        }
        if presentation_id.len() > 128
            || !presentation_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err("--resume-presentation requires a bounded exact presentation ID".into());
        }
        return Ok(LaunchOptions::Resume { presentation_id });
    }
    let presentation =
        presentation.ok_or("--presentation requires an authored shared Config ref")?;
    if !presentation.starts_with("config:") {
        return Err(
            "--presentation must select a Config; initial Surfaces belong to that Config".into(),
        );
    }
    Ok(LaunchOptions::Fresh {
        presentation,
        project,
    })
}

struct PromptRawMode;

fn valid_session_credential(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

impl Drop for PromptRawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn prompt_session_id() -> io::Result<String> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};

    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(io::Error::other("Resume requires an interactive terminal"));
    }
    ryeos_client_base::diagnostic::stderr_prompt("Still-live UI session credential: ");
    crossterm::terminal::enable_raw_mode()?;
    let guard = PromptRawMode;
    let mut credential = String::new();
    let result = loop {
        match event::read()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Enter if valid_session_credential(&credential) => {
                        break Ok(credential);
                    }
                    KeyCode::Enter => {
                        break Err(io::Error::other("session credential is required"));
                    }
                    KeyCode::Esc => {
                        break Err(io::Error::other("session credential entry cancelled"));
                    }
                    KeyCode::Backspace => {
                        credential.pop();
                    }
                    KeyCode::Char(ch) if ch.is_ascii_alphanumeric() || ch == '-' => {
                        if credential.len() == 64 {
                            break Err(io::Error::other(
                                "session credential exceeds the 64-byte limit",
                            ));
                        }
                        credential.push(ch);
                    }
                    KeyCode::Char(_) => {
                        break Err(io::Error::other(
                            "session credential contains an invalid character",
                        ));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    };
    drop(guard);
    ryeos_client_base::diagnostic::stderr_newline();
    result
}

fn print_help() {
    ryeos_client_base::diagnostic::stdout_line(
        "RYEOS TERMINAL

OPTIONS
--presentation <CONFIG>: Signed shared presentation Config (required)
--project <PATH>: Explicit project context; absent means node-level
--resume-presentation <ID>: Resume one exact retained presentation; prompts privately for a live UI session credential
--help: Show this help",
    );
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        print_help();
        return;
    }
    let options = parse_options(&args).unwrap_or_else(|error| {
        exit_with_error(
            error,
            Some("use ryeos tui to select the authored terminal presentation"),
        )
    });
    // One thread: the runtime's tasks are local (not `Send`) and share state
    // with the loop that folds them.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| {
            exit_with_error(format!("failed to create terminal runtime: {error}"), None)
        });
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async {
        let mint = match &options {
            LaunchOptions::Fresh {
                presentation,
                project,
            } => Some(MintRequest {
                config_ref: presentation.clone(),
                project_path: project.clone(),
            }),
            LaunchOptions::Resume { .. } => None,
        };
        let platform = Rc::new(NativePlatform::connect(mint).await.unwrap_or_else(|error| {
            exit_with_error(
                format!("daemon connection failed: {error}"),
                Some("check ryeos node status"),
            )
        }));
        let (established, mode) = match options {
            LaunchOptions::Fresh { .. } => {
                let established = platform
                    .auth()
                    .establish(EstablishMode::Fresh)
                    .await
                    .unwrap_or_else(|error| {
                        exit_with_error(format!("failed to initialize UI session: {error}"), None)
                    });
                (established, StartMode::Fresh)
            }
            LaunchOptions::Resume { presentation_id } => {
                let credential = prompt_session_id().unwrap_or_else(|error| {
                    exit_with_error(
                        format!("cannot read private UI session credential: {error}"),
                        None,
                    )
                });
                let established = platform
                    .auth()
                    .establish(EstablishMode::Adopt(credential))
                    .await
                    .unwrap_or_else(|error| {
                        exit_with_error(format!("UI session authentication failed: {error}"), None)
                    });
                (
                    established,
                    StartMode::Retained {
                        presentation_id,
                        claim: RetainedClaim::ResumeRelinquished,
                    },
                )
            }
        };
        let session =
            serde_json::from_value::<BrowserSession>(established.current).unwrap_or_else(|error| {
                exit_with_error(format!("UI session record is unreadable: {error}"), None)
            });
        if let StartMode::Retained {
            presentation_id, ..
        } = &mode
            && session
                .presentations
                .iter()
                .filter(|membership| membership.presentation_id == *presentation_id)
                .count()
                != 1
        {
            exit_with_error(
                "authenticated UI session has no unique membership for that presentation ID",
                None,
            );
        }
        if let Err(error) = app::run(Vec::new(), platform, session, mode).await {
            exit_with_error(format!("terminal presentation failed: {error}"), None);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn node_launch_does_not_infer_a_project_or_surface() {
        assert_eq!(
            parse_options(&args(&["--presentation", "config:test/terminal"])).unwrap(),
            LaunchOptions::Fresh {
                presentation: "config:test/terminal".into(),
                project: None
            }
        );
        assert!(parse_options(&[]).is_err());
        assert!(parse_options(&args(&["--presentation", "surface:test/lens"])).is_err());
    }

    #[test]
    fn explicit_project_is_preserved_without_default_owner() {
        assert_eq!(
            parse_options(&args(&[
                "--presentation",
                "config:test/terminal",
                "--project",
                "/project"
            ]))
            .unwrap(),
            LaunchOptions::Fresh {
                presentation: "config:test/terminal".into(),
                project: Some("/project".into())
            }
        );
    }

    #[test]
    fn resume_requires_exact_id_and_excludes_fresh_options() {
        assert_eq!(
            parse_options(&args(&["--resume-presentation", "p-123"])).unwrap(),
            LaunchOptions::Resume {
                presentation_id: "p-123".into()
            }
        );
        for flags in [
            vec![
                "--resume-presentation",
                "p-123",
                "--presentation",
                "config:test/terminal",
            ],
            vec!["--resume-presentation", "p-123", "--project", "/project"],
            vec!["--resume-presentation", "invalid/id"],
        ] {
            assert!(parse_options(&args(&flags)).is_err());
        }
    }

    #[test]
    fn private_prompt_accepts_only_bounded_transport_tokens() {
        assert!(valid_session_credential("live-123"));
        assert!(!valid_session_credential(""));
        assert!(!valid_session_credential("bad/token"));
        assert!(!valid_session_credential(&"a".repeat(65)));
    }

    #[test]
    fn predecessor_live_preview_flags_are_not_production_aliases() {
        for flag in ["--surface", "--surface-file", "--views-root", "--mock"] {
            assert!(parse_options(&args(&[flag, "ignored"])).is_err());
        }
        assert!(parse_options(&args(&["--presentation"])).is_err());
        assert!(
            parse_options(&args(&[
                "--presentation",
                "config:test/terminal",
                "--presentation",
                "config:test/other"
            ]))
            .is_err()
        );
    }
}
