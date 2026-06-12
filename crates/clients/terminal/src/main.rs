//! ryeos-tui — Native terminal TUI for Rye OS.
//!
//! A tiled workspace for AI agent operations: thread management,
//! execution, state inspection, remotes, and trust.

mod daemon;
mod render_text;
mod sse;
mod studio_app;
mod studio_render;
mod terminal;

fn surface_diagnostic_message(diag: &ryeos_client_base::surface::SurfaceDiagnostic) -> &str {
    match diag {
        ryeos_client_base::surface::SurfaceDiagnostic::ValidationError { message }
        | ryeos_client_base::surface::SurfaceDiagnostic::Info { message }
        | ryeos_client_base::surface::SurfaceDiagnostic::UnsupportedField { message, .. } => {
            message
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut project_path = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".into());
    let mut surface_file: Option<String> = None;
    let mut surface_name: Option<String> = None;
    let mut read_only = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--read-only" => read_only = true,
            "--project" => {
                i += 1;
                if i < args.len() {
                    project_path = args[i].clone();
                } else {
                    eprintln!("--project requires a path argument");
                    std::process::exit(1);
                }
            }
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
                eprintln!(
                    "  --surface <REF>         Open a surface by canonical ref (default: surface:ryeos/studio/base)"
                );
                eprintln!(
                    "  --surface-file <PATH>   Load surface spec from a local file (untrusted preview)"
                );
                eprintln!("  --project <PATH>        Project root for daemon-backed resolution");
                eprintln!("  --read-only             Read-only seat");
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

    // The seat opens a surface; the studio path is the only path.
    if surface_name.is_none() && surface_file.is_none() {
        surface_name = Some("surface:ryeos/studio/base".to_string());
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    rt.block_on(async {
        // Load surface
        let surface_opts = ryeos_client_base::surface::SurfaceLoadOptions {
            explicit_file: surface_file.map(std::path::PathBuf::from),
            surface_name: None,
        };

        // If --surface was given, resolve through daemon.
        // --surface always means daemon resolution, not local preview.
        let loaded: ryeos_client_base::surface::LoadedSurface = if surface_name.is_some() {
            match daemon::DaemonClient::try_connect().await {
                Ok(client) => {
                    let ref_str = surface_name.as_deref().unwrap();
                    eprintln!("info: resolving {} via daemon...", ref_str);
                    match client
                        .resolve_effective_surface(ref_str, Some(&project_path))
                        .await
                    {
                        Ok(mut value) => {
                            // Views-as-content: resolve every `view:` pane
                            // ref through the same effective-item machinery
                            // and embed the bindings — surfaces reference
                            // views, they never define them.
                            let mut view_refs: Vec<String> = value
                                .get("layout")
                                .and_then(|layout| layout.get("nodes"))
                                .and_then(|nodes| nodes.as_object())
                                .map(|nodes| {
                                    nodes
                                        .values()
                                        .filter_map(|node| node.get("view"))
                                        .filter_map(|view| view.as_str())
                                        .filter(|view| view.starts_with("view:"))
                                        .map(str::to_string)
                                        .collect()
                                })
                                .unwrap_or_default();
                            if let Some(home_ref) = value
                                .get("home_view")
                                .and_then(|v| v.as_str())
                                .filter(|v| v.starts_with("view:"))
                            {
                                view_refs.push(home_ref.to_string());
                            }
                            if let Some(library) = value.get("library").and_then(|v| v.as_array()) {
                                view_refs.extend(
                                    library
                                        .iter()
                                        .filter_map(|v| v.as_str())
                                        .filter(|v| v.starts_with("view:"))
                                        .map(str::to_string),
                                );
                            }
                            view_refs.dedup();
                            for view_ref in view_refs {
                                match client
                                    .resolve_effective_item(&view_ref, "view", Some(&project_path))
                                    .await
                                {
                                    Ok(binding) => {
                                        // Unwrap the effective-item
                                        // envelope to the composed value.
                                        let composed = binding
                                            .get("composed_value")
                                            .cloned()
                                            .unwrap_or(binding);
                                        value["views"][&view_ref] = composed;
                                    }
                                    Err(e) => {
                                        // Degrade: the pane renders the
                                        // missing-binding placeholder.
                                        eprintln!("warn: failed to resolve {view_ref}: {e}");
                                    }
                                }
                            }
                            match ryeos_client_base::surface::LoadedSurface::from_daemon(
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
                            }
                        }
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
                    eprintln!(
                        "error: failed to resolve surface '{}': daemon not available",
                        ref_str
                    );
                    eprintln!("hint: start ryeosd, or use --surface-file <path> for local preview");
                    std::process::exit(1);
                }
            }
        } else {
            ryeos_client_base::surface::load_surface(&surface_opts)
        };

        // Surface diagnostics
        for diag in loaded.all_diagnostics() {
            match diag {
                ryeos_client_base::surface::SurfaceDiagnostic::ValidationError { message } => {
                    eprintln!("error: {}", message);
                }
                ryeos_client_base::surface::SurfaceDiagnostic::UnsupportedField {
                    field,
                    message,
                } => {
                    eprintln!("warn: unsupported field '{}': {}", field, message);
                }
                ryeos_client_base::surface::SurfaceDiagnostic::Info { message } => {
                    eprintln!("info: {}", message);
                }
            }
        }

        let result = studio_app::run(&project_path, read_only, loaded).await;

        if let Err(e) = result {
            eprintln!("ryeos-tui error: {}", e);
            std::process::exit(1);
        }
    });
}
