use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Subcommand;

pub fn iso8601_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let seconds = day_secs % 60;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Current wall-clock time as milliseconds since Unix epoch.
pub fn timestamp_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[derive(Subcommand)]
pub enum TimeAction {
    /// Current wall-clock time
    Now,
    /// Sleep for N milliseconds
    After {
        #[arg(long)]
        ms: u64,
    },
}

pub fn run(action: TimeAction) -> serde_json::Value {
    match action {
        TimeAction::Now => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            serde_json::json!({
                "timestamp_ns": now.as_nanos() as u64,
                "timestamp_ms": now.as_millis() as u64,
            })
        }
        TimeAction::After { ms } => {
            let start = Instant::now();
            thread::sleep(Duration::from_millis(ms));
            let elapsed = start.elapsed().as_millis() as u64;
            serde_json::json!({ "elapsed_ms": elapsed })
        }
    }
}
