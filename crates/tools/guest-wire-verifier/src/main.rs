//! Entry: `--remote-probe-controller` (origin) or `--consumer-challenge-b64 <b64>` (guest).

use std::ffi::OsStr;
use std::io::{Read as _, Write as _};

use anyhow::{Context as _, Result, ensure};

fn origin() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin()
        .take((ryeos_guest_wire_verifier::origin::INPUT_LIMIT + 1) as u64)
        .read_to_end(&mut input)?;
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(ryeos_guest_wire_verifier::origin::run(&input))?;
    let encoded = lillux::canonical_json(&serde_json::to_value(&result)?)?;
    std::io::stdout()
        .lock()
        .write_all(format!("{encoded}\n").as_bytes())?;
    Ok(())
}

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().context("a role argument is required")?;
    if mode == OsStr::new("--remote-probe-controller") {
        ensure!(args.next().is_none(), "unexpected origin arguments");
        return origin();
    }
    if mode == OsStr::new("--consumer-challenge-b64") {
        let encoded = args.next().context("consumer challenge is absent")?;
        ensure!(args.next().is_none(), "unexpected guest arguments");
        return ryeos_guest_wire_verifier::guest::run(
            encoded
                .to_str()
                .context("consumer challenge is not UTF-8")?,
        );
    }
    anyhow::bail!("unsupported role")
}

fn main() {
    if let Err(error) = run() {
        eprintln!("guest wire verifier refused: {error:#}");
        std::process::exit(1);
    }
}
