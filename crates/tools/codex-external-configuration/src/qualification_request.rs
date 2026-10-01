//! Codex-specific finite qualification mapping. This product edge owns Codex
//! route/input syntax and shell probe recipes. All inputs are bounded data;
//! it reads no host path, credential, endpoint, capability or canary contents.
//! Resolving a template does not authenticate its owner or authorize a send.
use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::qualification_request_binding::{
    MAX_QUALIFICATION_REQUEST_BINDING_BYTES, QualificationRequestBindingInput,
    QualificationRequestBindingOutput,
};
use serde_json::Value;

/// Bind one typed, sealed request at the admitted product edge. This returns
/// data only; the daemon rejoins adapter/input identities before any send.
pub fn bind_qualification_request(
    input: &QualificationRequestBindingInput,
) -> Result<QualificationRequestBindingOutput> {
    input.validate()?;
    let resolved_body =
        resolve_controller_canary_turn(&input.template, &input.controller_canary_path)?;
    let output = QualificationRequestBindingOutput {
        schema: 1,
        input_digest: input.digest()?,
        resolved_body,
    };
    output.validate_for(input)?;
    Ok(output)
}

/// Strict current canonical input/output bytes. The CLI additionally requires
/// its inherited descriptor to be sealed before this pure byte boundary runs.
pub fn bind_qualification_request_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    let input: QualificationRequestBindingInput =
        ryeos_external_execution_contract::from_json_slice_strict(
            bytes,
            MAX_QUALIFICATION_REQUEST_BINDING_BYTES,
        )
        .context("decode bounded qualification request binding")?;
    input.validate()?;
    ensure!(
        input.canonical_bytes()?.as_slice() == bytes,
        "qualification request binding input is not canonical"
    );
    bind_qualification_request(&input)?.canonical_bytes()
}

pub const Q_CONTROLLER_CANARY_PATH_SLOT: &str = "{{ryeos.q.controller_canary_path.v1}}";
pub const Q_CONTROLLER_CANARY_DENIAL: &str = "controller-canary-read-denied";
const MAX_PATH_BYTES: usize = 1024;
const MAX_TURN_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QControllerCanaryCommands {
    pub read_command: String,
    pub local_refusal_command: String,
}

/// Recipes, not authority. The verifier must first join this public path to
/// the exact original-daemon binding and actual request/process transcript.
pub fn controller_canary_commands(controller_path: &str) -> Result<QControllerCanaryCommands> {
    validate_controller_canary_path(controller_path)?;
    Ok(QControllerCanaryCommands {
        read_command: format!(
            "{{ if IFS= read -r line < \"{controller_path}\"; then printf \"%s\" \"$line\"; else printf \"%s\" {Q_CONTROLLER_CANARY_DENIAL}; exit 73; fi; }} 2>/dev/null"
        ),
        local_refusal_command: format!(
            "printf '%s' 'unexpected local write' > '{controller_path}'"
        ),
    })
}

pub fn validate_controller_canary_path(path: &str) -> Result<()> {
    ensure!(
        path.len() <= MAX_PATH_BYTES
            && path.starts_with('/')
            && path.ends_with("/controller-canary/canary")
            && path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
            && path[1..]
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "Q canary path is not the bounded original child coordinate"
    );
    Ok(())
}

/// The finite Q setup cannot add an environment, capability, or configuration
/// override. Model selection is optional data, not evidence of tool availability.
pub fn validate_controller_canary_setup(setup: &Value) -> Result<()> {
    let root = setup.as_object().context("Q setup is not an object")?;
    ensure!(
        root.len() == 2 && root.get("route_id").and_then(Value::as_str) == Some("session.start"),
        "Q setup changed its finite route envelope"
    );
    let payload = root
        .get("payload")
        .and_then(Value::as_object)
        .context("Q setup has no payload")?;
    ensure!(
        payload
            .keys()
            .all(|key| matches!(key.as_str(), "experimentalRawEvents" | "model"))
            && payload.get("experimentalRawEvents") == Some(&Value::Bool(true)),
        "Q setup requires raw events and admits only optional model selection"
    );
    if let Some(model) = payload.get("model").filter(|value| !value.is_null()) {
        let model = model.as_str().context("Q setup model is not text")?;
        ensure!(
            !model.is_empty()
                && model.len() <= 256
                && model.trim() == model
                && !model.chars().any(char::is_control),
            "Q setup model is not a bounded canonical identifier"
        );
    }
    Ok(())
}

