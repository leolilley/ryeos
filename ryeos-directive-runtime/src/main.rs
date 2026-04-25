use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::json;

mod adapter;
mod bootstrap;
mod budget;
mod continuation;
mod directive;
mod dispatcher;
mod harness;
mod knowledge;
mod parser;
mod result_guard;
mod resume;
mod runner;

use ryeos_runtime::envelope::{LaunchEnvelope, RuntimeResult, ENVELOPE_VERSION};

fn main() {
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_directive_runtime());

    let result = run_directive();
    let exit_code = match &result {
        Ok(_) => 0,
        Err(_) => 1,
    };

    if let Ok(runtime_result) = result {
        let output = serde_json::to_string(&runtime_result).unwrap_or_else(|e| {
            serde_json::to_string(&json!({
                "success": false,
                "status": "errored",
                "thread_id": "",
                "result": format!("serialization error: {}", e),
            }))
            .unwrap()
        });
        println!("{}", output);
    }

    std::process::exit(exit_code);
}

fn run_directive() -> Result<RuntimeResult> {
    let mut stdin_data = Vec::new();
    std::io::stdin().read_to_end(&mut stdin_data)?;

    let envelope: LaunchEnvelope = match serde_json::from_slice(&stdin_data) {
        Ok(e) => e,
        Err(e) => {
            return Ok(RuntimeResult {
                success: false,
                status: "errored".to_string(),
                thread_id: String::new(),
                result: Some(format!("invalid envelope: {}", e)),
                outputs: json!({}),
                cost: None,
            });
        }
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_with_envelope(envelope))
}

async fn run_with_envelope(envelope: LaunchEnvelope) -> Result<RuntimeResult> {
    if envelope.envelope_version != ENVELOPE_VERSION {
        bail!(
            "unsupported envelope version: {} (expected {})",
            envelope.envelope_version,
            ENVELOPE_VERSION
        );
    }

    let project_root = envelope.roots.project_root.clone();
    let user_root = envelope.roots.user_root.clone();
    let system_roots = envelope.roots.system_roots.clone();

    let directive_path = project_root.join(&envelope.target.path);
    if !directive_path.exists() {
        bail!("directive file not found: {}", directive_path.display());
    }

    let content = std::fs::read_to_string(&directive_path)?;

    use sha2::{Digest, Sha256};
    let computed = format!("{:x}", Sha256::digest(content.as_bytes()));
    if let Some(expected) = envelope.target.digest.strip_prefix("sha256:") {
        if computed != expected {
            bail!("target digest mismatch: expected {}, got {}", expected, computed);
        }
    }

    let parsed = parser::parse_directive(&content, &envelope.target.path)?;

    let verified_loader = ryeos_runtime::verified_loader::VerifiedLoader::new(
        envelope.roots.project_root.clone(),
        envelope.roots.user_root.clone(),
        envelope.roots.system_roots.clone(),
    );

    let callback = ryeos_runtime::callback_client::CallbackClient::new(
        &envelope.callback,
        &envelope.thread_id,
        envelope.roots.project_root.to_str().unwrap_or(""),
    );

    let bootstrap_output = bootstrap::bootstrap(
        &project_root,
        user_root.as_deref(),
        &system_roots,
        &parsed,
        &envelope.policy.hard_limits,
        &verified_loader,
    )?;

    let provider = bootstrap_output.provider.clone();
    let model_name = bootstrap_output.model_name.clone();
    let context_window = bootstrap_output.context_window;

    let harness = harness::Harness::new(&envelope.policy, envelope.request.depth, bootstrap_output.config.risk_policy.clone());

    // Wire SIGTERM → harness cancelled flag so runner can exit cleanly
    {
        let cancelled = harness.cancelled_flag();
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .context("failed to install SIGTERM handler")?;
        tokio::spawn(async move {
            sigterm.recv().await;
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::info!("received SIGTERM, cancellation requested");
        });
    }
    let budget = budget::BudgetTracker::new(envelope.policy.hard_limits.spend_usd);

    let hooks = bootstrap_output.config.hooks.clone();

    let mut runner_inst = if let Some(ref resume_id) = envelope.request.previous_thread_id {
        let resume_state = resume::load_resume_state(&callback, resume_id).await?;
        callback.append_event("thread_continued", json!({"previous_thread_id": resume_id})).await?;
        runner::Runner::from_resume(
            resume_state,
            bootstrap_output.config.tools,
            bootstrap_output.config.system_prompt,
            harness,
            budget,
            callback,
            context_window,
            provider,
            model_name,
            envelope.thread_id.clone(),
            hooks,
        )
    } else {
        let user_prompt = bootstrap_output.config.user_prompt.clone();
        let inputs = envelope.request.inputs.clone();

        // Apply template interpolation with envelope inputs as context
        let interpolated_prompt = if !inputs.is_null() {
            ryeos_runtime::interpolate(&serde_json::json!(user_prompt), &inputs)
                .map(|v| v.as_str().unwrap_or(&user_prompt).to_string())
                .unwrap_or(user_prompt)
        } else {
            user_prompt
        };

        let prompt = if inputs.is_object() && !inputs.as_object().map_or(true, |o| o.is_empty()) {
            format!(
                "{}\n\nInputs:\n{}",
                interpolated_prompt,
                serde_json::to_string_pretty(&inputs)?
            )
        } else {
            interpolated_prompt
        };

        let mut messages = Vec::new();

        // Context before: injected before user prompt
        if let Some(ref before) = bootstrap_output.config.context_before {
            messages.push(directive::ProviderMessage {
                role: "user".to_string(),
                content: Some(json!(before)),
                tool_calls: None,
                tool_call_id: None,
            });
            messages.push(directive::ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("Understood. I will use this context.")),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        messages.push(directive::ProviderMessage {
            role: "user".to_string(),
            content: Some(json!(prompt)),
            tool_calls: None,
            tool_call_id: None,
        });

        // Context after: injected after user prompt
        if let Some(ref after) = bootstrap_output.config.context_after {
            messages.push(directive::ProviderMessage {
                role: "user".to_string(),
                content: Some(json!(after)),
                tool_calls: None,
                tool_call_id: None,
            });
            messages.push(directive::ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("Noted. I will apply this guidance.")),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        runner::Runner::new(
            messages,
            bootstrap_output.config.tools,
            bootstrap_output.config.system_prompt,
            harness,
            budget,
            callback,
            context_window,
            provider,
            model_name,
            envelope.thread_id.clone(),
            hooks,
        )
    };

    let result = runner_inst.run().await;

    let _ = crate::knowledge::write_thread_transcript(
        &project_root,
        &envelope.thread_id,
        &envelope.target.path,
        runner_inst.messages(),
    );
    let _ = crate::knowledge::write_capabilities(
        &project_root,
        &envelope.thread_id,
        runner_inst.tools(),
        None,
    );

    Ok(result)
}
