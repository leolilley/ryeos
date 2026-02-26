use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::Parser;
use notify::{EventKind, RecursiveMode, Watcher};
use rusqlite::Connection;

#[derive(Parser)]
#[command(name = "lillux-watch", about = "Watch thread registry for status changes")]
struct Args {
    /// Path to registry.db
    #[arg(long)]
    db: PathBuf,

    /// Thread ID to watch
    #[arg(long)]
    thread_id: String,

    /// Timeout in seconds
    #[arg(long, default_value_t = 300.0)]
    timeout: f64,
}

const TERMINAL: &[&str] = &["completed", "error", "cancelled", "continued"];

fn query_status(db: &PathBuf, thread_id: &str) -> Option<String> {
    let conn = Connection::open(db).ok()?;
    let mut stmt = conn
        .prepare("SELECT status FROM threads WHERE thread_id = ?1")
        .ok()?;
    stmt.query_row([thread_id], |row| row.get::<_, String>(0))
        .ok()
}

fn emit(status: &str, thread_id: &str) {
    let obj = serde_json::json!({
        "status": status,
        "thread_id": thread_id,
    });
    println!("{}", obj);
}

fn main() {
    let args = Args::parse();
    let deadline = Instant::now() + Duration::from_secs_f64(args.timeout);

    // Immediate check
    if let Some(status) = query_status(&args.db, &args.thread_id) {
        if TERMINAL.contains(&status.as_str()) {
            emit(&status, &args.thread_id);
            return;
        }
    }

    // Set up file watcher
    let (tx, rx) = mpsc::channel();

    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            ) {
                let _ = tx.send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(_) => {
            // Watcher init failed — fall back to reporting timeout
            emit("timeout", &args.thread_id);
            return;
        }
    };

    // Watch the directory containing registry.db (more reliable than watching the file directly
    // since SQLite uses temp files + rename for writes)
    let watch_dir = args.db.parent().unwrap_or(&args.db);
    if watcher
        .watch(watch_dir.as_ref(), RecursiveMode::NonRecursive)
        .is_err()
    {
        emit("timeout", &args.thread_id);
        return;
    }

    // Poll loop driven by file-change events
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            emit("timeout", &args.thread_id);
            return;
        }

        // Wait for a change event or timeout
        match rx.recv_timeout(remaining) {
            Ok(()) => {
                // Drain any queued events to coalesce rapid writes
                while rx.try_recv().is_ok() {}

                if let Some(status) = query_status(&args.db, &args.thread_id) {
                    if TERMINAL.contains(&status.as_str()) {
                        emit(&status, &args.thread_id);
                        return;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                emit("timeout", &args.thread_id);
                return;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Watcher dropped — do one last check then exit
                if let Some(status) = query_status(&args.db, &args.thread_id) {
                    if TERMINAL.contains(&status.as_str()) {
                        emit(&status, &args.thread_id);
                        return;
                    }
                }
                emit("timeout", &args.thread_id);
                return;
            }
        }
    }
}
