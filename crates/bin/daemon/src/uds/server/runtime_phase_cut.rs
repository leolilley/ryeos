//! Feature-only exact crash-qualification gate for runtime-owned phases.
//!
//! Runtime phase names are opaque to the daemon. A qualifying parent selects
//! one bounded name and supplies one exact inherited Lillux channel. When the
//! authenticated runtime reports that name, the daemon writes one record and
//! parks the callback forever so the parent can SIGKILL the process at a proven
//! boundary. No production build serves this RPC.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const PHASE_ENV: &str = "RYEOSD_TEST_RUNTIME_PHASE_CUT";
pub(crate) const CHANNEL_FD_ENV: &str = "RYEOSD_TEST_RUNTIME_PHASE_CUT_FD";

static GATE: OnceLock<RuntimePhaseCutGate> = OnceLock::new();

struct RuntimePhaseCutGate {
    selected: String,
    reached: AtomicBool,
    channel: Mutex<lillux::InheritedDuplexChannel>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimePhaseCutParams {
    thread_id: String,
    phase: String,
}

fn valid_phase(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Adopt and immediately close-on-exec-protect the test parent's channel
/// before startup can launch any runtime subprocess.
pub(crate) fn initialize_from_env() -> Result<()> {
    let selected = std::env::var(PHASE_ENV).ok();
    let descriptor = std::env::var(CHANNEL_FD_ENV).ok();
    let (Some(selected), Some(_)) = (selected, descriptor) else {
        if std::env::var_os(PHASE_ENV).is_some() || std::env::var_os(CHANNEL_FD_ENV).is_some() {
            bail!("runtime phase-cut environment must supply phase and channel together");
        }
        return Ok(());
    };
    if !valid_phase(&selected) {
        bail!("runtime phase-cut selection is not bounded lower-snake ASCII");
    }
    if GATE.get().is_some() {
        bail!("runtime phase-cut gate was initialized more than once");
    }
    // SAFETY: the test parent created this exact connected channel through
    // Lillux, retained its peer, and granted the daemon unique ownership by
    // explicitly inheriting the descriptor named in CHANNEL_FD_ENV.
    let channel = unsafe { lillux::take_inherited_duplex_channel_from_env(CHANNEL_FD_ENV) }
        .map_err(anyhow::Error::msg)
        .context("adopt inherited runtime phase-cut channel")?;
    GATE.set(RuntimePhaseCutGate {
        selected,
        reached: AtomicBool::new(false),
        channel: Mutex::new(channel),
    })
    .map_err(|_| anyhow!("runtime phase-cut gate was initialized concurrently"))?;
    Ok(())
}

pub(crate) async fn reach(params: &Value) -> Result<Value> {
    let params: RuntimePhaseCutParams =
        serde_json::from_value(params.clone()).context("decode runtime test phase-cut request")?;
    if params.thread_id.is_empty() {
        bail!("runtime test phase-cut request has no thread_id");
    }
    if !valid_phase(&params.phase) {
        bail!("runtime test phase is not bounded lower-snake ASCII");
    }
    let gate = GATE
        .get()
        .ok_or_else(|| anyhow!("runtime phase-cut gate is not configured"))?;
    if params.phase != gate.selected {
        bail!(
            "runtime reported phase `{}` but qualification selected `{}`",
            params.phase,
            gate.selected
        );
    }
    if gate.reached.swap(true, Ordering::AcqRel) {
        bail!("runtime phase-cut gate was reached more than once");
    }
    {
        let mut channel = gate
            .channel
            .lock()
            .map_err(|_| anyhow!("runtime phase-cut channel lock poisoned"))?;
        channel
            .write_all(format!("{}\n", params.phase).as_bytes())
            .context("write runtime phase-cut evidence")?;
        channel
            .flush()
            .context("flush runtime phase-cut evidence")?;
    }
    std::future::pending::<()>().await;
    #[allow(unreachable_code)]
    Ok(json!({"reached": true}))
}
