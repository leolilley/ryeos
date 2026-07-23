use serde_json::Value;

use crate::directive::{StreamingConfig, UsageAggregation};
use crate::provider_adapter::http::{ProviderUsageSource, TokenUsage};

/// Pull provider-side metadata (usage totals, finish_reason) out of a
/// single SSE event block. Stream events and response accounting are kept
/// separate: this normalizes the provider totals used for cost accounting and
/// response routing without affecting emitted event ordering.
///
/// When streaming metadata is declared, its paths and aggregation semantics
/// are authoritative. Provider identity never influences parsing.
pub(super) fn harvest_chunk_meta(
    block: &str,
    last_usage: &mut Option<TokenUsage>,
    last_finish: &mut Option<String>,
    last_response_id: &mut Option<String>,
    streaming: Option<&StreamingConfig>,
) {
    use ryeos_runtime::template::resolve_path;

    // Parse logical SSE events, not physical lines. Multiple `data:` lines in
    // one event are joined by the shared framing parser before JSON decoding.
    for (_, payload) in super::split_sse_events(block) {
        if payload == "[DONE]" || payload.is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            Err(e) => {
                tracing::trace!(
                    "skipping malformed SSE data JSON: {e}; raw={}",
                    payload.chars().take(120).collect::<String>()
                );
                continue;
            }
        };

        let metadata = streaming.and_then(|config| config.metadata.as_ref());
        if let Some(metadata) = metadata {
            if let Some(path) = metadata.response_id_path.as_deref() {
                if let Some(response_id) = resolve_path(&parsed, path).and_then(Value::as_str) {
                    if !response_id.is_empty() {
                        *last_response_id = Some(response_id.to_string());
                    }
                }
            }
            if let Some(usage_config) = metadata.usage.as_ref() {
                if let Some(usage) =
                    resolve_path(&parsed, &usage_config.path).filter(|value| !value.is_null())
                {
                    let mut anomalies = Vec::new();
                    let mut metadata_anomalies = Vec::new();
                    let input_tokens = read_required_u64(
                        usage,
                        usage_config.input_tokens_path.as_deref(),
                        &mut anomalies,
                    );
                    let output_tokens = read_required_u64(
                        usage,
                        usage_config.output_tokens_path.as_deref(),
                        &mut anomalies,
                    );
                    let reasoning_tokens = read_optional_u64(
                        usage,
                        usage_config.reasoning_tokens_path.as_deref(),
                        &mut anomalies,
                    );
                    let reported_cost_usd = read_optional_nonnegative_f64(
                        usage,
                        usage_config.reported_cost_path.as_deref(),
                        &mut metadata_anomalies,
                    );
                    let cost_details = usage_config
                        .cost_details_path
                        .as_deref()
                        .and_then(|path| resolve_path(usage, path))
                        .cloned();
                    let is_byok = read_optional_bool(
                        usage,
                        usage_config.is_byok_path.as_deref(),
                        &mut metadata_anomalies,
                    );
                    if usage_config.reasoning_included_in_output {
                        if let (Some(reasoning_tokens), Some(output_tokens)) =
                            (reasoning_tokens, output_tokens)
                        {
                            if reasoning_tokens > output_tokens {
                                anomalies.push(format!(
                                    "reasoning tokens {reasoning_tokens} exceed output tokens \
                                     {output_tokens}"
                                ));
                            }
                        }
                    }
                    let update = TokenUsage {
                        input_tokens,
                        output_tokens,
                        reasoning_tokens,
                        reported_cost_usd,
                        cost_details,
                        is_byok,
                        source: ProviderUsageSource::SignedMetadata,
                        comparability: Default::default(),
                        provider_limit_contract: Default::default(),
                        anomalies,
                        metadata_anomalies,
                        contract_anomalies: Vec::new(),
                        snapshots_seen: 0,
                    };
                    match usage_config.aggregation {
                        UsageAggregation::CumulativeFields => {
                            update_usage_cumulative(last_usage, update)
                        }
                        UsageAggregation::LatestSnapshot => {
                            update_usage_latest(last_usage, update, usage_config.single_snapshot)
                        }
                    }
                }
            }
            if let Some(path) = metadata.finish_reason_path.as_deref() {
                if let Some(reason) = resolve_path(&parsed, path).and_then(Value::as_str) {
                    if !reason.is_empty() {
                        *last_finish = Some(reason.to_string());
                    }
                }
            }
        }

        // When no signed metadata declaration exists, the configured
        // protocol-family parser is the sole authority for usage and finish
        // events. Extracting them here as well would parse the same frame twice
        // and could synthesize conflicting snapshots.
    }
}

