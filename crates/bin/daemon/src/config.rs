//! Daemon CLI entry surface.
//!
//! `Config` (the resolved data struct) lives in `ryeos-app::config`.
//! This module owns the clap-based `Cli` and converts it into a
//! `ConfigSources` for `Config::load`.
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

pub use ryeos_app::config::{Config, ConfigSources};

#[derive(Debug, Parser)]
#[command(name = "ryeosd", about = "Rust control plane daemon for Rye OS")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<DaemonCommand>,

    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Override the app rootectory (default: XDG data dir / ryeos)
    #[arg(long)]
    pub app_root: Option<PathBuf>,

    #[arg(long)]
    pub bind: Option<SocketAddr>,

    #[arg(long)]
    pub db_path: Option<PathBuf>,

    #[arg(long)]
    pub uds_path: Option<PathBuf>,

    #[arg(long)]
    pub require_auth: bool,

    #[arg(long)]
    pub authorized_keys_dir: Option<PathBuf>,

    /// Resolve stored config conflicts in favor of explicit CLI values.
    #[arg(long)]
    pub force: bool,
}

impl Cli {
    /// Convert the parsed CLI into the plain-data `ConfigSources`
    /// consumed by `ryeos_app::config::Config::load`.
    pub fn to_sources(&self) -> ConfigSources {
        ConfigSources {
            config_file: self.config.clone(),
            app_root: self.app_root.clone(),
            bind: self.bind,
            db_path: self.db_path.clone(),
            uds_path: self.uds_path.clone(),
            require_auth: self.require_auth,
            authorized_keys_dir: self.authorized_keys_dir.clone(),
            force: self.force,
        }
    }
}

#[derive(Debug, clap::Subcommand)]
pub enum DaemonCommand {
    /// Run a service handler in standalone mode (daemon must be stopped).
    RunService {
        /// Canonical service ref, e.g. service:system/status
        service_ref: String,

        /// JSON parameters for the service call
        #[arg(long)]
        params: Option<String>,
    },
}
