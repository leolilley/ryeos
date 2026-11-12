//! A process-wide progress beacon for long bounded byte and entry loops.
//!
//! A process may install one plain function, once; Lillux and its callers
//! invoke it synchronously at natural progress points. It never spawns a
//! thread, so a single-threaded namespace owner stays single-threaded. With
//! no beacon installed a beat costs one atomic load.

use std::sync::OnceLock;

static BEACON: OnceLock<fn()> = OnceLock::new();

/// Install the process's beacon. Returns false when one is already installed.
pub fn install_beacon(beacon: fn()) -> bool {
    BEACON.set(beacon).is_ok()
}

/// One natural progress point. The beacon must be cheap and must not fail.
#[inline]
pub fn beat() {
    if let Some(beacon) = BEACON.get() {
        beacon();
    }
}
