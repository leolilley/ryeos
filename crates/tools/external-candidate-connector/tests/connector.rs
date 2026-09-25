use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_state::external_execution::connector::{
    ExternalConnectorClientFrame, ExternalConnectorServerFrame,
    read_external_connector_client_frame, read_external_connector_hello,
    write_external_connector_server_frame,
};

struct ConnectorFixture {
    process: lillux::SubordinateProcess,
    input: Option<lillux::SubordinateProcessInput>,
    output: lillux::SubordinateProcessOutput,
    error: lillux::SubordinateProcessError,
}

impl ConnectorFixture {
    fn start(endpoint: &std::path::Path, placement: &str, binding: &str, capability: &str) -> Self {
        let mut process = lillux::SubordinateProcess::spawn(lillux::SubordinateProcessRequest {
            cmd: env!("CARGO_BIN_EXE_ryeos-external-candidate-connector").to_owned(),
            argv0: None,
            args: Vec::new(),
            cwd: "/".to_owned(),
            envs: vec![
                (
                    "RYEOS_EXTERNAL_CONNECTOR_ENDPOINT".to_owned(),
                    endpoint.to_string_lossy().into_owned(),
                ),
                (
                    "RYEOS_EXTERNAL_CONNECTOR_PLACEMENT".to_owned(),
                    placement.to_owned(),
                ),
                (
                    "RYEOS_EXTERNAL_CONNECTOR_BINDING".to_owned(),
                    binding.to_owned(),
                ),
                (
                    "RYEOS_EXTERNAL_CONNECTOR_CAPABILITY".to_owned(),
                    capability.to_owned(),
                ),
            ],
            limits: None,
            inherited_fds: Vec::new(),
        })
        .unwrap();
        let input = process.take_input().unwrap();
        let output = process.take_output().unwrap();
        let error = process.take_error().unwrap();
        Self {
            process,
            input: Some(input),
            output,
            error,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.input
            .as_mut()
            .unwrap()
            .write_all_until(
                bytes,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
            )
            .unwrap();
    }

    fn close_input(&mut self) {
        drop(self.input.take());
    }

    fn finish(mut self) -> (lillux::SubordinateProcessExit, Vec<u8>, Vec<u8>) {
        // Deliberately retain the parent stdin writer until AFTER exact child
        // exit. Closing it here would conceal a stuck input relay.
        let status = wait_promptly(&mut self);
        let output = self.output.read_frame_until(0, 4096, deadline()).unwrap();
        let error = self.error.drain_until(8192, deadline()).unwrap();
        assert!(error.eof && !error.truncated);
        (status, output, error.bytes)
    }
}

impl Drop for ConnectorFixture {
    fn drop(&mut self) {
        // Assertion/peer failure also retains the exact owner through cleanup.
        // On an exceptional kill timeout SubordinateProcess's owning Drop is
        // the final reap backstop, never a detached diagnostic task.
        let _ = self.process.kill_exact_child_until(deadline());
    }
}

fn deadline() -> lillux::time::MonotonicDeadline {
    lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(3))
}

fn socket_root() -> tempfile::TempDir {
    // Respect the run's explicitly retained TMPDIR. An overlong endpoint must
    // fail at bind, not silently relocate into an unrelated temporary root.
    tempfile::Builder::new()
        .prefix("rxconn-")
        .tempdir()
        .unwrap()
}

