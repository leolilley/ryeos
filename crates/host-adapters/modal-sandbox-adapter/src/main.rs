//! Controller-side Modal sandbox lease lifecycle adapter.

use std::io::Write as _;

fn main() {
    if ryeos_modal_sandbox_lifecycle_adapter::run().is_err() {
        // Never forward provider text, credentials, URLs, or settings to stderr.
        let _ = writeln!(std::io::stderr(), "modal lifecycle adapter failed closed");
        std::process::exit(1);
    }
}
