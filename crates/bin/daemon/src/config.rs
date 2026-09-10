//! Daemon CLI entry surface.
//!
//! `Config` (the resolved data struct) lives in `ryeos-app::config`.
//! This module owns the clap-based `Cli` and converts it into a
//! `ConfigSources` for `Config::load`.
use std::ffi::OsString;
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

    /// Override the app root (default: XDG data dir / ryeos)
    #[arg(long)]
    pub app_root: Option<PathBuf>,

    #[arg(long)]
    pub bind: Option<SocketAddr>,

    #[arg(long)]
    pub db_path: Option<PathBuf>,

    #[arg(long)]
    pub uds_path: Option<PathBuf>,

    #[arg(long)]
    pub authorized_keys_dir: Option<PathBuf>,

    /// Resolve stored config conflicts in favor of explicit CLI values.
    #[arg(long)]
    pub force: bool,

    /// Test-only selection paired with a Lillux-owned inherited channel.
    #[cfg(feature = "handoff-test-support")]
    #[arg(long, hide = true)]
    pub handoff_phase_cut_boundary: Option<String>,
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
            authorized_keys_dir: self.authorized_keys_dir.clone(),
            force: self.force,
        }
    }
}

#[derive(Debug, clap::Subcommand)]
pub enum DaemonCommand {
    /// Root-only one-time native host setup. The caller supplies explicit host
    /// selections; no user-writable node policy is consulted here.
    #[command(hide = true)]
    HostProvision {
        #[arg(long)]
        app_root: PathBuf,
        #[arg(long)]
        controller_account_json: String,
    },
    /// Root-only installer transaction bridge. It owns the shared package
    /// namespace lock; it is neither node configuration nor worker authority.
    #[command(hide = true)]
    HostInstall {
        /// Exact administrator-owned package namespace to protect.
        #[arg(long)]
        package_root: PathBuf,
        #[command(subcommand)]
        action: HostInstallAction,
    },
    /// External installer bridge to shared host-upgrade authority.
    #[command(hide = true)]
    HostUpgrade {
        #[arg(long)]
        app_root: PathBuf,
        #[arg(long, required_unless_present = "inspect", conflicts_with = "inspect")]
        expected_daemon_sha256: Option<String>,
        #[arg(long, conflicts_with = "action")]
        inspect: bool,
        #[arg(value_enum, required_unless_present = "inspect")]
        action: Option<HostUpgradeAction>,
    },
    /// Administrator-installed service entry; never loads node config as root.
    #[command(hide = true)]
    HostService {
        /// Exact app root selected by the administrator-owned service.
        #[arg(long)]
        app_root: PathBuf,
    },
    /// Print build provenance and exit without loading daemon state.
    BuildInfo {
        /// Print only the baked git revision.
        #[arg(long)]
        revision: bool,

        /// Print only the compiled artifact profile.
        #[arg(long)]
        profile: bool,

        /// Print build provenance as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Run a service handler in standalone mode (daemon must be stopped).
    RunService {
        /// Canonical service ref, e.g. service:node/status
        service_ref: String,

        /// JSON parameters for the service call
        #[arg(long)]
        params: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum HostUpgradeAction {
    Begin,
    ReplacementSafe,
    RestoreReady,
    Finish,
    Observe,
}

#[derive(Debug, clap::Subcommand)]
pub enum HostInstallAction {
    /// Create/prove the package root, retain its exclusive lock and exec the
    /// already-authorized installer. The inherited descriptor is the proof;
    /// the environment only transports its coordinate.
    Acquire {
        #[arg(long)]
        installer: PathBuf,
        #[arg(long)]
        installer_digest: String,
        #[arg(long)]
        prepared: bool,
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Validate the inherited transaction lock after installer exec.
    Validate {
        #[arg(long)]
        transaction_fd: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_upgrade_requires_an_exact_action_and_digest_except_for_inspection() {
        let base = ["ryeosd", "host-upgrade", "--app-root", "/home/example/node"];
        assert!(Cli::try_parse_from(base).is_err());
        assert!(Cli::try_parse_from(base.into_iter().chain(["--inspect"])).is_ok());
        assert!(Cli::try_parse_from(base.into_iter().chain(["begin"])).is_err());
        let digest = "a".repeat(64);
        assert!(
            Cli::try_parse_from(base.into_iter().chain([
                "--expected-daemon-sha256",
                &digest,
                "begin",
            ]))
            .is_ok()
        );
        assert!(Cli::try_parse_from(base.into_iter().chain(["--inspect", "begin",])).is_err());
    }

    #[test]
    fn host_service_entry_requires_an_explicit_node_without_loading_its_config() {
        assert!(Cli::try_parse_from(["ryeosd", "host-service"]).is_err());
        let cli = Cli::try_parse_from([
            "ryeosd",
            "host-service",
            "--app-root",
            "/home/example/.local/share/ryeos",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Some(DaemonCommand::HostService { app_root })
            if app_root == PathBuf::from("/home/example/.local/share/ryeos"))
        );
    }

    #[test]
    fn host_install_requires_a_subcommand_and_preserves_the_original_argument_vector() {
        assert!(
            Cli::try_parse_from([
                "ryeosd",
                "host-install",
                "--package-root",
                "/usr/share/ryeos",
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from([
            "ryeosd",
            "host-install",
            "--package-root",
            "/usr/share/ryeos",
            "acquire",
            "--installer",
            "/checkout/scripts/pkg/install-local-direct.sh",
            "--installer-digest",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--prepared",
            "--",
            "--populate",
            "--all",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(DaemonCommand::HostInstall {
                package_root,
                action: HostInstallAction::Acquire { prepared: true, args, .. },
            }) if package_root == PathBuf::from("/usr/share/ryeos")
                && args == vec![OsString::from("--populate"), OsString::from("--all")]
        ));
    }
}