/// Validate the original signed setup and turn together before retaining a
/// consumer record. This pure product check grants no contact authority.
pub fn validate_controller_canary_conversation(setup: &Value, turn: &Value) -> Result<()> {
    validate_controller_canary_setup(setup)?;
    validate_controller_canary_turn(turn)
}

/// Only one text input in the existing exact turn.start envelope is bindable.
/// No caller-selected JSON pointer, recursive replacement, or shell expansion.
/// The outer request validator separately bounds setup + resolved turn together.
pub fn validate_controller_canary_turn(template: &Value) -> Result<()> {
    let root = template
        .as_object()
        .context("Q canary turn is not an object")?;
    ensure!(
        root.len() == 2 && root.get("route_id").and_then(Value::as_str) == Some("turn.start"),
        "Q canary turn changed its finite route envelope"
    );
    let payload = root
        .get("payload")
        .and_then(Value::as_object)
        .context("Q canary turn has no payload")?;
    // The bridge supplies the original bound thread and fixed route settings.
    // This finite probe may not change sticky environments, permissions, model
    // selection or any other turn state, including through null-valued keys.
    ensure!(
        payload.len() == 1 && payload.contains_key("input"),
        "Q canary turn payload must contain only its original input"
    );
    let input = payload
        .get("input")
        .and_then(Value::as_array)
        .context("Q canary turn has no input array")?;
    ensure!(
        input.len() == 1,
        "Q canary turn requires one original text input"
    );
    let item = input[0]
        .as_object()
        .context("Q canary input is not an object")?;
    ensure!(
        item.get("type").and_then(Value::as_str) == Some("text")
            && item
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "text" | "text_elements"))
            && item
                .get("text_elements")
                .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty)),
        "Q canary input is not plain text without spans"
    );
    let text = item
        .get("text")
        .and_then(Value::as_str)
        .context("Q canary text is absent")?;
    let bytes = lillux::canonical_json(template)?;
    ensure!(
        bytes.len() <= MAX_TURN_BYTES
            && text.matches(Q_CONTROLLER_CANARY_PATH_SLOT).count() == 1
            && bytes.matches(Q_CONTROLLER_CANARY_PATH_SLOT).count() == 1,
        "Q canary path slot must occur exactly once at its fixed text field"
    );
    Ok(())
}

