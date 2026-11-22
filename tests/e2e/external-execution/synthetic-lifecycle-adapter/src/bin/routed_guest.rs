//! Fixture executable retaining the original command name while the native
//! guest implementation belongs to the independent verifier product edge.

fn main() -> std::process::ExitCode {
    ryeos_codex_runtime_verifier::native_guest::run_probe_entrypoint()
}
