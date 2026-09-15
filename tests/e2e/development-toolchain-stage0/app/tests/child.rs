#[test]
fn executable_child() {
    assert!(
        std::process::Command::new(env!("CARGO_BIN_EXE_linkage-probe"))
            .status()
            .unwrap()
            .success()
    );
}
