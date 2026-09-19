use anyhow::{Context as _, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "ryeos node bundle-set update",
    about = "Install one exact remotely published bundle set",
    no_binary_name = true
)]
struct Args {
    /// Strict current-schema JSON request; URLs and implicit keys are unsupported.
    request: PathBuf,
    #[arg(long)]
    app_root: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let args = match Args::try_parse_from(argv) {
        Ok(args) => args,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            console.text(&error.to_string())?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let raw = lillux::read_regular_file_bounded_no_follow(&args.request, 64 * 1024)
        .with_context(|| format!("read exact update request {}", args.request.display()))?;
    let request =
        serde_yaml::from_slice(&raw).context("parse strict stopped bundle-set update request")?;
    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        app_root: args.app_root,
        ..Default::default()
    })?;
    let report =
        ryeos_api::remote::bundle_set_update::update_stopped_bundle_set(&config.app_root, request)
            .await?;
    let rendered = if args.json {
        serde_json::to_string(&report)?
    } else {
        serde_json::to_string_pretty(&report)?
    };
    console.text(&format!("{rendered}\n"))?;
    Ok(())
}