fn with_ready_peer(test: impl FnOnce(ConnectorFixture, lillux::LocalDuplexStream)) {
    let root = socket_root();
    let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&pinned, "c").unwrap();
    let capability = STANDARD.encode([75_u8; 32]);
    let placement = "T-external-connector";
    let binding = "a".repeat(64);
    let child = ConnectorFixture::start(listener.endpoint(), placement, &binding, &capability);
    let mut stream = listener.accept_before(deadline()).unwrap().unwrap();
    let hello = read_external_connector_hello(&mut stream.with_deadline(deadline())).unwrap();
    assert_eq!(hello.placement_thread_id, placement);
    assert_eq!(hello.execution_binding_hash, binding);
    assert_eq!(
        hello.capability_hash().unwrap(),
        lillux::sha256_hex(&[75_u8; 32])
    );
    write_external_connector_server_frame(
        &mut stream.with_deadline(deadline()),
        &ExternalConnectorServerFrame::Ready {
            placement_thread_id: placement.into(),
            execution_binding_hash: binding,
        },
    )
    .unwrap();
    test(child, stream);
}

fn eof() -> ExternalConnectorServerFrame {
    ExternalConnectorServerFrame::ProtocolEof {
        remote_sequence: 4,
        remote_frame_digest: "d".repeat(64),
    }
}

fn observe_request(child: &mut ConnectorFixture, stream: &mut lillux::LocalDuplexStream) {
    child.write(b"request\n");
    let frame =
        read_external_connector_client_frame(&mut stream.with_deadline(deadline())).unwrap();
    assert_eq!(frame.local_sequence(), 1);
    assert_eq!(frame.protocol_bytes().unwrap().unwrap(), b"request\n");
}

fn wait_promptly(child: &mut ConnectorFixture) -> lillux::SubordinateProcessExit {
    if let Some(status) = child.process.wait_exact_child_until(deadline()).unwrap() {
        return status;
    }
    let cleanup = child.process.kill_exact_child_until(deadline());
    panic!("connector did not terminate promptly; exact-child cleanup: {cleanup:?}");
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn exact_process_relays_stdio_through_one_authenticated_connection() {
    let root = socket_root();
    let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&pinned, "c").unwrap();
    let capability = STANDARD.encode([71_u8; 32]);
    let placement = "T-external-connector";
    let binding = "a".repeat(64);
    let mut child = ConnectorFixture::start(listener.endpoint(), placement, &binding, &capability);
    let mut raw_stream = listener.accept_before(deadline()).unwrap().unwrap();
    let mut stream = raw_stream.with_deadline(deadline());
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

    child.write(b"request\n");
    child.close_input();
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

    let (status, output, error) = child.finish();
    assert!(status.success, "connector stderr was not empty");
    assert_eq!(output, b"response\n");
    assert!(error.is_empty());
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn readiness_substitution_fails_without_disclosing_capability() {
    let root = socket_root();
    let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&pinned, "c").unwrap();
    let capability = STANDARD.encode([72_u8; 32]);
    let mut child = ConnectorFixture::start(
        listener.endpoint(),
        "T-external-connector",
        &"a".repeat(64),
        &capability,
    );
    child.close_input();
    let mut raw_stream = listener.accept_before(deadline()).unwrap().unwrap();
    let mut stream = raw_stream.with_deadline(deadline());
    read_external_connector_hello(&mut stream).unwrap();
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::Ready {
            placement_thread_id: "T-external-connector".into(),
            execution_binding_hash: "b".repeat(64),
        },
    )
    .unwrap();
    let (status, output, error) = child.finish();
    assert!(!status.success);
    assert!(!String::from_utf8_lossy(&error).contains(&capability));
    assert!(output.is_empty());
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn wrong_input_acknowledgement_is_fatal_while_controller_stays_connected() {
    let root = socket_root();
    let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&pinned, "c").unwrap();
    let mut child = ConnectorFixture::start(
        listener.endpoint(),
        "T-external-connector",
        &"a".repeat(64),
        &STANDARD.encode([73_u8; 32]),
    );
    let mut raw_stream = listener.accept_before(deadline()).unwrap().unwrap();
    let mut stream = raw_stream.with_deadline(deadline());
    read_external_connector_hello(&mut stream).unwrap();
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::Ready {
            placement_thread_id: "T-external-connector".into(),
            execution_binding_hash: "a".repeat(64),
        },
    )
    .unwrap();
    child.write(b"request\n");
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

    assert!(!wait_promptly(&mut child).success);
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn output_eof_cannot_conceal_unacknowledged_input() {
    let root = socket_root();
    let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&pinned, "c").unwrap();
    let mut child = ConnectorFixture::start(
        listener.endpoint(),
        "T-external-connector",
        &"a".repeat(64),
        &STANDARD.encode([74_u8; 32]),
    );
    let mut raw_stream = listener.accept_before(deadline()).unwrap().unwrap();
    let mut stream = raw_stream.with_deadline(deadline());
    read_external_connector_hello(&mut stream).unwrap();
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::Ready {
            placement_thread_id: "T-external-connector".into(),
            execution_binding_hash: "a".repeat(64),
        },
    )
    .unwrap();
    child.write(b"request\n");
    read_external_connector_client_frame(&mut stream).unwrap();
    write_external_connector_server_frame(
        &mut stream,
        &ExternalConnectorServerFrame::ProtocolEof {
            remote_sequence: 2,
            remote_frame_digest: "b".repeat(64),
        },
    )
    .unwrap();

    let (status, output, error) = child.finish();
    assert!(!status.success);
    assert!(output.is_empty());
    assert!(String::from_utf8_lossy(&error).contains("uncertain input delivery"));
}

