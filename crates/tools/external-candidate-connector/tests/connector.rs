use std::io::Write as _;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_state::external_execution::connector::{
    ExternalConnectorClientFrame, ExternalConnectorServerFrame,
    read_external_connector_client_frame, read_external_connector_hello,
    write_external_connector_server_frame,
};

fn wait_promptly(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("connector did not terminate promptly");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn exact_process_relays_stdio_through_one_authenticated_connection() {
    let root = tempfile::tempdir().unwrap();
    let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&pinned, "connector").unwrap();
    let capability = STANDARD.encode([71_u8; 32]);
    let placement = "T-external-connector";
    let binding = "a".repeat(64);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ryeos-external-candidate-connector"))
        .env("RYEOS_EXTERNAL_CONNECTOR_ENDPOINT", listener.endpoint())
        .env("RYEOS_EXTERNAL_CONNECTOR_PLACEMENT", placement)
        .env("RYEOS_EXTERNAL_CONNECTOR_BINDING", &binding)
        .env("RYEOS_EXTERNAL_CONNECTOR_CAPABILITY", &capability)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stream = listener.accept().unwrap();
    let hello = read_external_connector_hello(&mut stream).unwrap();
    assert_eq!(hello.placement_thread_id, placement);
    assert_eq!(hello.execution_binding_hash, binding);
    assert_eq!(
        hello.capability_hash().unwrap(),
        lillux::sha256_hex(&[71_u8; 32])
    );
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::Ready {
            placement_thread_id: placement.into(),
            execution_binding_hash: binding.clone(),
        },
    )
    .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"request\n").unwrap();
    drop(stdin);
    let input = read_external_connector_client_frame(&mut stream).unwrap();
    assert_eq!(input.local_sequence(), 1);
    assert_eq!(input.protocol_bytes().unwrap().unwrap(), b"request\n");
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::InputApplied {
            local_sequence: 1,
            remote_sequence: 2,
            remote_frame_digest: "b".repeat(64),
        },
    )
    .unwrap();
    assert!(matches!(
        read_external_connector_client_frame(&mut stream).unwrap(),
        ExternalConnectorClientFrame::InputClosed { local_sequence: 2 }
    ));
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::ProtocolBytes {
            remote_sequence: 3,
            remote_frame_digest: "c".repeat(64),
            bytes_base64: STANDARD.encode(b"response\n"),
        },
    )
    .unwrap();
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::ProtocolEof {
            remote_sequence: 4,
            remote_frame_digest: "d".repeat(64),
        },
    )
    .unwrap();

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "connector stderr was not empty");
    assert_eq!(output.stdout, b"response\n");
    assert!(output.stderr.is_empty());
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn readiness_substitution_fails_without_disclosing_capability() {
    let root = tempfile::tempdir().unwrap();
    let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&pinned, "connector").unwrap();
    let capability = STANDARD.encode([72_u8; 32]);
    let child = Command::new(env!("CARGO_BIN_EXE_ryeos-external-candidate-connector"))
        .env("RYEOS_EXTERNAL_CONNECTOR_ENDPOINT", listener.endpoint())
        .env("RYEOS_EXTERNAL_CONNECTOR_PLACEMENT", "T-external-connector")
        .env("RYEOS_EXTERNAL_CONNECTOR_BINDING", "a".repeat(64))
        .env("RYEOS_EXTERNAL_CONNECTOR_CAPABILITY", &capability)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stream = listener.accept().unwrap();
    read_external_connector_hello(&mut stream).unwrap();
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::Ready {
            placement_thread_id: "T-external-connector".into(),
            execution_binding_hash: "b".repeat(64),
        },
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&capability));
    assert!(output.stdout.is_empty());
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn wrong_input_acknowledgement_is_fatal_while_controller_stays_connected() {
    let root = tempfile::tempdir().unwrap();
    let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&pinned, "connector").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ryeos-external-candidate-connector"))
        .env("RYEOS_EXTERNAL_CONNECTOR_ENDPOINT", listener.endpoint())
        .env("RYEOS_EXTERNAL_CONNECTOR_PLACEMENT", "T-external-connector")
        .env("RYEOS_EXTERNAL_CONNECTOR_BINDING", "a".repeat(64))
        .env(
            "RYEOS_EXTERNAL_CONNECTOR_CAPABILITY",
            STANDARD.encode([73_u8; 32]),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stream = listener.accept().unwrap();
    read_external_connector_hello(&mut stream).unwrap();
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::Ready {
            placement_thread_id: "T-external-connector".into(),
            execution_binding_hash: "a".repeat(64),
        },
    )
    .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"request\n")
        .unwrap();
    assert_eq!(
        read_external_connector_client_frame(&mut stream)
            .unwrap()
            .local_sequence(),
        1
    );
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::InputApplied {
            local_sequence: 2,
            remote_sequence: 2,
            remote_frame_digest: "b".repeat(64),
        },
    )
    .unwrap();

    assert!(!wait_promptly(&mut child).success());
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn output_eof_cannot_conceal_unacknowledged_input() {
    let root = tempfile::tempdir().unwrap();
    let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&pinned, "connector").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ryeos-external-candidate-connector"))
        .env("RYEOS_EXTERNAL_CONNECTOR_ENDPOINT", listener.endpoint())
        .env("RYEOS_EXTERNAL_CONNECTOR_PLACEMENT", "T-external-connector")
        .env("RYEOS_EXTERNAL_CONNECTOR_BINDING", "a".repeat(64))
        .env(
            "RYEOS_EXTERNAL_CONNECTOR_CAPABILITY",
            STANDARD.encode([74_u8; 32]),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stream = listener.accept().unwrap();
    read_external_connector_hello(&mut stream).unwrap();
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::Ready {
            placement_thread_id: "T-external-connector".into(),
            execution_binding_hash: "a".repeat(64),
        },
    )
    .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"request\n")
        .unwrap();
    read_external_connector_client_frame(&mut stream).unwrap();
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::ProtocolEof {
            remote_sequence: 2,
            remote_frame_digest: "b".repeat(64),
        },
    )
    .unwrap();

    assert!(!wait_promptly(&mut child).success());
}
