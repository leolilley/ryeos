//! ryeos-tui — Native terminal TUI for Rye OS.
//!
//! A tiled workspace for AI agent operations: thread management,
//! execution, state inspection, remotes, and trust.

mod app;
mod render;
mod render_text;
mod terminal;
mod transport;

/// Collect every `view:`-prefixed ref anywhere in a surface value so each
/// can be embedded. Skips the `views` map itself (it holds already-resolved
/// bindings keyed by ref, not refs to resolve).
///
/// Only the `--surface-file` local-preview path uses this: that spec exists
/// solely client-side, so its views must still be fetched here. A
/// daemon-resolved surface arrives with views already embedded.
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
    let mut views_root: Option<String> = None;
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
            "--views-root" => {
                i += 1;
                if i < args.len() {
                    views_root = Some(args[i].clone());
                } else {
                    eprintln!("--views-root requires a directory argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("Usage: ryeos-tui [OPTIONS] [PROJECT_PATH]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --surface <REF>         Open a surface by canonical ref");
                eprintln!(
                    "  --surface-file <PATH>   Load surface spec from a local file (untrusted preview)"
                );
                eprintln!(
                    "  --views-root <DIR>      Resolve view: refs from local YAML under DIR first (with --surface-file)"
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

        // Non-fatal resolution diagnostics (a view that failed to resolve, an
        // unsupported field) are collected here and handed to the TUI, which
        // shows them as notices — stderr scrolls off above the alternate screen.
        let mut diagnostics: Vec<String> = Vec::new();

        // The connection that resolves the surface is handed on to the app —
        // discovery and audience settle once, and every later call reuses the
        // same kept-alive client.
        let mut daemon_client: Option<transport::daemon::DaemonClient> = None;

        // If --surface was given, resolve through daemon.
        // --surface always means daemon resolution, not local preview.
        let loaded: ryeos_client_base::surface::LoadedSurface = if surface_name.is_some() {
            match transport::daemon::DaemonClient::try_connect().await {
                Ok(client) => {
                    let ref_str = surface_name.as_deref().unwrap();
                    eprintln!("info: resolving {} via daemon...", ref_str);
                    let resolved = client
                        .resolve_effective_surface(ref_str, Some(&project_path))
                        .await;
                    daemon_client = Some(client);
                    match resolved {
                        Ok(value) => {
                            // Views arrive embedded by the daemon
                            // (`composed_value.views`, keyed by ref) — no
                            // per-view round-trips here. A view that failed
                            // to resolve server-side arrives as a degraded
                            // entry the pane renders as a placeholder
                            // carrying the reason; the daemon also reports
                            // each failure as a warn diagnostic, which
                            // `from_daemon` folds into the surface
                            // diagnostics surfaced as notices below.
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
            // `--surface-file`: the SURFACE is an untrusted local file. Its
            // views come from `--views-root` first (read straight off disk —
            // the content-iteration path: edit a view in the repo, relaunch,
            // see it, no republish), then from the trusted daemon for
            // whatever the root doesn't carry. Without a daemon, the layout
            // still renders; unresolved panes show the missing-binding
            // placeholder.
            let mut loaded = ryeos_client_base::surface::load_surface(&surface_opts);
            let spec_value = serde_json::to_value(loaded.spec()).unwrap_or(serde_json::Value::Null);
            let mut view_refs: Vec<String> = Vec::new();
            collect_view_refs(&spec_value, &mut view_refs);
            view_refs.sort();
            view_refs.dedup();
            let mut views = serde_json::Map::new();
            if let Some(root) = &views_root {
                // `view:<path>` → `<root>/<path>.yaml`. The signed header
                // line is a YAML comment, so files parse as-is. Misses fall
                // through to daemon resolution below.
                view_refs.retain(|view_ref| {
                    let rel = view_ref.strip_prefix("view:").unwrap_or(view_ref);
                    let path = std::path::Path::new(root).join(format!("{rel}.yaml"));
                    match std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| serde_yaml::from_str::<serde_json::Value>(&text).ok())
                    {
                        Some(value) => {
                            views.insert(view_ref.clone(), value);
                            false
                        }
                        None => true,
                    }
                });
            }
            match transport::daemon::DaemonClient::try_connect().await {
                Ok(client) => {
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
                            Err(e) => diagnostics.push(format!("view {view_ref} unavailable: {e}")),
                        }
                    }
                    daemon_client = Some(client);
                }
                Err(_) => {
                    eprintln!(
                        "warn: no daemon — local preview renders only views the views-root carries"
                    );
                }
            }
            loaded.set_views(serde_json::Value::Object(views));
            loaded
        };

        // Surface diagnostics → the same in-TUI notice channel (errors/warnings
        // that don't abort the load; pure info stays on stderr as it's benign).
        for diag in loaded.all_diagnostics() {
            match diag {
                ryeos_client_base::surface::SurfaceDiagnostic::ValidationError { message } => {
                    diagnostics.push(format!("surface: {message}"));
                }
                ryeos_client_base::surface::SurfaceDiagnostic::UnsupportedField {
                    field,
                    message,
                } => {
                    diagnostics.push(format!("unsupported field '{field}': {message}"));
                }
                ryeos_client_base::surface::SurfaceDiagnostic::Info { message } => {
                    eprintln!("info: {}", message);
                }
            }
        }

        let result = app::run(&project_path, read_only, loaded, diagnostics, daemon_client).await;

        if let Err(e) = result {
            eprintln!("ryeos-tui error: {}", e);
            std::process::exit(1);
        }
    });
}
