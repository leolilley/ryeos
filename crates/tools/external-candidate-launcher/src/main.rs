//! Dedicated native launcher. All authority arrives on fixed inherited
//! descriptors; this process performs no project, credential or host-path
//! discovery.

fn main() {
    if let Err(error) = ryeos_external_candidate_launcher::run_from_inherited() {
        eprintln!("ryeos-external-candidate-launcher: {error:#}");
        std::process::exit(126);
    }
}
