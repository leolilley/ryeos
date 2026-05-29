use std::path::{Path, PathBuf};

pub fn default_sealed_store_path(system_space_dir: &Path) -> PathBuf {
    system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("secrets")
        .join("store.enc")
}

pub fn default_vault_secret_key_path(system_space_dir: &Path) -> PathBuf {
    system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("private_key.pem")
}

pub fn default_vault_public_key_path(system_space_dir: &Path) -> PathBuf {
    system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("public_key.pem")
}