fn usage_is_empty(update: &TokenUsage) -> bool {
    if update.input_tokens.is_none()
        && update.output_tokens.is_none()
        && update.reasoning_tokens.is_none()
        && update.reported_cost_usd.is_none()
        && update.cost_details.is_none()
        && update.is_byok.is_none()
        && update.anomalies.is_empty()
        && update.metadata_anomalies.is_empty()
        && update.contract_anomalies.is_empty()
    {
        return true;
    }
    false
}

fn update_usage_latest(
    last_usage: &mut Option<TokenUsage>,
    mut update: TokenUsage,
    single_snapshot: bool,
) {
    if usage_is_empty(&update) {
        return;
    }
    if let Some(previous) = last_usage.take() {
        let snapshots_seen = previous.snapshots_seen.saturating_add(1);
        let mut anomalies = previous.anomalies;
        anomalies.extend(update.anomalies);
        update.anomalies = anomalies;
        let mut metadata_anomalies = previous.metadata_anomalies;
        metadata_anomalies.extend(update.metadata_anomalies);
        update.metadata_anomalies = metadata_anomalies;
        let mut contract_anomalies = previous.contract_anomalies;
        contract_anomalies.extend(update.contract_anomalies);
        if single_snapshot {
            contract_anomalies.push(format!(
                "received {snapshots_seen} usage snapshots where the signed contract permits one"
            ));
        }
        update.contract_anomalies = contract_anomalies;
        update.snapshots_seen = snapshots_seen;
    } else {
        update.snapshots_seen = 1;
    }
    *last_usage = Some(update);
}

fn update_usage_cumulative(last_usage: &mut Option<TokenUsage>, update: TokenUsage) {
    if usage_is_empty(&update) {
        return;
    }
    let previous = last_usage.take().unwrap_or_default();
    let snapshots_seen = previous.snapshots_seen.saturating_add(1);
    let mut anomalies = previous.anomalies;
    anomalies.extend(update.anomalies);
    let mut metadata_anomalies = previous.metadata_anomalies;
    metadata_anomalies.extend(update.metadata_anomalies);
    let mut contract_anomalies = previous.contract_anomalies;
    contract_anomalies.extend(update.contract_anomalies);
    let input_tokens = merge_cumulative_counter(
        "input_tokens",
        previous.input_tokens,
        update.input_tokens,
        &mut anomalies,
    );
    let output_tokens = merge_cumulative_counter(
        "output_tokens",
        previous.output_tokens,
        update.output_tokens,
        &mut anomalies,
    );
    let reasoning_tokens = merge_cumulative_counter(
        "reasoning_tokens",
        previous.reasoning_tokens,
        update.reasoning_tokens,
        &mut anomalies,
    );
    let reported_cost_usd = match (previous.reported_cost_usd, update.reported_cost_usd) {
        (Some(previous), Some(update)) => {
            if update < previous {
                metadata_anomalies.push(format!(
                    "reported_cost_usd regressed from {previous} to {update} in cumulative usage metadata"
                ));
            }
            Some(update)
        }
        (previous, update) => update.or(previous),
    };
    *last_usage = Some(TokenUsage {
        input_tokens,
        output_tokens,
        reasoning_tokens,
        reported_cost_usd,
        cost_details: update.cost_details.or(previous.cost_details),
        is_byok: update.is_byok.or(previous.is_byok),
        source: if update.source == ProviderUsageSource::Unknown {
            previous.source
        } else {
            update.source
        },
        comparability: update.comparability,
        provider_limit_contract: update.provider_limit_contract,
        anomalies,
        metadata_anomalies,
        contract_anomalies,
        snapshots_seen,
    });
}

fn merge_cumulative_counter(
    label: &str,
    previous: Option<u64>,
    update: Option<u64>,
    anomalies: &mut Vec<String>,
) -> Option<u64> {
    match (previous, update) {
        (Some(previous), Some(update)) => {
            if update < previous {
                anomalies.push(format!(
                    "{label} regressed from {previous} to {update} in cumulative usage metadata"
                ));
            }
            Some(update)
        }
        (previous, update) => update.or(previous),
    }
}

fn read_optional_u64(root: &Value, path: Option<&str>, anomalies: &mut Vec<String>) -> Option<u64> {
    let path = path?;
    let value = ryeos_runtime::template::resolve_path(root, path)?;
    match value.as_u64() {
        Some(value) => Some(value),
        None => {
            anomalies.push(format!("{path} is not a u64"));
            None
        }
    }
}

