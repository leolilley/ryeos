//! ryeos-tui — Native terminal TUI for Rye OS.
//!
//! A tiled workspace for AI agent operations: thread management,
//! execution, state inspection, remotes, and trust.

mod app;
mod bootstrap;
mod braille;
mod capabilities;
mod daemon;
mod mock_transport;
mod persistence;
mod render;
mod render_scene;
mod render_text;
mod sse;
mod terminal;
mod transport;

fn surface_diagnostic_message(diag: &ryeos_tui_core::surface::SurfaceDiagnostic) -> &str {
    match diag {
        ryeos_tui_core::surface::SurfaceDiagnostic::ValidationError { message }
        | ryeos_tui_core::surface::SurfaceDiagnostic::Info { message }
        | ryeos_tui_core::surface::SurfaceDiagnostic::UnsupportedField { message, .. } => message,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut mock = false;
    let mut project_path = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".into());
    let mut surface_file: Option<String> = None;
    let mut surface_name: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--mock" => mock = true,
            "--surface-file" => {
                i += 1;
                if i < args.len() {
                    surface_file = Some(args[i].clone());
                } else {
                    eprintln!("--surface-file requires a path argument");
                    std::process::exit(1);
                }
            }
            "--surface" => {
                i += 1;
                if i < args.len() {
                    surface_name = Some(args[i].clone());
                } else {
                    eprintln!("--surface requires a name argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("Usage: ryeos-tui [OPTIONS] [PROJECT_PATH]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --mock                  Use mock data (no daemon required)");
                eprintln!(
                    "  --surface-file <PATH>   Load surface spec from a local file (untrusted)"
                );
                eprintln!("  --surface <REF>         Load surface by canonical ref via daemon");
                eprintln!("  --help                  Show this help");
                std::process::exit(0);
            }
            p if !p.starts_with('-') => {
                project_path = p.to_string();
            }
            _ => {
                eprintln!("Unknown option: {}", args[i]);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    rt.block_on(async {
        // Load surface
        let surface_opts = ryeos_tui_core::surface::SurfaceLoadOptions {
            explicit_file: surface_file.map(std::path::PathBuf::from),
            surface_name: None,
        };

        // If --surface was given, resolve through daemon.
        // --surface always means daemon resolution, not local preview.
        let loaded: ryeos_tui_core::surface::LoadedSurface =
            if surface_name.is_some() {
                if mock {
                    eprintln!("error: --surface requires daemon-backed effective item resolution");
                    eprintln!("hint: omit --surface for mock mode, or use --surface-file <path> for local preview");
                    std::process::exit(1);
                }
                match daemon::DaemonClient::try_connect().await {
                    Ok(client) => {
                        let ref_str = surface_name.as_deref().unwrap();
                        eprintln!("info: resolving {} via daemon...", ref_str);
                        match client
                            .resolve_effective_surface(ref_str, Some(&project_path))
                            .await
                        {
                            Ok(value) => match ryeos_tui_core::surface::LoadedSurface::from_daemon(
                                ref_str, value,
                            ) {
                                Ok(surface) => surface,
                                Err(diag) => {
                                    eprintln!(
                                        "error: invalid effective surface '{}': {}",
                                        ref_str,
                                        surface_diagnostic_message(&diag)
                                    );
                                    std::process::exit(1);
                                }
                            },
                            Err(e) => {
                                // Explicit surface request that fails — fail closed.
                                eprintln!("error: failed to resolve surface '{}': {}", ref_str, e);
                                eprintln!("hint: use --surface-file <path> for local preview");
                                std::process::exit(1);
                            }
                        }
                    }
                    Err(_) => {
                        let ref_str = surface_name.as_deref().unwrap();
                        eprintln!("error: failed to resolve surface '{}': daemon not available", ref_str);
                        eprintln!("hint: start ryeosd, or use --surface-file <path> for local preview");
                        std::process::exit(1);
                    }
                }
            } else {
                ryeos_tui_core::surface::load_surface(&surface_opts)
            };

        // Surface diagnostics
        for diag in loaded.all_diagnostics() {
            match diag {
                ryeos_tui_core::surface::SurfaceDiagnostic::ValidationError { message } => {
                    eprintln!("error: {}", message);
                }
                ryeos_tui_core::surface::SurfaceDiagnostic::UnsupportedField {
                    field,
                    message,
                } => {
                    eprintln!("warn: unsupported field '{}': {}", field, message);
                }
                ryeos_tui_core::surface::SurfaceDiagnostic::Info { message } => {
                    eprintln!("info: {}", message);
                }
            }
        }

        if let Err(e) = app::run(&project_path, mock, loaded).await {
            eprintln!("ryeos-tui error: {}", e);
            std::process::exit(1);
        }
    });
}