// These observe real process termination with the parent stdin writer retained.
// They do not alone distinguish joined cleanup from process-exit destruction of
// a detached relay: the source-level ownership/no-detach invariant is additional.
#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn controller_disconnect_interrupts_idle_input_with_parent_writer_open() {
    with_ready_peer(|child, stream| {
        stream.shutdown().unwrap();
        let (status, output, error) = child.finish();
        assert!(!status.success);
        assert!(output.is_empty());
        assert!(!error.is_empty());
    });
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn authenticated_eof_interrupts_idle_input_without_spurious_failure() {
    with_ready_peer(|child, mut stream| {
        write_external_connector_server_frame(&mut stream.with_deadline(deadline()), &eof())
            .unwrap();
        let (status, output, error) = child.finish();
        assert!(status.success, "{}", String::from_utf8_lossy(&error));
        assert!(output.is_empty());
        assert!(error.is_empty());
    });
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn controller_fault_releases_outstanding_input_acknowledgement_wait() {
    with_ready_peer(|mut child, mut stream| {
        observe_request(&mut child, &mut stream);
        write_external_connector_server_frame(
            &mut stream.with_deadline(deadline()),
            &ExternalConnectorServerFrame::Fault {
                code: ryeos_state::external_execution::connector::ExternalConnectorFault::ExecutionRevoked,
            },
        ).unwrap();
        let (status, output, error) = child.finish();
        assert!(!status.success);
        assert!(output.is_empty());
        assert!(String::from_utf8_lossy(&error).contains("controller reported a closed fault"));
    });
}

#[test]
#[ignore = "native Unix-socket connector qualification; exercises the real 30-second ACK deadline"]
fn connected_controller_withholding_acknowledgement_expires_delivery() {
    with_ready_peer(|mut child, mut stream| {
        let started = lillux::time::MonotonicTimer::start();
        let expiry = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(35));
        observe_request(&mut child, &mut stream);
        // Keep both the controller connection and parent stdin writer alive.
        // No Fault, EOF, acknowledgement, or harness shutdown can release the
        // delivery wait: the connector's own absolute budget must expire.
        let status = match child.process.wait_exact_child_until(expiry).unwrap() {
            Some(status) => status,
            None => {
                let cleanup = child.process.kill_exact_child_until(deadline());
                panic!("connected ACK wait exceeded 35 seconds; exact-child cleanup: {cleanup:?}");
            }
        };
        assert!(!status.success);
        assert!(
            started.elapsed() >= lillux::time::Duration::from_secs(29),
            "connector refused before exercising its real delivery deadline"
        );
        let error = child.error.drain_until(8192, deadline()).unwrap();
        assert!(error.eof && !error.truncated && !error.bytes.is_empty());
        assert!(
            child
                .output
                .read_frame_until(0, 4096, deadline())
                .unwrap()
                .is_empty()
        );
        // Explicitly keep the peer owned until after exit and diagnostics.
        drop(stream);
    });
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn acknowledged_input_then_output_eof_succeeds_with_parent_writer_open() {
    with_ready_peer(|mut child, mut stream| {
        observe_request(&mut child, &mut stream);
        write_external_connector_server_frame(
            &mut stream.with_deadline(deadline()),
            &ExternalConnectorServerFrame::InputApplied {
                local_sequence: 1,
                remote_sequence: 2,
                remote_frame_digest: "b".repeat(64),
            },
        )
        .unwrap();
        write_external_connector_server_frame(
            &mut stream.with_deadline(deadline()),
            &ExternalConnectorServerFrame::ProtocolBytes {
                remote_sequence: 3,
                remote_frame_digest: "c".repeat(64),
                bytes_base64: STANDARD.encode(b"response\n"),
            },
        )
        .unwrap();
        write_external_connector_server_frame(&mut stream.with_deadline(deadline()), &eof())
            .unwrap();
        let (status, output, error) = child.finish();
        assert!(status.success, "{}", String::from_utf8_lossy(&error));
        assert_eq!(output, b"response\n");
        assert!(error.is_empty());
    });
}

#[test]
#[ignore = "native Unix-socket connector qualification; tool sandboxes may deny bind"]
fn input_socket_failure_interrupts_backpressured_stdout() {
    with_ready_peer(|mut child, mut stream| {
        // Observe actual output first, without a background reader that would
        // prevent stdout backpressure. No pipe-capacity assumption or sleep.
        write_external_connector_server_frame(
            &mut stream.with_deadline(deadline()),
            &ExternalConnectorServerFrame::ProtocolBytes {
                remote_sequence: 1,
                remote_frame_digest: "b".repeat(64),
                bytes_base64: STANDARD.encode(b"x"),
            },
        )
        .unwrap();
        assert_eq!(
            child.output.read_frame_until(b'x', 1, deadline()).unwrap(),
            b"x"
        );

        // One absolute budget and at most 16 MiB: a timed-out peer write
        // establishes backpressure, rather than guessing when the child is
        // blocked. The partially written frame is never retried.
        let fill_deadline = deadline();
        let payload = STANDARD.encode(vec![b'x'; ryeos_state::external_execution::MAX_CHUNK_BYTES]);
        let mut backpressured = false;
        for sequence in 2..66 {
            let result = write_external_connector_server_frame(
                &mut stream.with_deadline(fill_deadline),
                &ExternalConnectorServerFrame::ProtocolBytes {
                    remote_sequence: sequence,
                    remote_frame_digest: "c".repeat(64),
                    bytes_base64: payload.clone(),
                },
            );
            if let Err(error) = result {
                assert!(
                    sequence > 2,
                    "no complete large output frame reached the connector"
                );
                assert!(
                    error.chain().any(|cause| cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)),
                    "expected bounded backpressure, got {error:#}"
                );
                backpressured = true;
                break;
            }
        }
        assert!(
            backpressured,
            "finite output budget did not establish backpressure"
        );
        assert!(child.process.try_exit().unwrap().is_none());
        stream.shutdown().unwrap();
        // Main is blocked on stdout; new stdin makes the input-side socket
        // write fail and must interrupt that output operation promptly.
        child.write(b"request\n");
        assert!(!wait_promptly(&mut child).success);
        let error = child.error.drain_until(8192, deadline()).unwrap();
        assert!(error.eof && !error.truncated && !error.bytes.is_empty());
        // Deliberately do not drain/retry the uncertain partial output.
    });
}
