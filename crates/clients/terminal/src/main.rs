//! Native terminal renderer for an admitted RyeOS shared presentation.
//! The authored Config selects shared mounts and initial Surface compositions;
//! this entry point does not manufacture a Surface or infer project authority.

mod app;
mod render;
mod render_text;
mod terminal;
mod transport;

use ryeos_cli::tty::{Console, Diagnostic, Document, Hint, OperationKind, Row, Section};
use std::io::{self, IsTerminal, Write};

fn exit_with_error(console: &Console, message: impl Into<String>, hint: Option<&str>) -> ! {
    let mut diagnostic = Diagnostic::error(message);
    diagnostic.hint = hint.map(Hint::new);
    let _ = console.error(&diagnostic);
    std::process::exit(1);
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
    eprint!("Still-live UI session credential: ");
    io::stderr().flush()?;
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
    eprintln!();
    result
}

fn print_help(console: &Console) {
    let mut document = Document::titled("RYEOS TERMINAL");
    let mut options = Section::named("options");
    options.rows = vec![
        Row::key_value(
            "--presentation <CONFIG>",
            "Signed shared presentation Config (required)",
        ),
        Row::key_value(
            "--project <PATH>",
            "Explicit project context; absent means node-level",
        ),
        Row::key_value(
            "--resume-presentation <ID>",
            "Resume one exact retained presentation; prompts privately for a live UI session credential",
        ),
        Row::key_value("--help", "Show this help"),
    ];
    document.sections.push(options);
    let _ = console.document(&document);
}

fn main() {
    let console = Console::detect(false);
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        print_help(&console);
        return;
    }
    let options = parse_options(&args).unwrap_or_else(|error| {
        exit_with_error(
            &console,
            error,
            Some("use ryeos tui to select the authored terminal presentation"),
        )
    });
    let runtime = tokio::runtime::Runtime::new().unwrap_or_else(|error| {
        exit_with_error(
            &console,
            format!("failed to create terminal runtime: {error}"),
            None,
        )
    });
    runtime.block_on(async {
        let client = transport::daemon::DaemonClient::try_connect()
            .await
            .unwrap_or_else(|error| {
                exit_with_error(
                    &console,
                    format!("daemon connection failed: {error}"),
                    Some("check ryeos node status"),
                )
            });
        let resume_id = match options {
            LaunchOptions::Fresh { presentation, project } => {
                let mut progress = console.progress(OperationKind::Fetch, "opening UI session").ok().flatten();
                if let Some(progress) = progress.as_mut() {
                    let _ = progress.update("admitting shared presentation", Some(&presentation));
                }
                let admitted = client.mint_ui_session(&presentation, project.as_deref()).await;
                if let Some(progress) = progress { let _ = progress.finish(); }
                if let Err(error) = admitted {
                    exit_with_error(&console, format!("failed to initialize UI session: {error}"), None);
                }
                None
            }
            LaunchOptions::Resume { presentation_id } => {
                let credential = prompt_session_id().unwrap_or_else(|error| {
                    exit_with_error(&console, format!("cannot read private UI session credential: {error}"), None)
                });
                client.adopt_ui_session(&credential).unwrap_or_else(|error| {
                    exit_with_error(&console, format!("invalid UI session credential: {error}"), None)
                });
                let session = client.current_ui_session().await.unwrap_or_else(|error| {
                    exit_with_error(&console, format!("UI session authentication failed: {error}"), None)
                });
                if session.session_id != credential {
                    exit_with_error(&console, "authenticated UI session did not match the supplied credential", None);
                }
                if session.presentations.iter().filter(|membership| membership.presentation_id == presentation_id).count() != 1 {
                    exit_with_error(&console, "authenticated UI session has no unique membership for that presentation ID", None);
                }
                Some(presentation_id)
            }
        };
        if let Err(error) = app::run(Vec::new(), client, resume_id).await {
            exit_with_error(
                &console,
                format!("terminal presentation failed: {error}"),
                None,
            );
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
