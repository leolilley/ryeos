//! Core lifecycle commands remain available without provider setup implementation.

#[test]
fn provider_command_selection_preserves_core_lifecycle() {
    let commands = ryeos_cli::lifecycle_commands::local_command_descriptors();
    assert_eq!(
        commands.iter().any(|command| command.tokens == ["setup"]),
        cfg!(feature = "model-provider-setup")
    );
    for tokens in [
        &["init"][..],
        &["identity"][..],
        &["node", "doctor"][..],
        &["node", "status"][..],
    ] {
        assert!(
            commands.iter().any(|command| command.tokens == tokens),
            "missing Core command {tokens:?}"
        );
    }
}
