pub use ryeos_client_transport::signing::{SignHeaders, Signer, operator_signing_key_path};

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::operator_signing_key_path;

    /// The transport owns the client-side operator key location; the engine
    /// owns the runtime-root layout. Both must name the same file.
    #[test]
    fn operator_key_path_matches_runtime_root_layout() {
        let app_root = Path::new("/srv/ryeos-app-root");
        assert_eq!(
            operator_signing_key_path(app_root),
            ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf())
                .operator_signing_key_path()
        );
    }
}