fn read_required_u64(root: &Value, path: Option<&str>, anomalies: &mut Vec<String>) -> Option<u64> {
    let Some(path) = path else {
        anomalies.push("required token path is not configured".to_string());
        return None;
    };
    let Some(value) = ryeos_runtime::template::resolve_path(root, path) else {
        anomalies.push(format!("{path} is missing"));
        return None;
    };
    match value.as_u64() {
        Some(value) => Some(value),
        None => {
            anomalies.push(format!("{path} is not a u64"));
            None
        }
    }
}

fn read_optional_nonnegative_f64(
    root: &Value,
    path: Option<&str>,
    anomalies: &mut Vec<String>,
) -> Option<f64> {
    let path = path?;
    let value = ryeos_runtime::template::resolve_path(root, path)?;
    match value.as_f64() {
        Some(value) if value >= 0.0 => Some(value),
        Some(_) => {
            anomalies.push(format!("{path} is negative"));
            None
        }
        None => {
            anomalies.push(format!("{path} is not a number"));
            None
        }
    }
}

fn read_optional_bool(
    root: &Value,
    path: Option<&str>,
    anomalies: &mut Vec<String>,
) -> Option<bool> {
    let path = path?;
    let value = ryeos_runtime::template::resolve_path(root, path)?;
    match value.as_bool() {
        Some(value) => Some(value),
        None => {
            anomalies.push(format!("{path} is not a boolean"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared_metadata(aggregation: UsageAggregation) -> StreamingConfig {
        StreamingConfig {
            mode: Some(crate::directive::StreamingMode::DeltaMerge),
            paths: None,
            metadata: Some(crate::directive::StreamMetadataConfig {
                usage: Some(crate::directive::StreamUsageConfig {
                    path: "usage".into(),
                    input_tokens_path: Some("prompt_tokens".into()),
                    output_tokens_path: Some("completion_tokens".into()),
                    reasoning_tokens_path: Some(
                        "completion_tokens_details.reasoning_tokens".into(),
                    ),
                    reported_cost_path: Some("cost".into()),
                    reported_cost_unit: Some(crate::directive::ReportedCostUnit::Usd),
                    cost_details_path: Some("cost_details".into()),
                    is_byok_path: Some("is_byok".into()),
                    reasoning_included_in_output: true,
                    aggregation,
                    single_snapshot: false,
                }),
                finish_reason_path: Some("choices.0.finish_reason".into()),
                error: None,
                response_id_path: Some("id".into()),
                generation_id_header: None,
            }),
        }
    }

    #[test]
    fn generic_cumulative_usage_flags_regression_and_latest_finish_wins() {
        let block = concat!(
            "data: {\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":8},",
            "\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":9},",
            "\"choices\":[{\"finish_reason\":\"stop\"}]}\n",
        );
        let mut usage = None;
        let mut finish = None;

        let mut response_id = None;
        let streaming = declared_metadata(UsageAggregation::CumulativeFields);
        harvest_chunk_meta(
            block,
            &mut usage,
            &mut finish,
            &mut response_id,
            Some(&streaming),
        );

        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(9));
        assert!(usage
            .anomalies
            .iter()
            .any(|anomaly| anomaly.contains("regressed")));
        assert_eq!(finish.as_deref(), Some("stop"));
    }

    #[test]
    fn openai_compatible_usage_preserves_reasoning_cost_and_byok_metadata() {
        let block = concat!(
            "data: {\"id\":\"gen-response-123\",\"choices\":[],\"usage\":{\"prompt_tokens\":50,",
            "\"completion_tokens\":20,\"completion_tokens_details\":{",
            "\"reasoning_tokens\":15},\"cost\":0.0123,",
            "\"cost_details\":{\"upstream_inference_cost\":0.01},",
            "\"is_byok\":false}}\n",
        );
        let mut usage = None;
        let mut finish = None;

        let mut response_id = None;
        let streaming = declared_metadata(UsageAggregation::LatestSnapshot);
        harvest_chunk_meta(
            block,
            &mut usage,
            &mut finish,
            &mut response_id,
            Some(&streaming),
        );

        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, Some(50));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.reasoning_tokens, Some(15));
        assert_eq!(usage.reported_cost_usd, Some(0.0123));
        assert_eq!(usage.cost_details.unwrap()["upstream_inference_cost"], 0.01);
        assert_eq!(usage.is_byok, Some(false));
        assert_eq!(response_id.as_deref(), Some("gen-response-123"));
    }

    #[test]
    fn usage_totals_are_aggregated_without_provider_identity_branching() {
        let block = concat!(
            "data: {\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":8}}\n\n",
            "data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":9,",
            "\"cost\":0.5}}\n",
        );
        let mut usage = None;
        let mut finish = None;
        let mut response_id = None;
        let streaming = declared_metadata(UsageAggregation::CumulativeFields);

        harvest_chunk_meta(
            block,
            &mut usage,
            &mut finish,
            &mut response_id,
            Some(&streaming),
        );

        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(9));
        assert!(usage
            .anomalies
            .iter()
            .any(|anomaly| anomaly.contains("regressed")));
        assert_eq!(usage.reported_cost_usd, Some(0.5));
    }

    #[test]
    fn malformed_usage_is_diagnostic_metadata_not_a_stream_limit() {
        let block = concat!(
            "data: {\"usage\":{\"prompt_tokens\":\"bad\",",
            "\"completion_tokens\":5,\"completion_tokens_details\":{",
            "\"reasoning_tokens\":7},\"cost\":-1,\"is_byok\":\"no\"}}\n",
        );
        let mut usage = None;
        let mut finish = None;
        let mut response_id = None;
        let streaming = declared_metadata(UsageAggregation::LatestSnapshot);

        harvest_chunk_meta(
            block,
            &mut usage,
            &mut finish,
            &mut response_id,
            Some(&streaming),
        );

        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, Some(5));
        assert!(usage
            .anomalies
            .iter()
            .any(|anomaly| anomaly.contains("prompt_tokens is not a u64")));
        assert!(usage
            .anomalies
            .iter()
            .any(|anomaly| anomaly.contains("reasoning tokens 7 exceed")));
        assert!(usage
            .metadata_anomalies
            .iter()
            .any(|anomaly| anomaly.contains("cost is negative")));
        assert!(usage
            .metadata_anomalies
            .iter()
            .any(|anomaly| anomaly.contains("is_byok is not a boolean")));
    }

    #[test]
    fn protocol_paths_do_not_compete_with_metadata_authority() {
        let paths = crate::directive::StreamPaths {
            content_path: "candidates.0.content.parts".into(),
            text_field: "text".into(),
            thought_field: None,
            tool_call_field: None,
            tool_call_name_path: None,
            tool_call_args_path: None,
            usage_path: Some("usageMetadata".into()),
            input_tokens_field: Some("promptTokenCount".into()),
            output_tokens_field: Some("candidatesTokenCount".into()),
            finish_reason_path: Some("candidates.0.finishReason".into()),
        };
        let streaming = StreamingConfig {
            mode: Some(crate::directive::StreamingMode::CompleteChunks),
            paths: Some(paths),
            metadata: None,
        };
        let block = concat!(
            "data: {\"usageMetadata\":{\"promptTokenCount\":7,",
            "\"candidatesTokenCount\":5},\"candidates\":[{\"finishReason\":\"MAX_TOKENS\"}],",
            "\"usage\":{\"prompt_tokens\":99,\"completion_tokens\":99}}\n",
        );
        let mut usage = None;
        let mut finish = None;

        let mut response_id = None;
        harvest_chunk_meta(
            block,
            &mut usage,
            &mut finish,
            &mut response_id,
            Some(&streaming),
        );

        assert!(usage.is_none());
        assert!(finish.is_none());
    }

    #[test]
    fn metadata_parser_joins_multiline_sse_data_before_json_decode() {
        let block = concat!(
            "data: {\"usage\":{\n",
            "data: \"prompt_tokens\":7,\"completion_tokens\":5},\n",
            "data: \"choices\":[{\"finish_reason\":\"stop\"}]}\n\n",
        );
        let mut usage = None;
        let mut finish = None;
        let mut response_id = None;

        let streaming = declared_metadata(UsageAggregation::CumulativeFields);
        harvest_chunk_meta(
            block,
            &mut usage,
            &mut finish,
            &mut response_id,
            Some(&streaming),
        );

        let usage = usage.expect("multiline logical event usage");
        assert_eq!(usage.complete_token_counts(), Some((7, 5)));
        assert_eq!(finish.as_deref(), Some("stop"));
    }

    #[test]
    fn absent_optional_reasoning_metadata_does_not_invalidate_usage() {
        let block = "data: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":5}}\n\n";
        let mut usage = None;
        let mut finish = None;
        let mut response_id = None;
        let streaming = declared_metadata(UsageAggregation::LatestSnapshot);

        harvest_chunk_meta(
            block,
            &mut usage,
            &mut finish,
            &mut response_id,
            Some(&streaming),
        );

        let usage = usage.expect("usage snapshot");
        assert!(usage.is_valid(), "optional omission is not malformed");
        assert_eq!(usage.reasoning_tokens, None);
        assert!(usage.anomalies.is_empty());
    }
}