pub fn resolve_controller_canary_turn(template: &Value, controller_path: &str) -> Result<Value> {
    validate_controller_canary_turn(template)?;
    validate_controller_canary_path(controller_path)?;
    let mut resolved = template.clone();
    let text = resolved
        .pointer_mut("/payload/input/0/text")
        .context("Q canary slot disappeared")?;
    *text = Value::String(
        text.as_str()
            .context("Q canary slot is not text")?
            .replacen(Q_CONTROLLER_CANARY_PATH_SLOT, controller_path, 1),
    );
    ensure!(
        lillux::canonical_json(&resolved)?.len() <= MAX_TURN_BYTES,
        "Q resolved canary turn exceeds the original request bound"
    );
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn template() -> Value {
        json!({"route_id":"turn.start", "payload":{"input":[{"type":"text", "text":format!("Probe {Q_CONTROLLER_CANARY_PATH_SLOT} exactly once")}]}})
    }

    #[test]
    fn fixed_slot_changes_only_original_text_and_is_not_authority() {
        let original = template();
        let path = "/private/Q-abc/controller-canary/canary";
        let resolved = resolve_controller_canary_turn(&original, path).unwrap();
        assert_eq!(
            resolved["payload"]["input"][0]["text"],
            format!("Probe {path} exactly once")
        );
        assert_eq!(original, template());
        let mut expected = original.clone();
        expected["payload"]["input"][0]["text"] = resolved["payload"]["input"][0]["text"].clone();
        assert_eq!(resolved, expected);
        assert_ne!(
            lillux::sha256_hex(lillux::canonical_json(&resolved).unwrap().as_bytes()),
            lillux::sha256_hex(lillux::canonical_json(&original).unwrap().as_bytes())
        );
    }

    #[test]
    fn changed_route_slot_spans_and_path_are_refused() {
        let path = "/private/Q-abc/controller-canary/canary";
        for change in [
            json!({"route_id":"session.start", "payload":template()["payload"]}),
            json!({"route_id":"turn.start", "payload":{"input":[{"type":"text","text":"missing"}]}}),
            json!({"route_id":"turn.start", "payload":{"input":[{"type":"text","text":format!("{Q_CONTROLLER_CANARY_PATH_SLOT}{Q_CONTROLLER_CANARY_PATH_SLOT}")}]}}),
            json!({"route_id":"turn.start", "payload":{"input":template()["payload"]["input"], "extra":Q_CONTROLLER_CANARY_PATH_SLOT}}),
            json!({"route_id":"turn.start", "payload":{"input":[{"type":"text","text":Q_CONTROLLER_CANARY_PATH_SLOT,"text_elements":[{}]}]}}),
        ] {
            assert!(resolve_controller_canary_turn(&change, path).is_err());
        }
        for path in [
            "relative/controller-canary/canary",
            "/private/../controller-canary/canary",
            "/private//controller-canary/canary",
            "/private/$x/controller-canary/canary",
            "/private/other",
        ] {
            assert!(resolve_controller_canary_turn(&template(), path).is_err());
        }
        let mut spans = template();
        spans["payload"]["input"][0]["text_elements"] = json!([]);
        assert!(resolve_controller_canary_turn(&spans, path).is_ok());
    }

    #[test]
    fn both_template_and_expanded_body_remain_bounded() {
        let mut value = template();
        value["payload"]["input"][0]["text"] = json!(format!(
            "{}{Q_CONTROLLER_CANARY_PATH_SLOT}",
            "x".repeat(MAX_TURN_BYTES)
        ));
        assert!(validate_controller_canary_turn(&value).is_err());
        value["payload"]["input"][0]["text"] = json!(format!(
            "{}{Q_CONTROLLER_CANARY_PATH_SLOT}",
            "x".repeat(7900)
        ));
        validate_controller_canary_turn(&value).unwrap();
        let long_path = format!("/{}/controller-canary/canary", "a".repeat(900));
        assert!(resolve_controller_canary_turn(&value, &long_path).is_err());
    }

    fn binding_input() -> QualificationRequestBindingInput {
        QualificationRequestBindingInput {
            schema: 1,
            template: template(),
            controller_canary_path: "/private/Q-abc/controller-canary/canary".into(),
        }
    }

    fn setup() -> Value {
        json!({"route_id":"session.start","payload":{"experimentalRawEvents":true}})
    }

    #[test]
    fn canary_setup_accepts_only_bounded_optional_model_and_required_raw_events() {
        validate_controller_canary_conversation(&setup(), &template()).unwrap();
        for model in [Value::Null, json!("codex-model"), json!("é".repeat(128))] {
            let mut value = setup();
            value["payload"]["model"] = model;
            validate_controller_canary_conversation(&value, &template()).unwrap();
        }
        for model in [
            json!(""),
            json!(" leading"),
            json!("trailing "),
            json!("a\nb"),
            json!("a\u{7f}b"),
            json!("é".repeat(129)),
            json!(false),
            json!([]),
            json!({}),
        ] {
            let mut value = setup();
            value["payload"]["model"] = model;
            assert!(validate_controller_canary_setup(&value).is_err());
        }
        for raw in [Value::Null, json!(false), json!("true"), json!(1)] {
            let mut value = setup();
            value["payload"]["experimentalRawEvents"] = raw;
            assert!(validate_controller_canary_setup(&value).is_err());
        }
        let mut missing = setup();
        missing["payload"]
            .as_object_mut()
            .unwrap()
            .remove("experimentalRawEvents");
        assert!(validate_controller_canary_setup(&missing).is_err());
    }

    #[test]
    fn canary_conversation_refuses_setup_overrides_and_changed_turn() {
        for key in [
            "config",
            "environments",
            "cwd",
            "runtimeWorkspaceRoots",
            "permissions",
            "approvalPolicy",
            "approvalsReviewer",
            "sandbox",
            "dynamicTools",
            "selectedCapabilityRoots",
            "developerInstructions",
            "baseInstructions",
            "modelProvider",
            "serviceName",
            "threadSource",
            "futureOverride",
        ] {
            for value in [Value::Null, json!({"unexpected":true})] {
                let mut changed = setup();
                changed["payload"][key] = value;
                assert!(
                    validate_controller_canary_conversation(&changed, &template()).is_err(),
                    "{key}"
                );
            }
        }
        for changed in [
            Value::Null,
            json!([]),
            json!({"route_id":"thread/start","payload":{}}),
            json!({"route_id":"session.start","payload":null}),
            json!({"route_id":"session.start","payload":{"experimentalRawEvents":true},"extra":null}),
        ] {
            assert!(validate_controller_canary_conversation(&changed, &template()).is_err());
        }
        let mut changed_turn = template();
        changed_turn["payload"]["environments"] = Value::Null;
        assert!(validate_controller_canary_conversation(&setup(), &changed_turn).is_err());
        changed_turn = template();
        changed_turn["payload"]["input"][0]["text"] = json!("missing original slot");
        assert!(validate_controller_canary_conversation(&setup(), &changed_turn).is_err());
    }

    #[test]
    fn canary_turn_refuses_sticky_overrides_at_every_binding_boundary() {
        for (key, value) in [
            ("environments", json!([{"environmentId":"local","cwd":"/"}])),
            ("cwd", json!("/controller")),
            ("runtimeWorkspaceRoots", json!(["/controller"])),
            ("permissions", json!("danger-full-access")),
            ("approvalPolicy", json!("never")),
            ("approvalsReviewer", json!("user")),
            ("sandboxPolicy", json!({"type":"dangerFullAccess"})),
            ("model", json!("another-model")),
            ("effort", json!("high")),
            ("collaborationMode", json!({"mode":"plan"})),
            ("additionalContext", json!([])),
            ("threadId", json!("another-thread")),
            ("futureOverride", json!(true)),
        ] {
            for override_value in [value, Value::Null] {
                let mut input = binding_input();
                input.template["payload"][key] = override_value;
                assert!(
                    validate_controller_canary_turn(&input.template).is_err(),
                    "{key}"
                );
                assert!(
                    resolve_controller_canary_turn(&input.template, &input.controller_canary_path)
                        .is_err(),
                    "{key}"
                );
                assert!(bind_qualification_request(&input).is_err(), "{key}");
                assert!(
                    bind_qualification_request_bytes(&input.canonical_bytes().unwrap()).is_err(),
                    "{key}"
                );
            }
        }
    }

    #[test]
    fn typed_binding_commits_exact_input_and_matches_independent_pure_resolution() {
        let input = binding_input();
        let encoded = bind_qualification_request_bytes(&input.canonical_bytes().unwrap()).unwrap();
        let output: QualificationRequestBindingOutput =
            ryeos_external_execution_contract::from_json_slice_strict(
                &encoded,
                MAX_QUALIFICATION_REQUEST_BINDING_BYTES,
            )
            .unwrap();
        output.validate_for(&input).unwrap();
        assert_eq!(output.canonical_bytes().unwrap(), encoded);
        assert_eq!(
            output.resolved_body,
            resolve_controller_canary_turn(&input.template, &input.controller_canary_path).unwrap()
        );
        let mut swapped = input;
        swapped.controller_canary_path = "/private/Q-other/controller-canary/canary".into();
        assert!(output.validate_for(&swapped).is_err());
    }

    #[test]
    fn byte_boundary_refuses_noncanonical_duplicate_unknown_oversized_and_stale_inputs() {
        let input = binding_input();
        let canonical = input.canonical_bytes().unwrap();
        let mut whitespace = canonical.clone();
        whitespace.push(b'\n');
        assert!(bind_qualification_request_bytes(&whitespace).is_err());
        let mut duplicate = b"{\"schema\":1,".to_vec();
        duplicate.extend_from_slice(&canonical[1..]);
        assert!(bind_qualification_request_bytes(&duplicate).is_err());
        assert!(
            bind_qualification_request_bytes(&vec![
                b'x';
                MAX_QUALIFICATION_REQUEST_BINDING_BYTES + 1
            ])
            .is_err()
        );
        let mut unknown = serde_json::to_value(&input).unwrap();
        unknown["caller_command"] = json!("unadmitted");
        assert!(
            bind_qualification_request_bytes(lillux::canonical_json(&unknown).unwrap().as_bytes())
                .is_err()
        );
        unknown.as_object_mut().unwrap().remove("caller_command");
        unknown["schema"] = json!(0);
        assert!(
            bind_qualification_request_bytes(lillux::canonical_json(&unknown).unwrap().as_bytes())
                .is_err()
        );
    }

    #[test]
    fn recipes_are_exact_path_only_data_and_refuse_shell_interpolation() {
        let path = "/private/Q-abc/controller-canary/canary";
        let commands = controller_canary_commands(path).unwrap();
        assert_eq!(
            commands.local_refusal_command,
            format!("printf '%s' 'unexpected local write' > '{path}'")
        );
        assert!(
            commands
                .read_command
                .starts_with("{ if IFS= read -r line < ")
        );
        assert!(
            commands
                .read_command
                .ends_with("; exit 73; fi; } 2>/dev/null")
        );
        assert_eq!(commands.read_command.matches(path).count(), 1);
        for invalid in [
            "/x/$(id)/controller-canary/canary",
            "/x/'/controller-canary/canary",
            "/x/../controller-canary/canary",
        ] {
            assert!(controller_canary_commands(invalid).is_err());
        }
    }
}
