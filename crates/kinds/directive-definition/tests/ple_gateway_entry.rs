//! The PLE gateway provider entry loads, validates, and resolves per-model
//! rows. The fixture mirrors the consumer's entry file and is updated with it.

use ryeos_directive_definition::ProviderConfig;
use serde_json::{Value, json};

fn load() -> ProviderConfig {
    let yaml = include_str!("fixtures/ple-gateway.yaml");
    let value: Value = serde_yaml::from_str(yaml).expect("fixture is yaml");
    serde_json::from_value(value).expect("fixture deserializes as a provider config")
}

fn resolved(model: &str) -> Value {
    serde_json::to_value(load().resolve_for_model(model)).expect("serialize")
}

fn accepts_images(model: &str) -> bool {
    !resolved(model)["image_input"]["accepts"].is_null()
}

#[test]
fn the_entry_validates() {
    load()
        .validate("")
        .expect("entry and every profile validate");
}

#[test]
fn image_input_per_model() {
    for model in [
        "claude-opus-5-5",
        "gemini-4-argon",
        "gemini-3.8-flash",
        "gemini-3.1-pro-preview",
        "gemma-4-26b",
        "google-cloud/gemini-4-argon",
    ] {
        assert!(accepts_images(model), "{model} accepts images");
        let accepts = &resolved(model)["image_input"]["accepts"];
        assert_eq!(accepts["media_types"][0], "image/png", "{model}");
    }
    for model in ["gpt-oss-120b", "glm-5.2", "some-unlisted-model"] {
        let value = resolved(model);
        assert!(value["image_input"]["accepts"].is_null(), "{model}");
        assert_eq!(value["image_input"]["unsupported"], "refuse", "{model}");
    }
}

#[test]
fn only_claude_marks_the_cache() {
    let claude = resolved("claude-opus-5-5");
    assert_eq!(
        claude["cache_marks"]["mark"],
        json!({"cache_control": {"type": "ephemeral"}})
    );
    assert_eq!(claude["cache_marks"]["max_marks"], 4);
    assert_eq!(
        claude["cache_marks"]["positions"],
        json!(["system", "newest", "previous_tail"])
    );
    for model in [
        "gemini-4-argon",
        "gemini-3.8-flash",
        "gemini-3.1-pro-preview",
        "gemma-4-26b",
        "gpt-oss-120b",
        "glm-5.2",
    ] {
        assert!(resolved(model).get("cache_marks").is_none(), "{model}");
    }
}

#[test]
fn claude_replays_signed_thinking_and_the_others_capture_text() {
    let claude = resolved("claude-opus-5-5");
    let reasoning = &claude["schemas"]["reasoning"];
    assert_eq!(reasoning["capture"][0]["kind"], "blocks");
    assert_eq!(reasoning["capture"][0]["field"], "thinking_blocks");
    assert_eq!(reasoning["capture"][0]["close_key"], "signature");
    assert_eq!(reasoning["replay"]["compat_group"], "claude-thinking-1");
    let placement = &reasoning["replay"]["placements"][0];
    assert_eq!(placement["from"], "blocks");
    assert_eq!(placement["scope"], "open_loop");
    assert_eq!(placement["on_missing"], "omit");

    for model in ["gpt-oss-120b", "glm-5.2", "gemini-4-argon", "gemma-4-26b"] {
        let reasoning = &resolved(model)["schemas"]["reasoning"];
        assert_eq!(reasoning["capture"][0]["kind"], "text_delta", "{model}");
        assert_eq!(
            reasoning["capture"][0]["fields"],
            json!(["reasoning_content"]),
            "{model}"
        );
        assert!(
            reasoning.get("replay").is_none(),
            "{model} has no replay row"
        );
    }
}

#[test]
fn every_model_reads_cached_counts_from_the_gateway_usage_chunk() {
    for model in [
        "claude-opus-5-5",
        "gemini-4-argon",
        "gpt-oss-120b",
        "glm-5.2",
    ] {
        let usage = &resolved(model)["schemas"]["streaming"]["metadata"]["usage"];
        assert_eq!(
            usage["cache_read_tokens_path"], "cache_read_input_tokens",
            "{model}"
        );
        assert_eq!(
            usage["cache_write_tokens_path"], "cache_creation_input_tokens",
            "{model}"
        );
        assert_eq!(usage["cache_included_in_input"], true, "{model}");
    }
}

#[test]
fn the_entry_carries_no_tariff_and_no_setup() {
    let config = load();
    let mut resolved = vec![serde_json::to_value(&config).expect("serialize")];
    for model in [
        "claude-opus-5-5",
        "gemini-4-argon",
        "gpt-oss-120b",
        "unlisted-model",
    ] {
        resolved.push(self::resolved(model));
    }
    for value in resolved {
        assert!(value.get("spend_authority").is_none_or(Value::is_null));
        assert!(value["extra"].get("setup").is_none());
    }
}

#[test]
fn near_miss_and_unlisted_models_fall_back_to_the_safe_default() {
    for model in ["gemma-4-31b", "gemini-3.8-flash-lite", "unlisted-model"] {
        let value = resolved(model);
        assert!(value.get("cache_marks").is_none(), "{model}");
        assert!(value["image_input"]["accepts"].is_null(), "{model}");
        assert_eq!(value["image_input"]["unsupported"], "refuse", "{model}");
    }
}

#[test]
fn the_claude_profile_restates_the_entry_schemas_except_reasoning() {
    let mut entry = resolved("unlisted-model")["schemas"].clone();
    let mut claude = resolved("claude-opus-5-5")["schemas"].clone();
    entry.as_object_mut().unwrap().remove("reasoning");
    claude.as_object_mut().unwrap().remove("reasoning");
    assert_eq!(entry, claude);
}
