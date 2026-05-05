use std::io::Read;

use anyhow::{Context, Result};
use serde_json::json;

mod adapter;
mod bootstrap;
mod budget;
mod continuation;
mod directive;
mod dispatcher;
mod harness;
mod knowledge;
mod provider_adapter;
mod result_guard;
mod resume;
mod runner;

use ryeos_runtime::envelope::{LaunchEnvelope, RuntimeResult};

fn main() {
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_directive_runtime());

    let result = run_directive();
    let exit_code = match &result {
        Ok(_) => 0,
        Err(_) => 1,
    };

    match result {
        Ok(runtime_result) => {
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
        Err(err) => {
            // Bootstrap / I/O failures pre-runner. Surface the full error
            // chain on stderr — the daemon captures stderr into the
            // RuntimeResult fallback path (`launch.rs` !result.success
            // branch) and tracing alone never showed the underlying
            // `anyhow::Error`. Without this, P3b.4 / P3b.5 failures appear
            // as opaque "runtime exited non-zero" with only the early
            // tracing lines visible (oracle-flagged P3b.4 diagnostic gap).
            eprintln!("ryeos-directive-runtime fatal: {err:#}");
        }
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
                result: Some(json!(format!("invalid envelope: {}", e))),
                outputs: json!({}),
                cost: None,
                warnings: Vec::new(),
            });
        }
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_with_envelope(envelope))
}

async fn run_with_envelope(envelope: LaunchEnvelope) -> Result<RuntimeResult> {
    let project_root = envelope.roots.project_root.clone();
    let user_root = envelope.roots.user_root.clone();
    let system_roots = envelope.roots.system_roots.clone();

    // The runtime no longer parses the directive body or walks extends.
    // The daemon-side extends-chain composer (handler:rye/core/extends-chain)
    // has already produced
    // `envelope.resolution.composed = KindComposedView::ExtendsChain(...)`
    // — we hand that view straight into bootstrap.
    let verified_loader = ryeos_runtime::verified_loader::VerifiedLoader::new(
        envelope.roots.project_root.clone(),
        envelope.roots.user_root.clone(),
        envelope.roots.system_roots.clone(),
    );

    let thread_auth_token = std::env::var("RYEOSD_THREAD_AUTH_TOKEN")
        .expect("RYEOSD_THREAD_AUTH_TOKEN must be set by daemon");
    let callback = ryeos_runtime::callback_client::CallbackClient::new(
        &envelope.callback,
        &envelope.thread_id,
        envelope.roots.project_root.to_str().unwrap_or(""),
        &thread_auth_token,
    );

    let bootstrap_output = bootstrap::bootstrap(
        &project_root,
        user_root.as_deref(),
        &system_roots,
        &envelope.resolution.composed,
        &envelope.policy.hard_limits,
        &verified_loader,
        &envelope.inventory,
    )?;

    let provider = bootstrap_output.provider.clone();
    let model_name = bootstrap_output.model_name.clone();
    let context_window = bootstrap_output.context_window;
    let execution = bootstrap_output.config.execution.clone();
    let sampling = bootstrap_output.sampling.clone();

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

        // R5: Resume gate — refuse resume if the prior thread has no
        // settled `thread_usage` event in the replay stream. Without
        // prior budget data, the runtime cannot reseed BudgetTracker
        // or Harness, so resuming would silently start from zero.
        if !resume_state.has_thread_usage_event {
            return Ok(RuntimeResult {
                success: false,
                status: "errored".to_string(),
                thread_id: envelope.thread_id.clone(),
                result: Some(json!(
                    "resume prerequisites unmet: no thread_usage event found in prior thread"
                )),
                outputs: json!({}),
                cost: None,
                warnings: Vec::new(),
            });
        }

        if let Err(e) = callback
            .append_event("thread_continued", json!({"previous_thread_id": resume_id}))
            .await
        {
            tracing::warn!(
                thread_id = %envelope.thread_id,
                error = %e,
                "callback append_event(thread_continued) failed"
            );
        }
        runner::Runner::from_resume(
            resume_state,
            runner::RunnerConfig {
                messages: vec![], // overridden by from_resume with resume.messages
                tools: bootstrap_output.config.tools,
                system_prompt: bootstrap_output.config.system_prompt,
                harness,
                budget,
                callback,
                context_window,
                provider_config: provider,
                execution,
                model_name,
                thread_id: envelope.thread_id.clone(),
                hooks,
                outputs: bootstrap_output.config.outputs,
                sampling,
            },
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

        let prompt = if inputs.is_object() && !inputs.as_object().is_none_or(|o| o.is_empty()) {
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

        runner::Runner::new(runner::RunnerConfig {
            messages,
            tools: bootstrap_output.config.tools,
            system_prompt: bootstrap_output.config.system_prompt,
            harness,
            budget,
            callback,
            context_window,
            provider_config: provider,
            execution,
            model_name,
            thread_id: envelope.thread_id.clone(),
            hooks,
            outputs: bootstrap_output.config.outputs,
            sampling,
        })
    };

    let result = runner_inst.run().await;

    // Persistence-first: the markdown transcript and capabilities
    // manifest are part of the durable output of a directive run. A
    // silent `let _ = …` swallowed I/O failure leaves the daemon
    // believing a thread completed successfully while the on-disk
    // transcript is missing — exactly the silent-fallback class of
    // bug remediation R4/R7 closed elsewhere. Surface any transcript
    // write failure as a non-success RuntimeResult; the daemon then
    // routes it through the same finalize-as-failed path as any
    // other terminal error.
    if let Err(e) = crate::knowledge::write_thread_transcript(
        &project_root,
        &envelope.thread_id,
        &envelope.resolution.root.source_path.to_string_lossy(),
        runner_inst.messages(),
    ) {
        return Ok(RuntimeResult {
            success: false,
            status: "errored".to_string(),
            thread_id: envelope.thread_id.clone(),
            result: Some(json!(format!("transcript write failed: {e:#}"))),
            outputs: json!({}),
            cost: result.cost.clone(),
            warnings: result.warnings.clone(),
        });
    }
    if let Err(e) = crate::knowledge::write_capabilities(
        &project_root,
        &envelope.thread_id,
        runner_inst.tools(),
        None,
    ) {
        return Ok(RuntimeResult {
            success: false,
            status: "errored".to_string(),
            thread_id: envelope.thread_id.clone(),
            result: Some(json!(format!("capabilities write failed: {e:#}"))),
            outputs: json!({}),
            cost: result.cost.clone(),
            warnings: result.warnings.clone(),
        });
    }

    Ok(result)
}
