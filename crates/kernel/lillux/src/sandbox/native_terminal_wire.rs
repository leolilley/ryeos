//! Closed private native testimony codec. Decode does not mint evidence.
//! Writer/alias/early-scrub, same-pin death, admitted-context and existing-wait
//! joins are separate required source cuts; no launch role/caller here.
use super::*;
use serde::{Deserialize, Serialize};

pub(crate) const NATIVE_TERMINAL_SCHEMA: &str = "lillux.native-terminal/v1";
pub(crate) const MAX_NATIVE_TERMINAL_FRAME_BYTES: usize = 4096;
#[cfg(target_os = "linux")]
const _: () = assert!(MAX_NATIVE_TERMINAL_FRAME_BYTES <= libc::PIPE_BUF);

/// Untrusted testimony only, never the opaque native producer observation.
/// Private to Lillux; future receiver must join independently retained context.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct NativeTerminalTestimony {
    pub(crate) schema: String,
    pub(crate) process_identity: crate::ExactProcessIdentity,
    pub(crate) held_mount: LinuxSandboxMountPreparationReceipt,
    #[serde(deserialize_with = "required_nullable")]
    pub(crate) applied_launch: Option<LinuxSandboxAppliedLaunchReceipt>,
    pub(crate) exit: NativeTerminalExit,
    pub(crate) owner_requested_termination: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub(crate) launch_failure: Option<NativeTerminalLaunchFailure>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub(crate) enum NativeTerminalExit {
    Code(i32),
    Signal(i32),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTerminalLaunchFailure {
    pub(crate) diagnostic: String,
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub(super) fn encode_observation(
    observation: &LinuxSandboxNativeTerminalObservation,
) -> Result<Vec<u8>, String> {
    let record = NativeTerminalTestimony {
        schema: NATIVE_TERMINAL_SCHEMA.to_owned(),
        process_identity: observation.process_identity().clone(),
        held_mount: observation.held_mount_preparation().clone(),
        applied_launch: observation.applied_launch().cloned(),
        exit: match observation.exit() {
            LinuxSandboxExit::Code(code) => NativeTerminalExit::Code(code),
            LinuxSandboxExit::Signal(signal) => NativeTerminalExit::Signal(signal),
        },
        owner_requested_termination: observation.owner_requested_termination(),
        launch_failure: observation
            .launch_failure()
            .map(|failure| NativeTerminalLaunchFailure {
                diagnostic: failure.diagnostic().to_owned(),
            }),
    };
    encode_testimony(&record)
}

fn encode_testimony(record: &NativeTerminalTestimony) -> Result<Vec<u8>, String> {
    validate_testimony(record)?;
    let mut bytes = serde_json::to_vec(record)
        .map_err(|error| format!("encode private native terminal testimony: {error}"))?;
    let framed_length = bytes
        .len()
        .checked_add(1)
        .ok_or("native terminal framing length overflow")?;
    if framed_length > MAX_NATIVE_TERMINAL_FRAME_BYTES {
        return Err("complete native terminal testimony exceeds atomic frame cap".into());
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Return data only. Exact EOF, protected producer, retained target death and
/// admitted identity/mount/applied joins are mandatory in the same owner.
pub(crate) fn decode_testimony(frame: &[u8]) -> Result<NativeTerminalTestimony, String> {
    if frame.is_empty()
        || frame.len() > MAX_NATIVE_TERMINAL_FRAME_BYTES
        || frame.last() != Some(&b'\n')
    {
        return Err("native terminal frame is incomplete or exceeds atomic cap".into());
    }
    let record: NativeTerminalTestimony = serde_json::from_slice(&frame[..frame.len() - 1])
        .map_err(|error| format!("decode private native terminal testimony: {error}"))?;
    validate_testimony(&record)?;
    // Struct and nested struct derives reject duplicate/unknown keys. The
    // closed canonical re-encoding also rejects alternate whitespace/order,
    // extra documents, surplus newlines and noncanonical numeric/escape bytes.
    if encode_testimony(&record)?.as_slice() != frame {
        return Err("native terminal testimony is not closed canonical framing".into());
    }
    Ok(record)
}

fn validate_testimony(record: &NativeTerminalTestimony) -> Result<(), String> {
    if record.schema != NATIVE_TERMINAL_SCHEMA {
        return Err("unsupported private native terminal schema".into());
    }
    let identity = &record.process_identity;
    identity.validate()?;
    let boot = identity.boot_id.as_bytes();
    if boot.len() != 36
        || identity.target_pid <= 1
        || identity.group_leader_pid <= 1
        || boot.iter().enumerate().any(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                *byte != b'-'
            } else {
                !byte.is_ascii_digit() && !(b'a'..=b'f').contains(byte)
            }
        })
    {
        return Err("native terminal incarnation data is not bounded canonical identity".into());
    }
    if record.held_mount.schema != 1
        || record.held_mount.owned_child_pid != identity.target_pid
        || record.held_mount.mount_count > 4096
    {
        return Err("native terminal held mount differs from its declared child".into());
    }
    if let Some(applied) = &record.applied_launch {
        if applied.owned_child_pid != identity.target_pid
            || applied.namespace_pid != 1
            || !applied.no_new_privs
            || applied.seccomp_mode != 2
            || applied.post_release_mount_view.schema != record.held_mount.schema
            || applied.post_release_mount_view.mount_count != record.held_mount.mount_count
            || applied.post_release_mount_view.destination_access_sha256
                != record.held_mount.destination_access_sha256
        {
            return Err(
                "native terminal applied data differs from its declared held context".into(),
            );
        }
    }
    match &record.exit {
        NativeTerminalExit::Code(code) if (0..=255).contains(code) => {}
        NativeTerminalExit::Signal(signal) if (1..=64).contains(signal) => {}
        _ => return Err("native terminal exit lacks supported Linux status data".into()),
    }
    if record
        .launch_failure
        .as_ref()
        .is_some_and(|failure| failure.diagnostic.len() > MAX_NATIVE_TERMINAL_FRAME_BYTES - 5)
    {
        return Err("native terminal pre-exec diagnostic exceeds native failure bound".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure hostile-byte codec data, NOT native evidence or an opaque producer
    // fixture. No physical/native process/driver claim is constructed here.
    fn raw_testimony() -> NativeTerminalTestimony {
        serde_json::from_value(serde_json::json!({
            "schema": NATIVE_TERMINAL_SCHEMA,
            "process-identity": {
                "boot_id": "00000000-0000-0000-0000-000000000001",
                "target_pid": 200, "target_start_time_ticks": 1,
                "group_leader_pid": 100, "group_leader_start_time_ticks": 1
            },
            "held-mount": {
                "schema": 1, "owned_child_pid": 200, "mount_count": 0,
                "destination_access_sha256": vec![0_u8; 32]
            },
            "applied-launch": null, "exit": {"kind": "signal", "value": 9},
            "owner-requested-termination": true, "launch-failure": null
        }))
        .unwrap()
    }

    #[test]
    fn closed_byte_codec_is_testimony_only_and_requires_exact_framing() {
        let record = raw_testimony();
        let frame = encode_testimony(&record).unwrap();
        let decoded = decode_testimony(&frame).unwrap();
        assert!(decoded.applied_launch.is_none());
        assert!(decoded.launch_failure.is_none());
        assert!(frame.len() <= MAX_NATIVE_TERMINAL_FRAME_BYTES);
        assert!(decode_testimony(&frame[..frame.len() - 1]).is_err());
        let mut duplicate = frame.clone();
        duplicate.extend_from_slice(&frame);
        assert!(decode_testimony(&duplicate).is_err());
        let mut surplus = frame.clone();
        surplus.push(b'\n');
        assert!(decode_testimony(&surplus).is_err());
    }

    #[test]
    fn nullable_fields_and_closed_keys_are_mandatory() {
        let frame = encode_testimony(&raw_testimony()).unwrap();
        for key in ["applied-launch", "launch-failure"] {
            let mut value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
            value.as_object_mut().unwrap().remove(key);
            let mut missing = serde_json::to_vec(&value).unwrap();
            missing.push(b'\n');
            assert!(decode_testimony(&missing).is_err());
        }
        let mut value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        value["not-native-proof"] = serde_json::json!(true);
        let mut unknown = serde_json::to_vec(&value).unwrap();
        unknown.push(b'\n');
        assert!(decode_testimony(&unknown).is_err());
        let text = String::from_utf8(frame).unwrap();
        let duplicate = text.replacen("{", "{\"schema\":\"lillux.native-terminal/v1\",", 1);
        assert!(decode_testimony(duplicate.as_bytes()).is_err());
    }

    #[test]
    fn canonical_zero_birth_ticks_refuse_as_incomplete_incarnation() {
        for target_zero in [true, false] {
            let mut record = raw_testimony();
            if target_zero {
                record.process_identity.target_start_time_ticks = 0;
            } else {
                record.process_identity.group_leader_start_time_ticks = 0;
            }
            // Serialize the declared struct directly to preserve the exact
            // canonical field order. This hostile byte fixture must fail the
            // actual identity predicate, not alternate JSON ordering.
            let mut frame = serde_json::to_vec(&record).unwrap();
            assert!(serde_json::from_slice::<NativeTerminalTestimony>(&frame).is_ok());
            frame.push(b'\n');
            let error = decode_testimony(&frame).unwrap_err();
            assert!(error.contains("exact process identity is incomplete"));
        }
    }

    #[test]
    fn negative_preexec_data_does_not_require_successful_applied_receipt() {
        let mut record = raw_testimony();
        record.launch_failure = Some(NativeTerminalLaunchFailure {
            diagnostic: "pre-exec refused".into(),
        });
        record.exit = NativeTerminalExit::Code(125);
        let decoded = decode_testimony(&encode_testimony(&record).unwrap()).unwrap();
        assert!(decoded.applied_launch.is_none());
        assert!(decoded.launch_failure.is_some());
        record.exit = NativeTerminalExit::Signal(0);
        assert!(encode_testimony(&record).is_err());
        record.exit = NativeTerminalExit::Code(256);
        assert!(encode_testimony(&record).is_err());
    }

    #[test]
    fn complete_framing_cap_refuses_oversize_without_truncating_data() {
        let mut record = raw_testimony();
        let base = encode_testimony(&record).unwrap().len();
        let empty_failure = serde_json::to_vec(&NativeTerminalLaunchFailure {
            diagnostic: String::new(),
        })
        .unwrap()
        .len();
        // Replacing null with a failure object changes that field by this exact
        // amount. ASCII payload bytes each encode as one byte; framing included.
        let length = MAX_NATIVE_TERMINAL_FRAME_BYTES - base - (empty_failure - 4);
        record.launch_failure = Some(NativeTerminalLaunchFailure {
            diagnostic: "x".repeat(length),
        });
        let at_cap = encode_testimony(&record).unwrap();
        assert_eq!(at_cap.len(), MAX_NATIVE_TERMINAL_FRAME_BYTES);
        assert_eq!(
            decode_testimony(&at_cap)
                .unwrap()
                .launch_failure
                .unwrap()
                .diagnostic
                .len(),
            length
        );
        let retained = record.launch_failure.as_mut().unwrap();
        retained.diagnostic.push('x');
        assert!(encode_testimony(&record).is_err());
        assert_eq!(
            record.launch_failure.as_ref().unwrap().diagnostic.len(),
            length + 1
        );
    }
}
