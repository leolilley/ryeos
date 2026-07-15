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
use ryeos_runtime::provider_snapshot::ResolvedProviderSnapshot;

/// Render the stimulus that opens a run: interpolate the directive body with the
/// envelope inputs, then append any inputs the body did not itself reference.
/// Shared by the fresh-launch and chained-resume paths so both produce the same
/// stimulus.
fn render_stimulus(prompt_template: &str, inputs: &serde_json::Value) -> Result<String> {
    let interpolated_prompt = if inputs.is_null() {
        prompt_template.to_string()
    } else {
        let context = serde_json::json!({ "inputs": inputs });
        match ryeos_runtime::interpolate(&serde_json::json!(prompt_template), &context)? {
            serde_json::Value::String(rendered) => rendered,
            other => anyhow::bail!("directive body interpolated to a non-string value: {other}"),
        }
    };

    // Surface only the inputs the template did NOT already place via a
    // `{input:KEY}` / `${inputs.KEY}` reference.
    let prompt = match inputs.as_object() {
        Some(obj) if !obj.is_empty() => {
            let referenced = ryeos_runtime::referenced_input_keys(prompt_template);
            let leftover: serde_json::Map<String, serde_json::Value> = obj
                .iter()
                .filter(|(key, _)| !referenced.contains(key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            if leftover.is_empty() {
                interpolated_prompt
            } else {
                format!(
                    "{}\n\nInputs:\n{}",
                    interpolated_prompt,
                    serde_json::to_string_pretty(&serde_json::Value::Object(leftover))?
                )
            }
        }
        _ => interpolated_prompt,
    };
    Ok(prompt)
}

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
    // Callback identity + state-write anchor: the deliberate `state_root`
    // override when the launch carried one, otherwise the project root. The
    // daemon minted this run's callback token against exactly this path, so
    // every callback must advertise it; resolution stays on `project_root`.
    let state_root = envelope.roots.state_root().to_path_buf();
    let bundle_roots = envelope.roots.bundle_roots.clone();

    // The runtime no longer parses the directive body or walks extends.
    // The daemon-side extends-chain composer (handler:ryeos/core/extends-chain)
    // has already produced
    // `envelope.resolution.composed = KindComposedView::ExtendsChain(...)`
    // — we hand that view straight into bootstrap.
    let verified_loader = ryeos_runtime::verified_loader::VerifiedLoader::new(
        envelope.roots.project_root.clone(),
        envelope.roots.bundle_roots.clone(),
        &envelope.roots.node_trusted_keys_dir,
    );

    let thread_auth_token = std::env::var("RYEOSD_THREAD_AUTH_TOKEN")
        .expect("RYEOSD_THREAD_AUTH_TOKEN must be set by daemon");
    let callback = ryeos_runtime::callback_client::CallbackClient::new(
        &envelope.callback,
        &envelope.thread_id,
        state_root.to_str().unwrap_or(""),
        &thread_auth_token,
    );

    // Register this process's pgid BEFORE any durable callback — the resume
    // replay read, `append_event(thread_continued)`, and the opening
    // `emit_stimulus` below all happen after this. Without it the daemon
    // cannot tell a live runtime from a crashed one on restart and would
    // resume a duplicate. Resume-critical: must precede all work.
    callback.attach_current_process().await?;

    let provider_snapshot: ResolvedProviderSnapshot =
        serde_json::from_value(envelope.provider_snapshot.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "launch envelope missing provider_snapshot — the daemon must \
                 embed the resolved provider config in the envelope"
            )
        })?)
        .map_err(|e| {
            anyhow::anyhow!("failed to deserialize provider_snapshot from envelope: {e}")
        })?;

    let bootstrap_output = bootstrap::bootstrap(
        &bootstrap::BootstrapRoots {
            project_root: &project_root,
            bundle_roots: &bundle_roots,
        },
        &envelope.resolution.composed,
        &envelope.policy.hard_limits,
        &verified_loader,
        &envelope.inventory,
        &provider_snapshot,
    )?;

    let provider = bootstrap_output.provider.clone();
    let provider_id = bootstrap_output.provider_id.clone();
    let model_name = bootstrap_output.model_name.clone();
    let context_window = bootstrap_output.context_window;
    let execution = bootstrap_output.config.execution.clone();
    let sampling = bootstrap_output.sampling.clone();
    let matched_profile = provider_snapshot.matched_profile.clone();
    let config_hash = provider_snapshot.config_hash.clone();

    let harness = harness::Harness::new(
        &envelope.policy,
        envelope.request.depth,
        bootstrap_output.config.risk_policy.clone(),
    );

    // Wire SIGTERM → harness cancelled flag so runner can exit cleanly
    {
        let cancelled = harness.cancelled_flag();
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("failed to install SIGTERM handler")?;
        tokio::spawn(async move {
            sigterm.recv().await;
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::info!("received SIGTERM, cancellation requested");
        });
    }

    // Wire SIGUSR1 → harness interrupt flag (live intervention). This MUST set
    // the flag at signal-DELIVERY time, not at async-task-poll time: the runner
    // clears any stale interrupt at the turn boundary before streaming, so a
    // signal delivered between turns has to be visible to that boundary clear.
    // A tokio::signal task sets the flag only when the task is next polled, which
    // can land AFTER the boundary clear — the flag would then cut the fresh
    // cognition (stale-interrupt race). `signal_hook::flag::register` installs a
    // synchronous handler that stores `true` into the shared atomic the instant
    // the signal arrives (an atomic store is async-signal-safe), closing that
    // race. It coexists with tokio's SIGTERM handler via signal-hook-registry and
    // stays armed for the process lifetime (repeatable). SIGTERM keeps its async
    // task: a late cancel only finalizes later, it never cuts-then-continues.
    signal_hook::flag::register(signal_hook::consts::SIGUSR1, harness.interrupted_flag())
        .context("failed to register SIGUSR1 live-interrupt flag")?;
    let budget = budget::BudgetTracker::new(envelope.policy.hard_limits.spend_usd);

    let hooks = bootstrap_output.config.hooks.clone();

    // The opening stimulus is rendered only where it is actually injected (fresh
    // launch, or operator follow-up). A MACHINE continuation suppresses it
    // entirely and never renders — a changed/broken prompt template must not be
    // able to abort a cut-off task that asks for nothing new.
    let mut runner_inst = if let Some(ref resume_id) = envelope.request.previous_thread_id {
        let carry_turns = bootstrap_output
            .config
            .continuation_runtime
            .resolve_carry_turns(bootstrap_output.config.continuation.declared_carry_turns());
        let mut resume_state = resume::load_resume_state(&callback, resume_id, carry_turns).await?;

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

        // Operator follow-up: emit the new stimulus as a `cognition_in` AFTER
        // folding the chain (so the replay does not double-count it), then seed
        // it as the live provider message this run answers. `role` here is the
        // provider-wire mapping, not a substrate concept.
        //
        // A MACHINE continuation (limit cut-off) suppresses this: it folds the
        // chain and resumes the cut-off task with nothing new asked — `inputs`
        // are the source's originals, already present in the folded chain, so
        // re-injecting them would double the opening stimulus.
        if !envelope.request.suppress_stimulus {
            let rendered_prompt = render_stimulus(
                &bootstrap_output.config.user_prompt,
                &envelope.request.inputs,
            )?;
            callback.emit_stimulus(&rendered_prompt).await?;
            resume_state.messages.push(directive::ProviderMessage {
                role: "user".to_string(),
                content: Some(json!(rendered_prompt)),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
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
                provider_id,
                matched_profile,
                config_hash,
                execution,
                model_name,
                thread_id: envelope.thread_id.clone(),
                hooks,
                outputs: bootstrap_output.config.outputs,
                return_nudge: bootstrap_output.config.return_nudge,
                continuation: bootstrap_output.config.continuation,
                context_threshold_ratio: bootstrap_output
                    .config
                    .continuation_runtime
                    .context_threshold_ratio,
                sampling,
            },
        )
    } else {
        // Fresh launch: render and emit the opening stimulus as a `cognition_in`
        // so a later turn can fold it from the chain.
        let rendered_prompt = render_stimulus(
            &bootstrap_output.config.user_prompt,
            &envelope.request.inputs,
        )?;
        callback.emit_stimulus(&rendered_prompt).await?;

        let mut messages = Vec::new();

        // Context before: injected before user prompt
        if let Some(ref before) = bootstrap_output.config.context_before {
            messages.push(directive::ProviderMessage {
                role: "user".to_string(),
                content: Some(json!(before)),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
            messages.push(directive::ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("Understood. I will use this context.")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }

        messages.push(directive::ProviderMessage {
            role: "user".to_string(),
            content: Some(json!(rendered_prompt)),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        });

        // Context after: injected after user prompt
        if let Some(ref after) = bootstrap_output.config.context_after {
            messages.push(directive::ProviderMessage {
                role: "user".to_string(),
                content: Some(json!(after)),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
            messages.push(directive::ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("Noted. I will apply this guidance.")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
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
            provider_id,
            matched_profile,
            config_hash,
            execution,
            model_name,
            thread_id: envelope.thread_id.clone(),
            hooks,
            outputs: bootstrap_output.config.outputs,
            return_nudge: bootstrap_output.config.return_nudge,
            continuation: bootstrap_output.config.continuation,
            context_threshold_ratio: bootstrap_output
                .config
                .continuation_runtime
                .context_threshold_ratio,
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
        &state_root,
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
        &state_root,
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
