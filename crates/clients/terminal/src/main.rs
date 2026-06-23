//! ryeos-tui — Native terminal TUI for Rye OS.
//!
//! A tiled workspace for AI agent operations: thread management,
//! execution, state inspection, remotes, and trust.

mod app;
mod render;
mod render_text;
mod terminal;
mod transport;

/// Collect every `view:`-prefixed ref anywhere in the resolved surface
/// value so each can be embedded. Skips the `views` map itself (it holds
/// already-resolved bindings keyed by ref, not refs to resolve).
fn collect_view_refs(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) if s.starts_with("view:") => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_view_refs(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, v) in map {
                if key == "views" {
                    continue;
                }
                collect_view_refs(v, out);
            }
        }
        _ => {}
    }
}

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
                    "  --surface <REF>         Open a surface by canonical ref"
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

    // No hardcoded default surface. The surface is supplied by the caller
    // (`--surface` / `--surface-file`) or by the launching client's config.
    // With neither, show an empty surface — never fabricate one or crash.
    if surface_name.is_none() && surface_file.is_none() {
        eprintln!(
            "info: no surface specified (--surface / --surface-file, or client config); showing an empty surface"
        );
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
            match transport::daemon::DaemonClient::try_connect().await {
                Ok(client) => {
                    let ref_str = surface_name.as_deref().unwrap();
                    eprintln!("info: resolving {} via daemon...", ref_str);
                    match client
                        .resolve_effective_surface(ref_str, Some(&project_path))
                        .await
                    {
                        Ok(mut value) => {
                            // Views-as-content: resolve every `view:` ref
                            // appearing ANYWHERE in the surface — center
                            // `tiles`, edge `slots`, `backdrop`, `library` —
                            // through the same effective-item machinery and
                            // embed the bindings. Surfaces reference views;
                            // they never define them. Walking the whole value
                            // keeps this correct as the surface schema grows.
                            let mut view_refs: Vec<String> = Vec::new();
                            collect_view_refs(&value, &mut view_refs);
                            view_refs.sort();
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
                                        // Embed INTO the composed surface:
                                        // `from_daemon` parses the SurfaceSpec
                                        // from `composed_value`, so the views
                                        // map must live there, not as a
                                        // sibling of it.
                                        value["composed_value"]["views"][&view_ref] = composed;
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
            // `--surface-file`: the SURFACE is an untrusted local file, but its
            // views still come from the trusted daemon — resolve and embed them
            // so a layout previews with real content (no populate/install just
            // to look at it). Without a daemon, the layout still renders; its
            // panes show the missing-binding placeholder.
            let mut loaded = ryeos_client_base::surface::load_surface(&surface_opts);
            match transport::daemon::DaemonClient::try_connect().await {
                Ok(client) => {
                    let spec_value =
                        serde_json::to_value(loaded.spec()).unwrap_or(serde_json::Value::Null);
                    let mut view_refs: Vec<String> = Vec::new();
                    collect_view_refs(&spec_value, &mut view_refs);
                    view_refs.sort();
                    view_refs.dedup();
                    let mut views = serde_json::Map::new();
                    for view_ref in view_refs {
                        match client
                            .resolve_effective_item(&view_ref, "view", Some(&project_path))
                            .await
                        {
                            Ok(binding) => {
                                let composed =
                                    binding.get("composed_value").cloned().unwrap_or(binding);
                                views.insert(view_ref, composed);
                            }
                            Err(e) => eprintln!("warn: failed to resolve {view_ref}: {e}"),
                        }
                    }
                    loaded.set_views(serde_json::Value::Object(views));
                }
                Err(_) => {
                    eprintln!(
                        "warn: no daemon — local preview shows layout only (views unresolved)"
                    );
                }
            }
            loaded
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

        let result = app::run(&project_path, read_only, loaded).await;

        if let Err(e) = result {
            eprintln!("ryeos-tui error: {}", e);
            std::process::exit(1);
        }
    });
}
