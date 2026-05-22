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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut mock = false;
    let mut project_path = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".into());

    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "--mock" => mock = true,
            "--help" | "-h" => {
                eprintln!("Usage: ryeos-tui [OPTIONS] [PROJECT_PATH]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --mock    Use mock data (no daemon required)");
                eprintln!("  --help    Show this help");
                std::process::exit(0);
            }
            p if !p.starts_with('-') => {
                project_path = p.to_string();
            }
            _ => {
                eprintln!("Unknown option: {}", arg);
                std::process::exit(1);
            }
        }
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    rt.block_on(async {
        if let Err(e) = app::run(&project_path, mock).await {
            eprintln!("ryeos-tui error: {}", e);
            std::process::exit(1);
        }
    });
}
