use std::path::PathBuf;

use clap::Subcommand;

/// Top-level CLI commands.
#[derive(Debug, Subcommand)]
pub enum ClientCmd {
    /// Execute an item ref via the daemon.
    Execute {
        /// Item ref to execute, e.g. service:system/status
        item_ref: String,

        /// JSON parameters for the call
        #[arg(long)]
        params: Option<String>,

        /// Project root path
        #[arg(long)]
        project_path: Option<PathBuf>,
    },

    /// Show daemon status.
    Status,

    /// Verify item trust chain.
    Verify {
        /// Item ref to verify (omit to verify all).
        item_ref: Option<String>,

        /// Verify all known items.
        #[arg(long)]
        all: bool,
    },

    /// Rebuild the projection database.
    Rebuild {
        /// Also run reachability sweep.
        #[arg(long)]
        verify: bool,
    },

    /// Submit a command to the daemon.
    SubmitCommand {
        #[arg(long)]
        thread_id: String,

        #[arg(long)]
        command_type: String,

        #[arg(long)]
        params: Option<String>,
    },

    /// Build a bundle from a source directory.
    BuildBundle {
        #[arg(long)]
        source: Option<PathBuf>,

        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// Sign a file with a user key.
    UserKeySign {
        /// Input file to sign.
        input: PathBuf,

        /// Path to the signing key.
        #[arg(long)]
        key: Option<PathBuf>,
    },
}
