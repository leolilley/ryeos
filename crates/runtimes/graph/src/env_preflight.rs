pub fn check_env_requires(
    graph_requires: &[String],
    node_requires: &[String],
) -> Result<(), String> {
    let mut all = graph_requires.to_vec();
    all.extend(node_requires.iter().cloned());
    all.sort();
    all.dedup();

    let mut missing = Vec::new();
    for var in &all {
        if std::env::var(var).is_err() {
            missing.push(var.clone());
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("missing required env vars: {}", missing.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_passes_when_all_present() {
        std::env::set_var("_TEST_ENV_PREFLIGHT_X", "1");
        let result = check_env_requires(&["_TEST_ENV_PREFLIGHT_X".to_string()], &[]);
        std::env::remove_var("_TEST_ENV_PREFLIGHT_X");
        assert!(result.is_ok());
    }

    #[test]
    fn check_fails_when_missing() {
        let result = check_env_requires(&["_NONEXISTENT_VAR_XYZ".to_string()], &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("_NONEXISTENT_VAR_XYZ"));
    }

    #[test]
    fn check_merges_graph_and_node_requires() {
        std::env::set_var("_TEST_ENV_A", "1");
        let result = check_env_requires(
            &["_TEST_ENV_A".to_string()],
            &["_NONEXISTENT_NODE_VAR".to_string()],
        );
        std::env::remove_var("_TEST_ENV_A");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("_NONEXISTENT_NODE_VAR"));
    }
}
