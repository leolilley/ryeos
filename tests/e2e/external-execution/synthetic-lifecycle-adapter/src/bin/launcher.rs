fn main() {
    if let Err(error) = ryeos_external_candidate_launcher::run_from_inherited() {
        eprintln!("ryeos-synthetic-external-candidate-launcher: {error:#}");
        std::process::exit(126);
    }
}
